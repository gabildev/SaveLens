//! SQLite databases used as save files.
//!
//! # Model
//!
//! The document keeps the **database file bytes** in memory; they are the
//! source of truth. The semantic tree is a read-only projection:
//!
//! ```text
//! Map  table name -> Map  row key -> Map  column name -> value
//! ```
//!
//! for every ordinary user table (internal `sqlite_%` tables, views, virtual
//! tables and their shadow tables are not projected). Row keys are the rowid
//! as a decimal string for rowid tables (including tables with an
//! `INTEGER PRIMARY KEY` alias) and the primary-key values joined with `|`
//! for `WITHOUT ROWID` tables (`|` and `\` inside text values are escaped as
//! `\|` and `\\`). Rows are ordered by rowid / primary key. Storage classes
//! map to `I64` (INTEGER), `F64` (REAL), `String` (TEXT), `Bytes` (BLOB) and
//! `Null`.
//!
//! # Serialisation
//!
//! For SQLite the file *is* the serialised form: the B-tree pages, free
//! lists, page size, text encoding and journal mode cannot be regenerated
//! from rows without a lossy `VACUUM`. [`SqliteDocument::encode`] therefore
//! returns the in-memory database bytes, which is the real serializer here:
//! an unedited document yields the original file exactly, and the database is
//! never written to while projecting (decoding opens a private copy with
//! `SQLITE_OPEN_READ_ONLY` and `immutable=1`).
//!
//! # Edits
//!
//! [`SqliteDocument::apply_ast`] diffs the current projection against the
//! edited tree and replays the changes with SQL on a private read-write copy,
//! in a single transaction: `UPDATE` for changed cells, `INSERT` for added
//! rows and `DELETE` for removed rows (always with bound parameters and quoted
//! identifiers). The schema is never altered: adding or removing tables or
//! columns, or changing a primary-key value through a cell edit, is rejected
//! with [`CodecError::StructureUnsupported`]. WAL databases are checkpointed
//! (`wal_checkpoint(TRUNCATE)`) before the copy is closed, then the main file
//! is read back. Page size, text encoding and journal mode are preserved (no
//! `VACUUM`).
//!
//! # Temporary files
//!
//! Every copy lives in a fresh [`tempfile::TempDir`] inside the system
//! temporary directory, never next to the game's save, and is deleted when
//! the operation finishes (success or error).

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use rusqlite::types::{Value, ValueRef};
use rusqlite::{params_from_iter, Connection, OpenFlags};
use savelens_core::{diff, ChangeKind, Node, NodeMap, NodePath, PathSeg};

use crate::document::{Capabilities, Confidence, DetectContext, Document, StructuralCodec};
use crate::error::{CodecError, CodecResult};

const CODEC: &str = "sqlite";
const MAGIC: &[u8; 16] = b"SQLite format 3\0";
const HEADER_LEN: usize = 100;
const DB_FILE_NAME: &str = "save.sqlite";
const SCRATCH_LOG_CAP: usize = 256;

thread_local! {
    static SCRATCH_LOG: RefCell<VecDeque<PathBuf>> = const { RefCell::new(VecDeque::new()) };
}

/// Drains the paths of the scratch directories the codec created on the
/// current thread (most recent last, at most 256 kept).
///
/// Diagnostics helper: tests use it to prove that every temporary copy of a
/// database was removed once the operation finished.
pub fn take_recent_scratch_dirs() -> Vec<PathBuf> {
    SCRATCH_LOG.with(|log| log.borrow_mut().drain(..).collect())
}

/// A private temporary directory holding one copy of the database.
struct Scratch {
    dir: tempfile::TempDir,
}

impl Scratch {
    /// Creates a fresh directory in the system temp dir and writes `bytes`
    /// into it.
    fn with_bytes(bytes: &[u8]) -> CodecResult<(Scratch, PathBuf)> {
        let dir = tempfile::Builder::new().prefix("savelens-sqlite-").tempdir()?;
        let path = dir.path().to_path_buf();
        SCRATCH_LOG.with(|log| {
            let mut log = log.borrow_mut();
            if log.len() >= SCRATCH_LOG_CAP {
                log.pop_front();
            }
            log.push_back(path);
        });
        let scratch = Scratch { dir };
        let file = scratch.dir.path().join(DB_FILE_NAME);
        std::fs::write(&file, bytes)?;
        Ok((scratch, file))
    }
}

/// Parsed fixed header fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    page_size: u32,
    wal: bool,
    encoding: u32,
}

fn parse_header(bytes: &[u8]) -> CodecResult<Header> {
    if bytes.len() < HEADER_LEN {
        return Err(CodecError::Truncated {
            codec: CODEC,
            context: "database header".into(),
            offset: bytes.len(),
            needed: HEADER_LEN - bytes.len(),
        });
    }
    if &bytes[..16] != MAGIC {
        return Err(CodecError::malformed(CODEC, 0, "missing `SQLite format 3` magic"));
    }
    let raw = u16::from_be_bytes([bytes[16], bytes[17]]);
    let page_size = match raw {
        1 => 65_536,
        n if (512..=32_768).contains(&n) && n.is_power_of_two() => u32::from(n),
        n => return Err(CodecError::malformed(CODEC, 16, format!("invalid page size {n}"))),
    };
    let encoding = u32::from_be_bytes([bytes[56], bytes[57], bytes[58], bytes[59]]);
    Ok(Header { page_size, wal: bytes[18] == 2 || bytes[19] == 2, encoding })
}

fn encoding_name(code: u32) -> &'static str {
    match code {
        1 => "UTF-8",
        2 => "UTF-16le",
        3 => "UTF-16be",
        _ => "UTF-8 (unset)",
    }
}

/// Quotes an SQL identifier (`"name"`, inner quotes doubled).
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Builds an `immutable=1` URI for `path`, percent-encoding everything that
/// is not unreserved.
fn immutable_uri(path: &Path) -> String {
    let mut out = String::from("file:");
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out.push_str("?immutable=1");
    out
}

/// Opens a private copy read-only (no locks, no WAL/SHM files).
fn open_read_only(path: &Path) -> CodecResult<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    Ok(Connection::open_with_flags(immutable_uri(path), flags)?)
}

#[derive(Debug, Clone)]
struct Column {
    name: String,
    /// 1-based position in the primary key, 0 when not part of it.
    pk: u32,
}

/// How a row is addressed in SQL.
#[derive(Debug, Clone, PartialEq)]
enum RowId {
    Rowid(i64),
    Pk(Vec<Value>),
}

#[derive(Debug, Clone)]
struct Table {
    name: String,
    without_rowid: bool,
    columns: Vec<Column>,
    /// Column indexes of the primary key, in key order.
    pk: Vec<usize>,
    /// Name used to address the rowid (`rowid`, `_rowid_` or `oid`).
    rowid_name: Option<&'static str>,
    /// Index of the `INTEGER PRIMARY KEY` column aliasing the rowid.
    rowid_alias: Option<usize>,
    /// Row key -> SQL address.
    rows: IndexMap<String, RowId>,
}

impl Table {
    fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    fn where_clause(&self) -> String {
        match self.rowid_name {
            Some(r) if !self.without_rowid => format!("{r} = ?"),
            _ => self
                .pk
                .iter()
                .map(|&i| format!("{} = ?", quote_ident(&self.columns[i].name)))
                .collect::<Vec<_>>()
                .join(" AND "),
        }
    }

    fn where_params(id: &RowId) -> Vec<Value> {
        match id {
            RowId::Rowid(r) => vec![Value::Integer(*r)],
            RowId::Pk(v) => v.clone(),
        }
    }
}

fn value_to_node(v: ValueRef<'_>) -> Node {
    match v {
        ValueRef::Null => Node::Null,
        ValueRef::Integer(i) => Node::I64(i),
        ValueRef::Real(f) => Node::F64(f),
        ValueRef::Text(t) => Node::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Node::Bytes(b.to_vec()),
    }
}

/// Converts an edited node into an SQLite value.
fn node_to_value(path: &NodePath, node: &Node) -> CodecResult<Value> {
    Ok(match node {
        Node::Null => Value::Null,
        Node::Bool(b) => Value::Integer(i64::from(*b)),
        Node::I8(v) => Value::Integer(i64::from(*v)),
        Node::I16(v) => Value::Integer(i64::from(*v)),
        Node::I32(v) => Value::Integer(i64::from(*v)),
        Node::I64(v) => Value::Integer(*v),
        Node::U8(v) => Value::Integer(i64::from(*v)),
        Node::U16(v) => Value::Integer(i64::from(*v)),
        Node::U32(v) => Value::Integer(i64::from(*v)),
        Node::U64(v) => Value::Integer(i64::try_from(*v).map_err(|_| {
            CodecError::unencodable(CODEC, path, format!("{v} does not fit a 64-bit signed SQLite INTEGER"))
        })?),
        Node::F32(v) => Value::Real(f64::from(*v)),
        Node::F64(v) => Value::Real(*v),
        Node::String(s) => Value::Text(s.clone()),
        Node::Bytes(b) => Value::Blob(b.clone()),
        Node::RawRegion(r) => Value::Blob(r.bytes.clone()),
        Node::Array(_) | Node::Map(_) => {
            return Err(CodecError::type_mismatch(CODEC, path, "SQLite scalar", node.kind().name()));
        }
    })
}

/// Text form of one primary-key component of a row key.
fn key_part(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => format!("{f:?}"),
        Value::Text(t) => {
            let mut s = String::with_capacity(t.len());
            for c in t.chars() {
                if c == '|' || c == '\\' {
                    s.push('\\');
                }
                s.push(c);
            }
            s
        }
        Value::Blob(b) => format!("x'{}'", hex::encode_upper(b)),
    }
}

fn pk_key(values: &[Value]) -> String {
    values.iter().map(key_part).collect::<Vec<_>>().join("|")
}

fn owned(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Integer(i),
        ValueRef::Real(f) => Value::Real(f),
        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Blob(b.to_vec()),
    }
}

/// Everything read from one database image.
#[derive(Debug, Clone)]
struct Projection {
    header: Header,
    tables: Vec<Table>,
    ast: Node,
    rows: usize,
    skipped: Vec<String>,
}

/// Ordinary user tables (`name`, `WITHOUT ROWID` flag) and the names of
/// tables that are not projected.
struct TableList {
    tables: Vec<(String, bool)>,
    skipped: Vec<String>,
}

fn load_tables(conn: &Connection) -> CodecResult<TableList> {
    // `PRAGMA table_list` tells ordinary tables from virtual / shadow ones
    // and flags WITHOUT ROWID tables.
    let mut kinds: HashMap<String, (String, bool)> = HashMap::new();
    {
        let mut st = conn.prepare("PRAGMA main.table_list")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let schema: String = r.get("schema")?;
            if schema != "main" {
                continue;
            }
            let name: String = r.get("name")?;
            let ty: String = r.get("type")?;
            let wr: i64 = r.get("wr")?;
            kinds.insert(name, (ty, wr != 0));
        }
    }
    let mut st = conn.prepare("SELECT name FROM main.sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY rowid")?;
    let names: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    let mut tables = Vec::new();
    let mut skipped = Vec::new();
    for name in names {
        match kinds.get(&name) {
            Some((ty, wr)) if ty == "table" => tables.push((name, *wr)),
            Some((ty, _)) => skipped.push(format!("{name} ({ty})")),
            None => skipped.push(format!("{name} (unlisted)")),
        }
    }
    Ok(TableList { tables, skipped })
}

fn load_table(conn: &Connection, name: &str, without_rowid: bool) -> CodecResult<(Table, NodeMap)> {
    let mut st = conn.prepare("SELECT name, pk FROM pragma_table_info(?1, 'main') ORDER BY cid")?;
    let columns: Vec<Column> =
        st.query_map([name], |r| Ok(Column { name: r.get(0)?, pk: r.get(1)? }))?.collect::<Result<_, _>>()?;
    if columns.is_empty() {
        return Err(CodecError::malformed(CODEC, 0, format!("table `{name}` has no readable columns")));
    }
    let mut pk: Vec<usize> = (0..columns.len()).filter(|&i| columns[i].pk > 0).collect();
    pk.sort_by_key(|&i| columns[i].pk);

    let rowid_name = if without_rowid {
        None
    } else {
        let lower: Vec<String> = columns.iter().map(|c| c.name.to_ascii_lowercase()).collect();
        let pick = ["rowid", "_rowid_", "oid"].into_iter().find(|n| !lower.iter().any(|c| c == n));
        match pick {
            Some(p) => Some(p),
            None => {
                return Err(CodecError::unsupported(
                    CODEC,
                    format!("table `{name}` shadows every rowid alias (rowid, _rowid_, oid) with a column"),
                ))
            }
        }
    };
    if without_rowid && pk.is_empty() {
        return Err(CodecError::malformed(CODEC, 0, format!("WITHOUT ROWID table `{name}` has no primary key")));
    }
    // A single-column primary key of a rowid table aliases the rowid unless
    // SQLite had to build an automatic index for it.
    let rowid_alias = if !without_rowid && pk.len() == 1 {
        let mut st = conn.prepare("SELECT count(*) FROM pragma_index_list(?1, 'main') WHERE origin = 'pk'")?;
        let n: i64 = st.query_row([name], |r| r.get(0))?;
        (n == 0).then_some(pk[0])
    } else {
        None
    };

    let cols_sql = columns.iter().map(|c| quote_ident(&c.name)).collect::<Vec<_>>().join(", ");
    let sql = match rowid_name {
        Some(r) => format!("SELECT {r}, {cols_sql} FROM main.{} ORDER BY {r}", quote_ident(name)),
        None => {
            let order = pk.iter().map(|&i| quote_ident(&columns[i].name)).collect::<Vec<_>>().join(", ");
            format!("SELECT {cols_sql} FROM main.{} ORDER BY {order}", quote_ident(name))
        }
    };
    let offset = usize::from(rowid_name.is_some());
    let mut st = conn.prepare(&sql)?;
    let mut q = st.query([])?;
    let mut map = NodeMap::new();
    let mut rows = IndexMap::new();
    while let Some(r) = q.next()? {
        let mut row = NodeMap::new();
        for (i, c) in columns.iter().enumerate() {
            row.insert(c.name.clone(), value_to_node(r.get_ref(i + offset)?));
        }
        let (key, id) = match rowid_name {
            Some(_) => {
                let rid: i64 = r.get(0)?;
                (rid.to_string(), RowId::Rowid(rid))
            }
            None => {
                let vals: Vec<Value> = pk.iter().map(|&i| r.get_ref(i).map(owned)).collect::<Result<_, _>>()?;
                (pk_key(&vals), RowId::Pk(vals))
            }
        };
        if rows.insert(key.clone(), id).is_some() {
            return Err(CodecError::unsupported(
                CODEC,
                format!("table `{name}`: two rows map to the same row key `{key}` (primary-key values with identical text forms)"),
            ));
        }
        map.insert(key, Node::Map(row));
    }
    let table = Table { name: name.to_string(), without_rowid, columns, pk, rowid_name, rowid_alias, rows };
    Ok((table, map))
}

/// Checks that the image holds whole pages and matches the page count
/// recorded in the header (when that field is valid), so a truncated file
/// is never silently read with zero-filled pages.
fn check_size(bytes: &[u8], header: &Header) -> CodecResult<()> {
    let page = header.page_size as usize;
    if !bytes.len().is_multiple_of(page) {
        let pages = bytes.len() / page;
        return Err(CodecError::Truncated {
            codec: CODEC,
            context: format!("page {}", pages + 1),
            offset: bytes.len(),
            needed: page - bytes.len() % page,
        });
    }
    let be = |o: usize| u32::from_be_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
    let (change_counter, declared, valid_for) = (be(24), be(28), be(92));
    let actual = bytes.len() / page;
    if declared != 0 && change_counter == valid_for && declared as usize != actual {
        return Err(CodecError::malformed(
            CODEC,
            28,
            format!("header declares {declared} page(s) but the file holds {actual}"),
        ));
    }
    Ok(())
}

impl Projection {
    /// Projects a database image (read-only private copy).
    fn load(bytes: &[u8]) -> CodecResult<Projection> {
        let header = parse_header(bytes)?;
        check_size(bytes, &header)?;
        let (_scratch, path) = Scratch::with_bytes(bytes)?;
        let conn = open_read_only(&path)?;
        let check: String = conn.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
        if check != "ok" {
            return Err(CodecError::malformed(CODEC, 0, format!("database failed quick_check: {check}")));
        }
        let TableList { tables: names, skipped } = load_tables(&conn)?;
        let mut root = NodeMap::new();
        let mut tables = Vec::new();
        let mut rows = 0;
        for (name, wr) in names {
            let (table, map) = load_table(&conn, &name, wr)?;
            rows += map.len();
            root.insert(name, Node::Map(map));
            tables.push(table);
        }
        drop(conn);
        Ok(Projection { header, tables, ast: Node::Map(root), rows, skipped })
    }
}

/// One SQL statement to replay.
#[derive(Debug)]
struct Stmt {
    sql: String,
    params: Vec<Value>,
    /// Number of rows the statement must affect.
    expect: usize,
    path: NodePath,
}

/// SQLite database document. See the module documentation.
#[derive(Debug, Clone)]
pub struct SqliteDocument {
    bytes: Vec<u8>,
    proj: Projection,
}

impl SqliteDocument {
    /// Parses a database image.
    pub fn parse(bytes: &[u8]) -> CodecResult<SqliteDocument> {
        let proj = Projection::load(bytes)?;
        Ok(SqliteDocument { bytes: bytes.to_vec(), proj })
    }

    /// Current database file bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn table(&self, name: &str) -> Option<&Table> {
        self.proj.tables.iter().find(|t| t.name == name)
    }

    /// Translates the semantic diff into SQL statements.
    fn plan(&self, edited: &Node) -> CodecResult<Vec<Stmt>> {
        let current = &self.proj.ast;
        if !matches!(edited, Node::Map(_)) {
            return Err(CodecError::structure(CODEC, "$", "the root must stay a map of tables"));
        }
        let d = diff(current, edited);
        let mut deletes = Vec::new();
        let mut updates = Vec::new();
        let mut inserts = Vec::new();
        for c in &d.changes {
            let segs = c.path.segments();
            let keys: Vec<&str> = segs.iter().filter_map(PathSeg::as_key).collect();
            if keys.len() != segs.len() {
                return Err(CodecError::structure(CODEC, &c.path, "SQLite projections only contain maps"));
            }
            match (keys.as_slice(), &c.kind) {
                ([], _) => return Err(CodecError::structure(CODEC, &c.path, "the root must stay a map of tables")),
                ([_], ChangeKind::Added { .. }) => {
                    return Err(CodecError::structure(
                        CODEC,
                        &c.path,
                        "adding tables is not supported (the schema is never altered)",
                    ))
                }
                ([_], ChangeKind::Removed { .. }) => {
                    return Err(CodecError::structure(
                        CODEC,
                        &c.path,
                        "removing tables is not supported (the schema is never altered)",
                    ))
                }
                ([_], _) => return Err(CodecError::structure(CODEC, &c.path, "a table must stay a map of rows")),
                ([t, r], ChangeKind::Added { new }) => inserts.push(self.plan_insert(&c.path, t, r, new)?),
                ([t, r], ChangeKind::Removed { .. }) => deletes.push(self.plan_delete(&c.path, t, r)?),
                ([_, _], _) => return Err(CodecError::structure(CODEC, &c.path, "a row must stay a map of columns")),
                ([_, _, _], ChangeKind::Added { .. }) => {
                    return Err(CodecError::structure(
                        CODEC,
                        &c.path,
                        "adding columns is not supported (the schema is never altered)",
                    ))
                }
                ([_, _, _], ChangeKind::Removed { .. }) => {
                    return Err(CodecError::structure(
                        CODEC,
                        &c.path,
                        "removing columns is not supported (the schema is never altered)",
                    ))
                }
                ([t, r, col], _) => {
                    let new = edited.get(&c.path).ok_or_else(|| CodecError::path_not_found(CODEC, &c.path))?;
                    updates.push(self.plan_update(&c.path, t, r, col, new)?);
                }
                _ => return Err(CodecError::structure(CODEC, &c.path, "SQLite cells are scalars")),
            }
        }
        deletes.extend(updates);
        deletes.extend(inserts);
        Ok(deletes)
    }

    fn known_table(&self, path: &NodePath, name: &str) -> CodecResult<&Table> {
        self.table(name).ok_or_else(|| CodecError::path_not_found(CODEC, path))
    }

    fn plan_delete(&self, path: &NodePath, t: &str, r: &str) -> CodecResult<Stmt> {
        let table = self.known_table(path, t)?;
        let id = table.rows.get(r).ok_or_else(|| CodecError::path_not_found(CODEC, path))?;
        Ok(Stmt {
            sql: format!("DELETE FROM main.{} WHERE {}", quote_ident(&table.name), table.where_clause()),
            params: Table::where_params(id),
            expect: 1,
            path: path.clone(),
        })
    }

    fn plan_update(&self, path: &NodePath, t: &str, r: &str, col: &str, new: &Node) -> CodecResult<Stmt> {
        let table = self.known_table(path, t)?;
        let id = table.rows.get(r).ok_or_else(|| CodecError::path_not_found(CODEC, path))?;
        let ci = table.column(col).ok_or_else(|| CodecError::path_not_found(CODEC, path))?;
        if table.columns[ci].pk > 0 {
            return Err(CodecError::structure(
                CODEC,
                path,
                "primary-key values cannot be changed through a cell edit (delete the row and insert a new one)",
            ));
        }
        let mut params = vec![node_to_value(path, new)?];
        params.extend(Table::where_params(id));
        Ok(Stmt {
            sql: format!(
                "UPDATE main.{} SET {} = ? WHERE {}",
                quote_ident(&table.name),
                quote_ident(col),
                table.where_clause()
            ),
            params,
            expect: 1,
            path: path.clone(),
        })
    }

    fn plan_insert(&self, path: &NodePath, t: &str, r: &str, new: &Node) -> CodecResult<Stmt> {
        let table = self.known_table(path, t)?;
        let Node::Map(row) = new else {
            return Err(CodecError::type_mismatch(CODEC, path, "map (row)", new.kind().name()));
        };
        let mut cols: Vec<String> = Vec::new();
        let mut params: Vec<Value> = Vec::new();
        for (name, v) in row {
            let cp = path.key(name.clone());
            let ci = table.column(name).ok_or_else(|| {
                CodecError::structure(
                    CODEC,
                    &cp,
                    format!("table `{}` has no column `{name}` (the schema is never altered)", table.name),
                )
            })?;
            let val = node_to_value(&cp, v)?;
            if Some(ci) == table.rowid_alias {
                // The rowid comes from the row key below.
                match val {
                    Value::Null => continue,
                    Value::Integer(i) if i.to_string() == r => continue,
                    _ => {
                        return Err(CodecError::structure(
                            CODEC,
                            &cp,
                            format!("INTEGER PRIMARY KEY value must equal the row key `{r}`"),
                        ))
                    }
                }
            }
            cols.push(quote_ident(name));
            params.push(val);
        }
        match table.rowid_name {
            Some(rowid) if !table.without_rowid => {
                let rid: i64 = r.parse().ok().filter(|v: &i64| v.to_string() == r).ok_or_else(|| {
                    CodecError::structure(CODEC, path, "row keys of rowid tables must be canonical decimal integers")
                })?;
                cols.insert(0, rowid.to_string());
                params.insert(0, Value::Integer(rid));
            }
            _ => {
                let mut pk_vals = Vec::with_capacity(table.pk.len());
                for &i in &table.pk {
                    let name = &table.columns[i].name;
                    let v = row.get(name).ok_or_else(|| {
                        CodecError::structure(CODEC, path, format!("new row must provide primary-key column `{name}`"))
                    })?;
                    pk_vals.push(node_to_value(&path.key(name.clone()), v)?);
                }
                let expected = pk_key(&pk_vals);
                if expected != r {
                    return Err(CodecError::structure(
                        CODEC,
                        path,
                        format!("row key must match the primary-key values (expected `{expected}`)"),
                    ));
                }
            }
        }
        let sql = if cols.is_empty() {
            format!("INSERT INTO main.{} DEFAULT VALUES", quote_ident(&table.name))
        } else {
            let marks = vec!["?"; cols.len()].join(", ");
            format!("INSERT INTO main.{} ({}) VALUES ({marks})", quote_ident(&table.name), cols.join(", "))
        };
        Ok(Stmt { sql, params, expect: 1, path: path.clone() })
    }

    /// Runs the statements on a private read-write copy and returns the new
    /// main database file.
    fn execute(&self, stmts: &[Stmt]) -> CodecResult<Vec<u8>> {
        let (_scratch, path) = Scratch::with_bytes(&self.bytes)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let mut conn = Connection::open_with_flags(&path, flags)?;
        // Keep SQLite's own temporary structures in memory.
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        {
            let tx = conn.transaction()?;
            for s in stmts {
                let n = tx.execute(&s.sql, params_from_iter(s.params.iter()))?;
                if n != s.expect {
                    return Err(CodecError::malformed(
                        CODEC,
                        0,
                        format!("edit at `{}` affected {n} row(s) instead of {}", s.path, s.expect),
                    ));
                }
            }
            tx.commit()?;
        }
        if self.proj.header.wal {
            let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
            if busy != 0 {
                return Err(CodecError::malformed(CODEC, 0, "WAL checkpoint could not complete"));
            }
        }
        conn.close().map_err(|(_, e)| e)?;
        Ok(std::fs::read(&path)?)
    }
}

impl Document for SqliteDocument {
    fn codec_id(&self) -> &'static str {
        CODEC
    }

    fn to_ast(&self) -> Node {
        self.proj.ast.clone()
    }

    fn apply_ast(&mut self, root: &Node) -> CodecResult<()> {
        let stmts = self.plan(root)?;
        if stmts.is_empty() {
            return Ok(());
        }
        let bytes = self.execute(&stmts)?;
        let proj = Projection::load(&bytes)?;
        self.bytes = bytes;
        self.proj = proj;
        Ok(())
    }

    /// Returns the database file. The file is the serialised form of an
    /// SQLite database: unedited documents return the original bytes, edited
    /// ones the main file produced by the SQL transaction.
    fn encode(&self) -> CodecResult<Vec<u8>> {
        Ok(self.bytes.clone())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::FULL
    }

    fn endianness(&self) -> savelens_core::Endianness {
        savelens_core::Endianness::Big
    }

    fn notes(&self) -> Vec<String> {
        let h = &self.proj.header;
        let mut notes = vec![format!(
            "SQLite database: {} table(s), {} row(s), journal mode {}, page size {}, encoding {}",
            self.proj.tables.len(),
            self.proj.rows,
            if h.wal { "wal" } else { "rollback" },
            h.page_size,
            encoding_name(h.encoding)
        )];
        if !self.proj.skipped.is_empty() {
            notes.push(format!("not projected (virtual/shadow tables): {}", self.proj.skipped.join(", ")));
        }
        notes
    }

    fn clone_box(&self) -> Box<dyn Document> {
        Box::new(self.clone())
    }
}

/// SQLite codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct SqliteCodec;

impl StructuralCodec for SqliteCodec {
    fn id(&self) -> &'static str {
        CODEC
    }

    fn name(&self) -> &'static str {
        "SQLite database"
    }

    fn detect(&self, bytes: &[u8], _ctx: &DetectContext) -> Confidence {
        if parse_header(bytes).is_ok() {
            95
        } else {
            0
        }
    }

    fn decode(&self, bytes: &[u8], _ctx: &DetectContext) -> CodecResult<Box<dyn Document>> {
        Ok(Box::new(SqliteDocument::parse(bytes)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(setup: &str) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        let conn = Connection::open(&p).unwrap();
        conn.execute_batch(setup).unwrap();
        conn.close().unwrap();
        std::fs::read(&p).unwrap()
    }

    fn sample() -> Vec<u8> {
        build(
            "CREATE TABLE p(id INTEGER PRIMARY KEY, name TEXT, gold INTEGER);
             INSERT INTO p VALUES (1,'Hero',100),(2,'Mage',50);
             CREATE TABLE kv(k TEXT, v BLOB, PRIMARY KEY(k)) WITHOUT ROWID;
             INSERT INTO kv VALUES ('a|b', x'0102'), ('c\\d', NULL);",
        )
    }

    /// Path from raw keys (no quoting needed).
    fn p(s: &str) -> NodePath {
        NodePath::from_segments(s.split('/').map(|k| PathSeg::Key(k.to_string())).collect())
    }

    #[test]
    fn header_and_detect() {
        let db = sample();
        assert_eq!(SqliteCodec.detect(&db, &DetectContext::default()), 95);
        assert_eq!(SqliteCodec.detect(b"SQLite format 3\0", &DetectContext::default()), 0);
        let mut bad = db.clone();
        bad[16] = 0;
        bad[17] = 3;
        assert_eq!(SqliteCodec.detect(&bad, &DetectContext::default()), 0);
        assert_eq!(SqliteCodec.detect(b"{}", &DetectContext::default()), 0);
    }

    #[test]
    fn projection_and_keys() {
        let db = sample();
        let doc = SqliteDocument::parse(&db).unwrap();
        assert_eq!(doc.encode().unwrap(), db);
        let ast = doc.to_ast();
        assert_eq!(ast.get(&p("p/1/name")), Some(&Node::String("Hero".into())));
        assert_eq!(ast.get(&p("p/2/gold")), Some(&Node::I64(50)));
        assert_eq!(ast.get(&p("p/2/id")), Some(&Node::I64(2)));
        let Some(Node::Map(kv)) = ast.get(&p("kv")) else { panic!() };
        let keys: Vec<&String> = kv.keys().collect();
        assert_eq!(keys, vec!["a\\|b", "c\\\\d"]);
        assert_eq!(kv["a\\|b"].get(&p("v")), Some(&Node::Bytes(vec![1, 2])));
        assert!(doc.notes()[0].contains("2 table(s), 4 row(s)"));
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(key_part(&Value::Text("x|y\\".into())), "x\\|y\\\\");
        assert!(immutable_uri(Path::new("/tmp/a b?/c")).starts_with("file:/tmp/a%20b%3F/c"));
    }

    #[test]
    fn update_insert_delete() {
        let db = sample();
        let mut doc = SqliteDocument::parse(&db).unwrap();
        let mut ast = doc.to_ast();
        ast.set(&p("p/1/gold"), Node::I32(9999), false).unwrap();
        ast.set(&p("p/1/name"), Node::Bool(true), false).unwrap();
        ast.remove(&p("p/2")).unwrap();
        ast.set(&p("p/7"), Node::map_from([("name", Node::String("New".into()))]), true).unwrap();
        ast.set(&p("kv/z"), Node::map_from([("k", Node::String("z".into())), ("v", Node::F64(1.5))]), true).unwrap();
        doc.apply_ast(&ast).unwrap();
        let out = doc.to_ast();
        assert_eq!(out.get(&p("p/1/gold")), Some(&Node::I64(9999)));
        // TEXT affinity stores the Bool edit (INTEGER 1) as text.
        assert_eq!(out.get(&p("p/1/name")), Some(&Node::String("1".into())));
        assert!(out.get(&p("p/2")).is_none());
        assert_eq!(out.get(&p("p/7/id")), Some(&Node::I64(7)));
        assert_eq!(out.get(&p("p/7/gold")), Some(&Node::Null));
        assert_eq!(out.get(&p("kv/z/v")), Some(&Node::F64(1.5)));
        let re = SqliteDocument::parse(&doc.encode().unwrap()).unwrap();
        assert_eq!(re.to_ast(), out);
    }

    #[test]
    fn unchanged_edit_keeps_bytes() {
        let db = sample();
        let mut doc = SqliteDocument::parse(&db).unwrap();
        let ast = doc.to_ast();
        doc.apply_ast(&ast).unwrap();
        assert_eq!(doc.encode().unwrap(), db);
    }

    #[test]
    fn rejected_edits() {
        let db = sample();
        let doc = SqliteDocument::parse(&db).unwrap();
        let base = doc.to_ast();
        type Edit = Box<dyn Fn(&mut Node)>;
        let cases: Vec<Edit> = vec![
            Box::new(|a| {
                a.set(&p("p/1/extra"), Node::I64(1), true).unwrap();
            }),
            Box::new(|a| {
                a.remove(&p("p/1/gold")).unwrap();
            }),
            Box::new(|a| {
                a.set(&p("newt"), Node::map(), true).unwrap();
            }),
            Box::new(|a| {
                a.remove(&p("kv")).unwrap();
            }),
            Box::new(|a| {
                a.set(&p("p/1/id"), Node::I64(50), false).unwrap();
            }),
            Box::new(|a| {
                a.set(&p("kv/a\\|b/k"), Node::String("q".into()), false).unwrap();
            }),
            Box::new(|a| {
                a.set(&p("p/1/gold"), Node::Array(vec![]), false).unwrap();
            }),
            Box::new(|a| {
                a.set(&p("p/007"), Node::map(), true).unwrap();
            }),
            Box::new(|a| {
                a.set(&p("kv/wrong"), Node::map_from([("k", Node::String("z".into()))]), true).unwrap();
            }),
            Box::new(|a| {
                a.set(&p("p/1/gold"), Node::U64(u64::MAX), false).unwrap();
            }),
        ];
        for (i, f) in cases.iter().enumerate() {
            let mut a = base.clone();
            f(&mut a);
            let mut d = doc.clone();
            assert!(d.apply_ast(&a).is_err(), "case {i} should fail");
            assert_eq!(d.encode().unwrap(), db, "case {i} must not change bytes");
        }
    }

    #[test]
    fn corrupt_inputs_error() {
        let db = sample();
        for cut in [50, 100, 1000, db.len() / 2, db.len() - 1] {
            assert!(SqliteDocument::parse(&db[..cut]).is_err(), "cut {cut}");
        }
        let mut garbage = db.clone();
        for b in garbage.iter_mut().skip(100) {
            *b = 0xAB;
        }
        assert!(SqliteDocument::parse(&garbage).is_err());
    }

    #[test]
    fn scratch_dirs_are_removed() {
        let _ = take_recent_scratch_dirs();
        let db = sample();
        let mut doc = SqliteDocument::parse(&db).unwrap();
        let mut ast = doc.to_ast();
        ast.set(&p("p/1/gold"), Node::I64(1), false).unwrap();
        doc.apply_ast(&ast).unwrap();
        let dirs = take_recent_scratch_dirs();
        assert_eq!(dirs.len(), 3);
        let tmp = std::env::temp_dir();
        for d in dirs {
            assert!(d.starts_with(&tmp));
            assert!(!d.exists(), "{d:?} left behind");
        }
    }
}
