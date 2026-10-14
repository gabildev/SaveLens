//! SQLite save files: lossless round trip, SQL-level edits, schema
//! preservation, WAL databases, rejection of schema changes and corrupt input.

use rusqlite::Connection;
use savelens_codecs::codecs::sqlite::{take_recent_scratch_dirs, SqliteCodec};
use savelens_codecs::{DetectContext, EnvelopePipeline, OpenedSave, StructuralCodec};
use savelens_core::{Node, NodePath, PathSeg, RoundTripStatus};

const SCHEMA: &str = r#"
CREATE TABLE player(id INTEGER PRIMARY KEY, name TEXT, gold INTEGER, hp REAL, avatar BLOB, note TEXT NULL);
CREATE TABLE inventory(owner TEXT, slot TEXT, item TEXT, qty INTEGER, PRIMARY KEY(owner, slot)) WITHOUT ROWID;
CREATE TABLE flags(name TEXT, value INTEGER);
CREATE INDEX player_gold ON player(gold);
CREATE VIEW rich AS SELECT name FROM player WHERE gold > 1000;
INSERT INTO player VALUES (1, 'Hero', 1200, 87.5, x'89504E47', NULL);
INSERT INTO player VALUES (2, 'Sidekick', 30, 40.0, x'00', 'spare');
INSERT INTO player VALUES (3, 'Ghost', 0, 0.5, NULL, 'to delete');
INSERT INTO inventory VALUES ('Hero', 'belt|left', 'Potion', 3);
INSERT INTO inventory VALUES ('Hero', 'back\pack', 'Sword', 1);
INSERT INTO inventory VALUES ('Ghost', 'hand', 'Lantern', 1);
INSERT INTO flags VALUES ('intro_done', 1), ('boss_dead', 0);
"#;

/// Builds a database in a private temp dir and returns its main file bytes.
fn build_db(wal: bool) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("save.db");
    let conn = Connection::open(&path).unwrap();
    if wal {
        let mode: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }
    conn.execute_batch(SCHEMA).unwrap();
    conn.close().unwrap();
    assert!(
        !dir.path().join("save.db-wal").exists()
            || std::fs::metadata(dir.path().join("save.db-wal"))
                .unwrap()
                .len()
                == 0
    );
    std::fs::read(&path).unwrap()
}

fn p(keys: &[&str]) -> NodePath {
    NodePath::from_segments(
        keys.iter()
            .map(|k| PathSeg::Key((*k).to_string()))
            .collect(),
    )
}

fn ctx() -> DetectContext {
    DetectContext::for_file("slot1.db")
}

/// Opens bytes written by the codec directly with rusqlite (private copy).
fn reopen(bytes: &[u8]) -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("check.db");
    std::fs::write(&path, bytes).unwrap();
    let conn = Connection::open(&path).unwrap();
    (dir, conn)
}

fn schema_of(conn: &Connection) -> Vec<(String, String, Option<String>)> {
    let mut st = conn
        .prepare("SELECT type, name, sql FROM sqlite_master ORDER BY rowid")
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn open(bytes: &[u8]) -> OpenedSave {
    let s = EnvelopePipeline::builtin().open(bytes, &ctx()).unwrap();
    assert_eq!(s.codec_id(), "sqlite", "notes: {:?}", s.envelope().notes);
    s
}

#[test]
fn detect_and_lossless_round_trip() {
    for wal in [false, true] {
        let db = build_db(wal);
        assert_eq!(db[18] == 2, wal);
        assert_eq!(SqliteCodec.detect(&db, &ctx()), 95);
        let s = open(&db);
        assert!(s.layers().is_empty());
        assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
        assert_eq!(s.encode().unwrap(), db);
        let notes = s.envelope().notes.join("\n");
        assert!(notes.contains("3 table(s), 8 row(s)"), "{notes}");
        assert!(
            notes.contains(if wal {
                "journal mode wal"
            } else {
                "journal mode rollback"
            }),
            "{notes}"
        );
        assert!(notes.contains("page size 4096"), "{notes}");

        let root = s.root();
        let Node::Map(tables) = &root else {
            panic!("root not a map")
        };
        let names: Vec<&String> = tables.keys().collect();
        assert_eq!(
            names,
            vec!["player", "inventory", "flags"],
            "views/indexes must not be projected"
        );
        assert_eq!(
            root.get(&p(&["player", "1", "name"])),
            Some(&Node::String("Hero".into()))
        );
        assert_eq!(
            root.get(&p(&["player", "1", "gold"])),
            Some(&Node::I64(1200))
        );
        assert_eq!(root.get(&p(&["player", "1", "hp"])), Some(&Node::F64(87.5)));
        assert_eq!(
            root.get(&p(&["player", "1", "avatar"])),
            Some(&Node::Bytes(vec![0x89, b'P', b'N', b'G']))
        );
        assert_eq!(root.get(&p(&["player", "1", "note"])), Some(&Node::Null));
        assert_eq!(
            root.get(&p(&["flags", "2", "name"])),
            Some(&Node::String("boss_dead".into()))
        );
        let Some(Node::Map(inv)) = root.get(&p(&["inventory"])) else {
            panic!()
        };
        let keys: Vec<&String> = inv.keys().collect();
        // Ordered by primary key (owner, slot); `|` and `\` escaped.
        assert_eq!(
            keys,
            vec!["Ghost|hand", "Hero|back\\\\pack", "Hero|belt\\|left"]
        );
    }
}

#[test]
fn edit_insert_delete_and_verify_sql() {
    for wal in [false, true] {
        let db = build_db(wal);
        let (_d, orig_conn) = reopen(&db);
        let orig_schema = schema_of(&orig_conn);
        drop(orig_conn);

        let pipeline = EnvelopePipeline::builtin();
        let mut s = open(&db);
        let mut root = s.root();
        root.set(&p(&["player", "1", "gold"]), Node::I64(999_999), false)
            .unwrap();
        root.set(
            &p(&["player", "1", "name"]),
            Node::String("Hero \"the\" Brave".into()),
            false,
        )
        .unwrap();
        root.set(&p(&["player", "2", "hp"]), Node::F32(100.0), false)
            .unwrap();
        root.set(
            &p(&["player", "2", "avatar"]),
            Node::Bytes(vec![1, 2, 3, 4, 5]),
            false,
        )
        .unwrap();
        root.remove(&p(&["player", "3"])).unwrap();
        root.set(
            &p(&["player", "10"]),
            Node::map_from([
                ("id", Node::I64(10)),
                ("name", Node::String("Recruit".into())),
                ("gold", Node::I32(5)),
                ("hp", Node::F64(12.25)),
                ("avatar", Node::Null),
                ("note", Node::String("joined".into())),
            ]),
            true,
        )
        .unwrap();
        root.set(
            &p(&["inventory", "Hero|belt\\|left", "qty"]),
            Node::I64(99),
            false,
        )
        .unwrap();
        root.remove(&p(&["inventory", "Ghost|hand"])).unwrap();
        root.set(
            &p(&["inventory", "Recruit|a\\|b"]),
            Node::map_from([
                ("owner", Node::String("Recruit".into())),
                ("slot", Node::String("a|b".into())),
                ("item", Node::String("Stick".into())),
                ("qty", Node::I64(2)),
            ]),
            true,
        )
        .unwrap();
        root.set(&p(&["flags", "2", "value"]), Node::Bool(true), false)
            .unwrap();
        s.apply_ast(&root).unwrap();

        let out = s.encode_verified(&pipeline, &ctx()).unwrap();
        assert_ne!(out, db);
        assert_eq!(out[18] == 2, wal, "journal mode must be preserved");
        assert_eq!(&out[16..18], &db[16..18], "page size must be preserved");
        assert_eq!(&out[56..60], &db[56..60], "text encoding must be preserved");

        let (_d2, conn) = reopen(&out);
        let check: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(check, "ok");
        assert_eq!(schema_of(&conn), orig_schema, "schema must be identical");

        let (name, gold): (String, i64) = conn
            .query_row("SELECT name, gold FROM player WHERE id = 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(name, "Hero \"the\" Brave");
        assert_eq!(gold, 999_999);
        let (hp, avatar): (f64, Vec<u8>) = conn
            .query_row("SELECT hp, avatar FROM player WHERE id = 2", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(hp, 100.0);
        assert_eq!(avatar, vec![1, 2, 3, 4, 5]);
        let ids: Vec<i64> = conn
            .prepare("SELECT id FROM player ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(ids, vec![1, 2, 10]);
        let (rname, rgold, rnote): (String, i64, String) = conn
            .query_row(
                "SELECT name, gold, note FROM player WHERE id = 10",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (rname.as_str(), rgold, rnote.as_str()),
            ("Recruit", 5, "joined")
        );
        let qty: i64 = conn
            .query_row(
                "SELECT qty FROM inventory WHERE owner = 'Hero' AND slot = 'belt|left'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(qty, 99);
        let ghost: i64 = conn
            .query_row(
                "SELECT count(*) FROM inventory WHERE owner = 'Ghost'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ghost, 0);
        let stick: String = conn
            .query_row(
                "SELECT item FROM inventory WHERE owner = 'Recruit' AND slot = 'a|b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stick, "Stick");
        let flag: i64 = conn
            .query_row(
                "SELECT value FROM flags WHERE name = 'boss_dead'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(flag, 1);
        // The index still serves queries and agrees with the table.
        let rich: Vec<String> = conn
            .prepare("SELECT name FROM rich")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rich, vec!["Hero \"the\" Brave".to_string()]);
        drop(conn);

        // The edited file is itself a lossless, editable save.
        let s2 = open(&out);
        assert_eq!(s2.envelope().round_trip, Some(RoundTripStatus::Lossless));
        assert_eq!(s2.encode().unwrap(), out);
        assert_eq!(s2.root(), s.root());
    }
}

#[test]
fn set_single_value_through_pipeline() {
    let db = build_db(false);
    let mut s = open(&db);
    s.set(&p(&["player", "2", "gold"]), Node::I64(77)).unwrap();
    let out = s
        .encode_verified(&EnvelopePipeline::builtin(), &ctx())
        .unwrap();
    let (_d, conn) = reopen(&out);
    let gold: i64 = conn
        .query_row("SELECT gold FROM player WHERE id = 2", [], |r| r.get(0))
        .unwrap();
    assert_eq!(gold, 77);
}

#[test]
fn empty_edit_keeps_bytes() {
    let db = build_db(true);
    let mut s = open(&db);
    let root = s.root();
    s.apply_ast(&root).unwrap();
    assert_eq!(s.encode().unwrap(), db);
}

#[test]
fn schema_changes_are_rejected() {
    let db = build_db(false);
    let s = open(&db);
    let base = s.root();
    type Edit = Box<dyn Fn(&mut Node)>;
    let edits: Vec<(&str, Edit)> = vec![
        (
            "add column",
            Box::new(|r| {
                r.set(&p(&["player", "1", "level"]), Node::I64(3), true)
                    .unwrap();
            }),
        ),
        (
            "add column on insert",
            Box::new(|r| {
                r.set(
                    &p(&["player", "50"]),
                    Node::map_from([("level", Node::I64(3))]),
                    true,
                )
                .unwrap();
            }),
        ),
        (
            "remove column",
            Box::new(|r| {
                r.remove(&p(&["player", "1", "note"])).unwrap();
            }),
        ),
        (
            "add table",
            Box::new(|r| {
                r.set(&p(&["achievements"]), Node::map(), true).unwrap();
            }),
        ),
        (
            "remove table",
            Box::new(|r| {
                r.remove(&p(&["flags"])).unwrap();
            }),
        ),
        (
            "change rowid pk",
            Box::new(|r| {
                r.set(&p(&["player", "1", "id"]), Node::I64(42), false)
                    .unwrap();
            }),
        ),
        (
            "change composite pk",
            Box::new(|r| {
                r.set(
                    &p(&["inventory", "Ghost|hand", "slot"]),
                    Node::String("foot".into()),
                    false,
                )
                .unwrap();
            }),
        ),
        (
            "insert with mismatched key",
            Box::new(|r| {
                r.set(
                    &p(&["inventory", "wrong"]),
                    Node::map_from([
                        ("owner", Node::String("A".into())),
                        ("slot", Node::String("B".into())),
                    ]),
                    true,
                )
                .unwrap();
            }),
        ),
    ];
    for (what, f) in &edits {
        let mut root = base.clone();
        f(&mut root);
        let mut work = s.clone();
        let err = work.apply_ast(&root);
        assert!(err.is_err(), "{what} must be rejected");
        assert_eq!(
            work.encode().unwrap(),
            db,
            "{what}: failed edit must not change the save"
        );
    }
}

#[test]
fn temporary_copies_are_removed() {
    let _ = take_recent_scratch_dirs();
    let db = build_db(true);
    let mut s = open(&db);
    s.set(&p(&["player", "1", "gold"]), Node::I64(1)).unwrap();
    let _ = s
        .encode_verified(&EnvelopePipeline::builtin(), &ctx())
        .unwrap();
    let mut root = s.root();
    root.set(&p(&["nope"]), Node::map(), true).unwrap();
    assert!(s.apply_ast(&root).is_err());
    let dirs = take_recent_scratch_dirs();
    assert!(dirs.len() >= 4, "{dirs:?}");
    let tmp = std::env::temp_dir();
    for d in &dirs {
        assert!(d.starts_with(&tmp), "{d:?} outside the system temp dir");
        assert!(!d.exists(), "{d:?} was left behind");
    }
}

#[test]
fn corrupt_input_does_not_panic() {
    let db = build_db(false);
    let pipeline = EnvelopePipeline::builtin();
    let page = 4096;
    let cuts = [
        0,
        16,
        99,
        100,
        101,
        page,
        page + 1,
        db.len() - page,
        db.len() - 1,
    ];
    for cut in cuts {
        let data = &db[..cut];
        assert!(
            SqliteCodec.decode(data, &ctx()).is_err(),
            "cut at {cut} must not decode"
        );
        let s = pipeline.open(data, &ctx()).unwrap();
        assert_ne!(s.codec_id(), "sqlite", "cut at {cut}");
    }
    // Scrambled b-tree pages.
    let mut bad = db.clone();
    for (i, b) in bad.iter_mut().enumerate().skip(page) {
        *b = (i * 31 % 251) as u8;
    }
    assert!(SqliteCodec.decode(&bad, &ctx()).is_err());
    let s = pipeline.open(&bad, &ctx()).unwrap();
    assert_eq!(s.codec_id(), "raw");
    // Bad page size in an otherwise valid header.
    let mut bad_ps = db.clone();
    bad_ps[16] = 0x03;
    bad_ps[17] = 0x00;
    assert_eq!(SqliteCodec.detect(&bad_ps, &ctx()), 0);
    assert!(SqliteCodec.decode(&bad_ps, &ctx()).is_err());
}
