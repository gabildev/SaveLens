//! Save-State Scanner ("Cheat Engine for files on disk").
//!
//! A [`ScanSession`] narrows down the location of a variable across successive
//! snapshots of a save file. It scans simultaneously:
//!
//! * named numeric leaves of the AST (`Player.Gold`),
//! * numeric strings in the AST (`"gold": "1500"`),
//! * every `Bytes`/`RawRegion` node at aligned or unaligned offsets, as
//!   `i8..u64`/`f32`/`f64` in little and big endian, and ASCII decimals.
//!
//! An *unknown initial value* scan keeps a copy of each raw region instead of
//! materialising every `(offset, type, endian)` triple; consecutive
//! `Unchanged` filters are tracked with a per-byte dirty bitmap so they stay
//! cheap even on multi-megabyte files.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};
use crate::node::{parse_numeric_string, Node, ScalarValue};
use crate::path::NodePath;
use crate::raw::{read_ascii_decimal, read_primitive, write_primitive, Endianness, ScanType};

/// Where a scanned value lives.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Location {
    /// A numeric leaf or numeric string of the AST.
    Ast { path: NodePath },
    /// A primitive inside a `Bytes`/`RawRegion` node.
    Raw { region: NodePath, offset: u64, ty: ScanType, endian: Endianness },
}

impl Location {
    /// Reads the value at this location from a snapshot.
    pub fn read(&self, root: &Node) -> Option<ScalarValue> {
        match self {
            Location::Ast { path } => read_ast_value(root.get(path)?),
            Location::Raw { region, offset, ty, endian } => {
                let bytes = region_bytes(root.get(region)?)?;
                read_primitive(bytes, usize::try_from(*offset).ok()?, *ty, *endian)
            }
        }
    }

    /// Writes `value` at this location (type and range checked).
    pub fn write(&self, root: &mut Node, value: ScalarValue) -> CoreResult<()> {
        match self {
            Location::Ast { path } => {
                let node = root.resolve_mut(path)?;
                let new = node.with_scalar(value, path)?;
                *node = new;
                Ok(())
            }
            Location::Raw { region, offset, ty, endian } => {
                let node = root.resolve_mut(region)?;
                let off = usize::try_from(*offset).map_err(|_| CoreError::RawOffsetOutOfRange {
                    path: region.to_string(),
                    offset: *offset,
                    width: ty.width().unwrap_or(1),
                    len: 0,
                })?;
                let name = region.to_string();
                match node {
                    Node::Bytes(b) => write_primitive(b, off, *ty, *endian, value, &name),
                    Node::RawRegion(r) => {
                        write_primitive(&mut r.bytes, off, *ty, *endian, value, &name)?;
                        r.reindex();
                        Ok(())
                    }
                    other => Err(CoreError::TypeMismatch {
                        path: name,
                        expected: "bytes or raw region".into(),
                        found: other.kind().to_string(),
                    }),
                }
            }
        }
    }

    /// Short type description (`i32`, `f32le@raw`, …).
    pub fn type_label(&self, root: Option<&Node>) -> String {
        match self {
            Location::Ast { path } => root
                .and_then(|r| r.get(path))
                .map(|n| match n {
                    Node::String(_) => "numeric string".to_string(),
                    other => other.kind().to_string(),
                })
                .unwrap_or_else(|| "ast".to_string()),
            Location::Raw { ty, endian, .. } => {
                if ty.is_multibyte() {
                    format!("{}{}", ty.name(), endian.suffix())
                } else {
                    ty.name().to_string()
                }
            }
        }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Location::Ast { path } => write!(f, "{path}"),
            Location::Raw { region, offset, ty, endian } => {
                if ty.is_multibyte() {
                    write!(f, "{region}@{offset:#x}:{}{}", ty.name(), endian.suffix())
                } else {
                    write!(f, "{region}@{offset:#x}:{}", ty.name())
                }
            }
        }
    }
}

fn read_ast_value(node: &Node) -> Option<ScalarValue> {
    match node {
        Node::String(s) => parse_numeric_string(s).map(|(v, _)| v),
        other => other.as_scalar(),
    }
}

fn region_bytes(node: &Node) -> Option<&[u8]> {
    match node {
        Node::Bytes(b) => Some(b),
        Node::RawRegion(r) => Some(&r.bytes),
        _ => None,
    }
}

/// Offset alignment for raw scans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Alignment {
    /// Every byte offset.
    Unaligned,
    /// Offsets that are a multiple of the type width.
    Natural,
    /// Offsets that are a multiple of `n`.
    Fixed(usize),
}

impl Alignment {
    fn accepts(self, offset: usize, ty: ScanType) -> bool {
        match self {
            Alignment::Unaligned => true,
            Alignment::Natural => offset % ty.width().unwrap_or(1) == 0,
            Alignment::Fixed(n) => offset % n.max(1) == 0,
        }
    }
}

/// Scanner configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanConfig {
    /// Primitive types tried on raw regions.
    pub types: Vec<ScanType>,
    /// Byte orders tried for multi-byte types.
    pub endians: Vec<Endianness>,
    /// Offset alignment for raw regions.
    pub alignment: Alignment,
    /// Scan numeric AST leaves.
    pub scan_ast: bool,
    /// Scan numeric strings in the AST.
    pub scan_numeric_strings: bool,
    /// Scan `Bytes`/`RawRegion` nodes.
    pub scan_raw: bool,
    /// Absolute tolerance when comparing floats.
    pub float_tolerance: f64,
    /// Upper bound of explicit candidates kept in memory.
    pub max_candidates: usize,
}

impl Default for ScanConfig {
    fn default() -> Self {
        ScanConfig {
            types: ScanType::COMMON.to_vec(),
            endians: vec![Endianness::Little],
            alignment: Alignment::Unaligned,
            scan_ast: true,
            scan_numeric_strings: true,
            scan_raw: true,
            float_tolerance: 1e-3,
            max_candidates: 2_000_000,
        }
    }
}

/// Filters of the first and next scans.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "filter", content = "args", rename_all = "kebab-case")]
pub enum ScanFilter {
    /// First scan only: remember every value.
    Unknown,
    /// Value equals `v`.
    Exact(ScalarValue),
    /// `min <= value <= max`.
    Between(ScalarValue, ScalarValue),
    /// Value grew.
    Increased,
    /// Value shrank.
    Decreased,
    /// Value is different.
    Changed,
    /// Value is identical.
    Unchanged,
    /// Value grew by exactly `d`.
    IncreasedBy(ScalarValue),
    /// Value shrank by exactly `d`.
    DecreasedBy(ScalarValue),
    /// Value was `from` and is now `to` (`1500 -> 1300`).
    Transition { from: ScalarValue, to: ScalarValue },
}

impl ScanFilter {
    /// True if the filter compares against the previous value.
    pub fn is_relational(&self) -> bool {
        matches!(
            self,
            ScanFilter::Increased
                | ScanFilter::Decreased
                | ScanFilter::Changed
                | ScanFilter::Unchanged
                | ScanFilter::IncreasedBy(_)
                | ScanFilter::DecreasedBy(_)
                | ScanFilter::Transition { .. }
        )
    }

    /// Parses compact user syntax:
    /// `1500`, `=1500`, `1500 -> 1300`, `+300`, `-300`, `>`/`inc`, `<`/`dec`,
    /// `!=`/`changed`, `==`/`unchanged`, `10..20`, `?`/`unknown`.
    pub fn parse(input: &str) -> CoreResult<ScanFilter> {
        let s = input.trim();
        let lower = s.to_ascii_lowercase();
        let simple = match lower.as_str() {
            "?" | "unknown" => Some(ScanFilter::Unknown),
            ">" | "inc" | "increased" => Some(ScanFilter::Increased),
            "<" | "dec" | "decreased" => Some(ScanFilter::Decreased),
            "!=" | "changed" => Some(ScanFilter::Changed),
            "==" | "unchanged" | "same" => Some(ScanFilter::Unchanged),
            _ => None,
        };
        if let Some(f) = simple {
            return Ok(f);
        }
        if let Some((a, b)) = s.split_once("->") {
            return Ok(ScanFilter::Transition { from: ScalarValue::parse(a)?, to: ScalarValue::parse(b)? });
        }
        if let Some((a, b)) = s.split_once("..") {
            return Ok(ScanFilter::Between(ScalarValue::parse(a)?, ScalarValue::parse(b)?));
        }
        if let Some(rest) = s.strip_prefix('+') {
            return Ok(ScanFilter::IncreasedBy(ScalarValue::parse(rest)?));
        }
        if let Some(rest) = s.strip_prefix("-=") {
            return Ok(ScanFilter::DecreasedBy(ScalarValue::parse(rest)?));
        }
        if let Some(rest) = s.strip_prefix('=') {
            return Ok(ScanFilter::Exact(ScalarValue::parse(rest)?));
        }
        if let Some(rest) = s.strip_prefix('-') {
            // "-300" alone is ambiguous; a leading "-" with a space or
            // explicit "dec" keyword means "decreased by".
            if rest.starts_with(' ') {
                return Ok(ScanFilter::DecreasedBy(ScalarValue::parse(rest)?));
            }
        }
        Ok(ScanFilter::Exact(ScalarValue::parse(s)?))
    }
}

impl fmt::Display for ScanFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanFilter::Unknown => write!(f, "unknown initial value"),
            ScanFilter::Exact(v) => write!(f, "= {v}"),
            ScanFilter::Between(a, b) => write!(f, "between {a} and {b}"),
            ScanFilter::Increased => write!(f, "increased"),
            ScanFilter::Decreased => write!(f, "decreased"),
            ScanFilter::Changed => write!(f, "changed"),
            ScanFilter::Unchanged => write!(f, "unchanged"),
            ScanFilter::IncreasedBy(d) => write!(f, "increased by {d}"),
            ScanFilter::DecreasedBy(d) => write!(f, "decreased by {d}"),
            ScanFilter::Transition { from, to } => write!(f, "{from} -> {to}"),
        }
    }
}

fn approx_eq(a: ScalarValue, b: ScalarValue, tol: f64) -> bool {
    match (a, b) {
        (ScalarValue::Int(x), ScalarValue::Int(y)) => x == y,
        (x, y) => {
            let (x, y) = (x.as_f64(), y.as_f64());
            if x.is_nan() || y.is_nan() {
                return false;
            }
            (x - y).abs() <= tol.max(y.abs() * 1e-9)
        }
    }
}

fn cmp(a: ScalarValue, b: ScalarValue) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (ScalarValue::Int(x), ScalarValue::Int(y)) => Some(x.cmp(&y)),
        (x, y) => x.as_f64().partial_cmp(&y.as_f64()),
    }
}

/// Evaluates a filter for a value (`prev` is `None` on first scans).
pub fn filter_matches(filter: &ScanFilter, prev: Option<ScalarValue>, cur: ScalarValue, tol: f64) -> bool {
    use std::cmp::Ordering::{Equal, Greater, Less};
    if cur.is_nan() {
        return false;
    }
    let prev = prev.filter(|p| !p.is_nan());
    let exact_tol = |p: ScalarValue| match (cur, p) {
        (ScalarValue::Int(_), ScalarValue::Int(_)) => 0.0,
        _ => tol,
    };
    match filter {
        ScanFilter::Unknown => true,
        ScanFilter::Exact(v) => approx_eq(cur, *v, tol),
        ScanFilter::Between(lo, hi) => {
            matches!(cmp(cur, *lo), Some(Greater | Equal)) && matches!(cmp(cur, *hi), Some(Less | Equal))
        }
        ScanFilter::Increased => prev.is_some_and(|p| cmp(cur, p) == Some(Greater) && !approx_eq(cur, p, tol)),
        ScanFilter::Decreased => prev.is_some_and(|p| cmp(cur, p) == Some(Less) && !approx_eq(cur, p, tol)),
        ScanFilter::Changed => prev.is_some_and(|p| !approx_eq(cur, p, exact_tol(p))),
        ScanFilter::Unchanged => prev.is_some_and(|p| approx_eq(cur, p, tol)),
        ScanFilter::IncreasedBy(d) => prev.is_some_and(|p| approx_eq(cur.delta(p), *d, tol)),
        ScanFilter::DecreasedBy(d) => prev.is_some_and(|p| approx_eq(p.delta(cur), *d, tol)),
        ScanFilter::Transition { from, to } => {
            prev.is_some_and(|p| approx_eq(p, *from, tol) && approx_eq(cur, *to, tol))
        }
    }
}

/// A candidate as presented to UIs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateView {
    pub location: Location,
    pub previous: ScalarValue,
    pub current: ScalarValue,
}

#[derive(Debug, Clone)]
struct AstCand {
    path: NodePath,
    prev: ScalarValue,
    cur: ScalarValue,
}

#[derive(Debug, Clone, Copy)]
struct RawCand {
    region: u32,
    offset: u64,
    ty: ScanType,
    endian: Endianness,
    prev: ScalarValue,
    cur: ScalarValue,
}

#[derive(Debug, Clone)]
struct PendingRegion {
    region: u32,
    bytes: Vec<u8>,
    /// Bit set of bytes that changed since the unknown scan (for `Unchanged`).
    dirty: Vec<u64>,
}

impl PendingRegion {
    fn is_dirty(&self, start: usize, len: usize) -> bool {
        (start..start + len).any(|i| self.dirty.get(i / 64).is_some_and(|w| w & (1u64 << (i % 64)) != 0))
    }

    fn mark(&mut self, i: usize) {
        if let Some(w) = self.dirty.get_mut(i / 64) {
            *w |= 1u64 << (i % 64);
        }
    }
}

/// History entry of a scan session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanStep {
    pub filter: String,
    pub remaining: usize,
    pub snapshot_label: String,
}

/// A multi-step scan session.
#[derive(Debug, Clone)]
pub struct ScanSession {
    config: ScanConfig,
    regions: Vec<NodePath>,
    region_index: HashMap<NodePath, u32>,
    ast: Vec<AstCand>,
    raw: Vec<RawCand>,
    pending: Vec<PendingRegion>,
    steps: Vec<ScanStep>,
}

impl ScanSession {
    /// New empty session.
    pub fn new(config: ScanConfig) -> Self {
        ScanSession {
            config,
            regions: Vec::new(),
            region_index: HashMap::new(),
            ast: Vec::new(),
            raw: Vec::new(),
            pending: Vec::new(),
            steps: Vec::new(),
        }
    }

    /// Configuration.
    pub fn config(&self) -> &ScanConfig {
        &self.config
    }

    /// True before the first scan.
    pub fn is_fresh(&self) -> bool {
        self.steps.is_empty()
    }

    /// Scan history.
    pub fn steps(&self) -> &[ScanStep] {
        &self.steps
    }

    /// Clears all state (keeps the configuration).
    pub fn reset(&mut self) {
        *self = ScanSession::new(self.config.clone());
    }

    /// Number of explicit candidates plus the implicit raw candidate count.
    pub fn candidate_count(&self) -> usize {
        self.ast.len() + self.raw.len() + self.implicit_count()
    }

    /// Number of not-yet-materialised raw candidates (unknown-value scans).
    pub fn implicit_count(&self) -> usize {
        self.pending
            .iter()
            .map(|p| {
                let mut n = 0usize;
                for &ty in &self.config.types {
                    let w = ty.width().unwrap_or(1);
                    let endians = if ty.is_multibyte() { self.config.endians.len() } else { 1 };
                    let mut positions = 0usize;
                    for off in 0..p.bytes.len().saturating_sub(w - 1) {
                        if self.config.alignment.accepts(off, ty) && !p.is_dirty(off, w) {
                            positions += 1;
                        }
                    }
                    n += positions * endians;
                }
                n
            })
            .sum()
    }

    fn region_id(&mut self, path: &NodePath) -> u32 {
        if let Some(id) = self.region_index.get(path) {
            return *id;
        }
        let id = self.regions.len() as u32;
        self.regions.push(path.clone());
        self.region_index.insert(path.clone(), id);
        id
    }

    fn raw_endians(&self, ty: ScanType) -> Vec<Endianness> {
        if ty.is_multibyte() {
            self.config.endians.clone()
        } else {
            vec![Endianness::Little]
        }
    }

    /// First scan over a snapshot. Accepts `Unknown`, `Exact` and `Between`.
    pub fn first_scan(&mut self, snapshot: &Node, filter: &ScanFilter, label: &str) -> CoreResult<usize> {
        if filter.is_relational() {
            return Err(CoreError::Scan(format!(
                "`{filter}` needs a previous scan; use an exact value, a range or unknown for the first scan"
            )));
        }
        if self.config.types.is_empty() && self.config.scan_raw {
            return Err(CoreError::Scan("no scan types configured".into()));
        }
        self.reset();
        let tol = self.config.float_tolerance;
        let budget = self.config.max_candidates;
        let mut ast = Vec::new();
        let mut regions: Vec<(NodePath, Vec<u8>)> = Vec::new();
        let mut budget_err = false;
        let cfg = self.config.clone();
        snapshot.walk(|path, node| {
            match node {
                Node::Bytes(_) | Node::RawRegion(_) if cfg.scan_raw => {
                    if let Some(b) = region_bytes(node) {
                        regions.push((path.clone(), b.to_vec()));
                    }
                }
                Node::String(_) if cfg.scan_ast && cfg.scan_numeric_strings => {
                    if let Some(v) = read_ast_value(node) {
                        if filter_matches(filter, None, v, tol) {
                            ast.push(AstCand { path: path.clone(), prev: v, cur: v });
                        }
                    }
                }
                n if cfg.scan_ast && n.kind().is_numeric() => {
                    if let Some(v) = n.as_scalar() {
                        if filter_matches(filter, None, v, tol) {
                            ast.push(AstCand { path: path.clone(), prev: v, cur: v });
                        }
                    }
                }
                _ => {}
            }
            if ast.len() > budget {
                budget_err = true;
            }
        });
        if budget_err {
            return Err(CoreError::TooManyCandidates { limit: budget });
        }
        self.ast = ast;
        for (path, bytes) in regions {
            let id = self.region_id(&path);
            if matches!(filter, ScanFilter::Unknown) {
                let words = bytes.len().div_ceil(64);
                self.pending.push(PendingRegion { region: id, bytes, dirty: vec![0; words] });
            } else {
                self.scan_region_values(id, &bytes, filter, budget)?;
            }
        }
        let remaining = self.candidate_count();
        self.steps.push(ScanStep { filter: filter.to_string(), remaining, snapshot_label: label.to_string() });
        Ok(remaining)
    }

    fn scan_region_values(&mut self, id: u32, bytes: &[u8], filter: &ScanFilter, budget: usize) -> CoreResult<()> {
        let tol = self.config.float_tolerance;
        for ty in self.config.types.clone() {
            for endian in self.raw_endians(ty) {
                for off in 0..bytes.len() {
                    if !self.config.alignment.accepts(off, ty) {
                        continue;
                    }
                    let Some(v) = read_raw(bytes, off, ty, endian) else { continue };
                    if filter_matches(filter, None, v, tol) {
                        self.raw.push(RawCand { region: id, offset: off as u64, ty, endian, prev: v, cur: v });
                        if self.ast.len() + self.raw.len() > budget {
                            return Err(CoreError::TooManyCandidates { limit: budget });
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Next scan against a newer snapshot.
    pub fn next_scan(&mut self, snapshot: &Node, filter: &ScanFilter, label: &str) -> CoreResult<usize> {
        if self.steps.is_empty() {
            return Err(CoreError::Scan("run a first scan before a next scan".into()));
        }
        if matches!(filter, ScanFilter::Unknown) {
            return Err(CoreError::Scan("`unknown` is only valid for the first scan".into()));
        }
        let tol = self.config.float_tolerance;
        let budget = self.config.max_candidates;

        // AST candidates.
        let mut ast = Vec::with_capacity(self.ast.len());
        for c in &self.ast {
            let Some(node) = snapshot.get(&c.path) else { continue };
            let Some(v) = read_ast_value(node) else { continue };
            if filter_matches(filter, Some(c.cur), v, tol) {
                ast.push(AstCand { path: c.path.clone(), prev: c.cur, cur: v });
            }
        }

        // Explicit raw candidates.
        let mut raw = Vec::with_capacity(self.raw.len());
        for c in &self.raw {
            let Some(bytes) = snapshot.get(&self.regions[c.region as usize]).and_then(region_bytes) else {
                continue;
            };
            let Some(v) = read_raw(bytes, c.offset as usize, c.ty, c.endian) else { continue };
            if filter_matches(filter, Some(c.cur), v, tol) {
                raw.push(RawCand { prev: c.cur, cur: v, ..*c });
            }
        }

        // Pending (unknown-initial-value) regions.
        let mut pending = Vec::new();
        for p in &self.pending {
            let path = &self.regions[p.region as usize];
            let Some(new_bytes) = snapshot.get(path).and_then(region_bytes) else { continue };
            if matches!(filter, ScanFilter::Unchanged) {
                // Stay implicit: mark changed bytes dirty.
                let mut np = p.clone();
                for i in 0..np.bytes.len() {
                    if new_bytes.get(i) != Some(&np.bytes[i]) {
                        np.mark(i);
                    }
                }
                np.bytes.truncate(new_bytes.len());
                np.dirty.truncate(np.bytes.len().div_ceil(64));
                pending.push(np);
                continue;
            }
            let len = p.bytes.len().min(new_bytes.len());
            for &ty in &self.config.types {
                for endian in self.raw_endians(ty) {
                    for off in 0..len {
                        if !self.config.alignment.accepts(off, ty) {
                            continue;
                        }
                        let w = ty.width().unwrap_or(1);
                        if off + w > len {
                            break;
                        }
                        // Fast path: identical window can only match Unchanged-like filters.
                        let same_window = p.bytes[off..off + w] == new_bytes[off..off + w];
                        if same_window
                            && matches!(filter, ScanFilter::Changed | ScanFilter::Increased | ScanFilter::Decreased)
                        {
                            continue;
                        }
                        let Some(old) = read_raw(&p.bytes, off, ty, endian) else { continue };
                        if p.is_dirty(off, raw_len(&p.bytes, off, ty)) {
                            continue;
                        }
                        let Some(v) = read_raw(new_bytes, off, ty, endian) else { continue };
                        if filter_matches(filter, Some(old), v, tol) {
                            raw.push(RawCand { region: p.region, offset: off as u64, ty, endian, prev: old, cur: v });
                            if ast.len() + raw.len() > budget {
                                return Err(CoreError::TooManyCandidates { limit: budget });
                            }
                        }
                    }
                }
            }
        }
        if ast.len() + raw.len() > budget {
            return Err(CoreError::TooManyCandidates { limit: budget });
        }
        self.ast = ast;
        self.raw = raw;
        self.pending = pending;
        let remaining = self.candidate_count();
        self.steps.push(ScanStep { filter: filter.to_string(), remaining, snapshot_label: label.to_string() });
        Ok(remaining)
    }

    /// Runs a first scan followed by next scans over a sequence of snapshots.
    pub fn run_sequence(&mut self, snapshots: &[&Node], filters: &[ScanFilter]) -> CoreResult<usize> {
        if snapshots.is_empty() || snapshots.len() != filters.len() {
            return Err(CoreError::Scan("need one filter per snapshot".into()));
        }
        let mut n = self.first_scan(snapshots[0], &filters[0], "#0")?;
        for (i, (s, f)) in snapshots.iter().zip(filters).enumerate().skip(1) {
            n = self.next_scan(s, f, &format!("#{i}"))?;
        }
        Ok(n)
    }

    /// Explicit candidates (AST first, then raw), paginated.
    pub fn candidates(&self, start: usize, len: usize) -> Vec<CandidateView> {
        let ast = self.ast.iter().map(|c| CandidateView {
            location: Location::Ast { path: c.path.clone() },
            previous: c.prev,
            current: c.cur,
        });
        let raw = self.raw.iter().map(|c| CandidateView {
            location: Location::Raw {
                region: self.regions[c.region as usize].clone(),
                offset: c.offset,
                ty: c.ty,
                endian: c.endian,
            },
            previous: c.prev,
            current: c.cur,
        });
        ast.chain(raw).skip(start).take(len).collect()
    }

    /// All explicit candidate locations.
    pub fn locations(&self) -> Vec<Location> {
        self.candidates(0, usize::MAX).into_iter().map(|c| c.location).collect()
    }

    /// Number of explicit candidates.
    pub fn explicit_count(&self) -> usize {
        self.ast.len() + self.raw.len()
    }
}

fn read_raw(bytes: &[u8], off: usize, ty: ScanType, endian: Endianness) -> Option<ScalarValue> {
    let v = read_primitive(bytes, off, ty, endian)?;
    if let ScalarValue::Float(f) = v {
        // Reject NaN, infinities, subnormals and absurd magnitudes: they are noise.
        if !f.is_finite() || (f != 0.0 && (f.abs() < 1e-30 || f.abs() > 1e15)) {
            return None;
        }
        if ty == ScanType::F32 && f != 0.0 && !(f as f32).is_normal() {
            return None;
        }
    }
    Some(v)
}

fn raw_len(bytes: &[u8], off: usize, ty: ScanType) -> usize {
    match ty.width() {
        Some(w) => w,
        None => read_ascii_decimal(bytes, off).map(|(_, l)| l).unwrap_or(1),
    }
}

/// A variable discovered by the user and stored in the cheat table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PinnedVariable {
    pub alias: String,
    pub location: Location,
    /// When set, the value is re-applied after every save ("freeze on save").
    pub frozen_value: Option<ScalarValue>,
}

impl PinnedVariable {
    /// Applies the frozen value if any; returns whether the tree changed.
    pub fn apply_freeze(&self, root: &mut Node) -> CoreResult<bool> {
        let Some(v) = self.frozen_value else { return Ok(false) };
        let current = self.location.read(root);
        if current.is_some_and(|c| approx_eq(c, v, 0.0)) {
            return Ok(false);
        }
        self.location.write(root, v)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::RawRegion;

    fn ast_save(gold: i32, hp: f32, xp: i64) -> Node {
        Node::map_from([
            (
                "Player",
                Node::map_from([
                    ("Gold", Node::I32(gold)),
                    ("HP", Node::F32(hp)),
                    ("XP", Node::I64(xp)),
                    ("GoldStr", Node::String(gold.to_string())),
                ]),
            ),
            ("Other", Node::I32(1500)),
        ])
    }

    fn raw_save(gold: i32, hp: f32) -> Node {
        let mut b = vec![0u8; 64];
        b[13..17].copy_from_slice(&gold.to_le_bytes()); // unaligned
        b[32..36].copy_from_slice(&hp.to_le_bytes());
        b[40..44].copy_from_slice(&1500i32.to_le_bytes()); // decoy with the same value
        Node::map_from([("Blob", Node::RawRegion(RawRegion::new(b)))])
    }

    #[test]
    fn ast_exact_then_transition() {
        let mut s = ScanSession::new(ScanConfig::default());
        let n = s.first_scan(&ast_save(1500, 100.0, 10), &ScanFilter::Exact(ScalarValue::Int(1500)), "a").unwrap();
        // Gold, GoldStr and Other.
        assert_eq!(n, 3);
        let n = s.next_scan(&ast_save(1300, 100.0, 10), &ScanFilter::Exact(ScalarValue::Int(1300)), "b").unwrap();
        assert_eq!(n, 2);
        let locs = s.locations();
        assert!(locs.contains(&Location::Ast { path: NodePath::parse("Player.Gold").unwrap() }));
        assert!(locs.contains(&Location::Ast { path: NodePath::parse("Player.GoldStr").unwrap() }));
    }

    #[test]
    fn raw_unaligned_and_decreased_by() {
        let cfg = ScanConfig { types: vec![ScanType::I32, ScanType::F32], ..ScanConfig::default() };
        let mut s = ScanSession::new(cfg);
        s.first_scan(&raw_save(1500, 100.0), &ScanFilter::Exact(ScalarValue::Int(1500)), "a").unwrap();
        let n = s.next_scan(&raw_save(1300, 100.0), &ScanFilter::DecreasedBy(ScalarValue::Int(200)), "b").unwrap();
        assert_eq!(n, 1);
        let c = &s.candidates(0, 10)[0];
        assert_eq!(c.current, ScalarValue::Int(1300));
        assert_eq!(c.location.to_string(), "Blob@0xd:i32le");
    }

    #[test]
    fn unknown_then_changed_then_unchanged() {
        let cfg = ScanConfig { types: vec![ScanType::I32], alignment: Alignment::Unaligned, ..ScanConfig::default() };
        let mut s = ScanSession::new(cfg);
        s.first_scan(&raw_save(1500, 100.0), &ScanFilter::Unknown, "a").unwrap();
        assert!(s.implicit_count() > 0);
        // Unchanged keeps it implicit.
        s.next_scan(&raw_save(1500, 100.0), &ScanFilter::Unchanged, "b").unwrap();
        assert_eq!(s.explicit_count(), 0);
        let n = s.next_scan(&raw_save(1700, 100.0), &ScanFilter::Increased, "c").unwrap();
        assert!(n >= 1);
        assert!(s.locations().iter().any(|l| l.to_string() == "Blob@0xd:i32le"));
        let n = s.next_scan(&raw_save(1700, 100.0), &ScanFilter::Unchanged, "d").unwrap();
        assert!(n >= 1);
        let n = s.next_scan(&raw_save(1800, 100.0), &ScanFilter::IncreasedBy(ScalarValue::Int(100)), "e").unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn unchanged_dirty_tracking_excludes_changed_windows() {
        let cfg = ScanConfig {
            types: vec![ScanType::I32],
            alignment: Alignment::Natural,
            scan_ast: false,
            ..ScanConfig::default()
        };
        let mut s = ScanSession::new(cfg);
        s.first_scan(&raw_save(1, 1.0), &ScanFilter::Unknown, "a").unwrap();
        let before = s.implicit_count();
        assert_eq!(before, 16);
        s.next_scan(&raw_save(2, 1.0), &ScanFilter::Unchanged, "b").unwrap();
        // Gold 1 -> 2 only changes byte 13, which lies in the aligned word 12..16.
        assert_eq!(s.implicit_count(), 15);
        s.next_scan(&raw_save(0x0100_0002, 1.0), &ScanFilter::Unchanged, "c").unwrap();
        // Now byte 16 changed too, invalidating the word 16..20.
        assert_eq!(s.implicit_count(), 14);
    }

    #[test]
    fn floats_with_tolerance() {
        let mut s = ScanSession::new(ScanConfig::default());
        s.first_scan(&ast_save(1, 87.5, 1), &ScanFilter::Exact(ScalarValue::Float(87.5)), "a").unwrap();
        assert!(s.locations().contains(&Location::Ast { path: NodePath::parse("Player.HP").unwrap() }));
        let n = s.next_scan(&ast_save(1, 80.0, 1), &ScanFilter::Decreased, "b").unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn errors() {
        let mut s = ScanSession::new(ScanConfig::default());
        assert!(s.next_scan(&Node::Null, &ScanFilter::Changed, "x").is_err());
        assert!(s.first_scan(&Node::Null, &ScanFilter::Changed, "x").is_err());
        s.first_scan(&Node::Null, &ScanFilter::Unknown, "x").unwrap();
        assert!(s.next_scan(&Node::Null, &ScanFilter::Unknown, "x").is_err());
    }

    #[test]
    fn budget_enforced() {
        let cfg = ScanConfig { types: vec![ScanType::U8], max_candidates: 10, ..ScanConfig::default() };
        let mut s = ScanSession::new(cfg);
        let n = Node::Bytes(vec![0; 100]);
        let err = s.first_scan(&n, &ScanFilter::Exact(ScalarValue::Int(0)), "a").unwrap_err();
        assert!(matches!(err, CoreError::TooManyCandidates { limit: 10 }));
    }

    #[test]
    fn location_write_and_freeze() {
        let mut save = raw_save(1500, 100.0);
        let loc = Location::Raw {
            region: NodePath::parse("Blob").unwrap(),
            offset: 13,
            ty: ScanType::I32,
            endian: Endianness::Little,
        };
        loc.write(&mut save, ScalarValue::Int(99999)).unwrap();
        assert_eq!(loc.read(&save), Some(ScalarValue::Int(99999)));
        let pin =
            PinnedVariable { alias: "Gold".into(), location: loc.clone(), frozen_value: Some(ScalarValue::Int(5)) };
        assert!(pin.apply_freeze(&mut save).unwrap());
        assert!(!pin.apply_freeze(&mut save).unwrap());
        assert_eq!(loc.read(&save), Some(ScalarValue::Int(5)));

        let mut a = ast_save(1, 1.0, 1);
        let l = Location::Ast { path: NodePath::parse("Player.GoldStr").unwrap() };
        l.write(&mut a, ScalarValue::Int(777)).unwrap();
        assert_eq!(a.get(&NodePath::parse("Player.GoldStr").unwrap()), Some(&Node::String("777".into())));
    }

    #[test]
    fn filter_parse() {
        assert_eq!(ScanFilter::parse("1500").unwrap(), ScanFilter::Exact(ScalarValue::Int(1500)));
        assert_eq!(ScanFilter::parse("-5").unwrap(), ScanFilter::Exact(ScalarValue::Int(-5)));
        assert_eq!(
            ScanFilter::parse("1500 -> 1300").unwrap(),
            ScanFilter::Transition { from: ScalarValue::Int(1500), to: ScalarValue::Int(1300) }
        );
        assert_eq!(ScanFilter::parse("+300").unwrap(), ScanFilter::IncreasedBy(ScalarValue::Int(300)));
        assert_eq!(ScanFilter::parse("- 300").unwrap(), ScanFilter::DecreasedBy(ScalarValue::Int(300)));
        assert_eq!(ScanFilter::parse("-=300").unwrap(), ScanFilter::DecreasedBy(ScalarValue::Int(300)));
        assert_eq!(ScanFilter::parse("changed").unwrap(), ScanFilter::Changed);
        assert_eq!(ScanFilter::parse("?").unwrap(), ScanFilter::Unknown);
        assert_eq!(
            ScanFilter::parse("10..20").unwrap(),
            ScanFilter::Between(ScalarValue::Int(10), ScalarValue::Int(20))
        );
        assert!(ScanFilter::parse("abc").is_err());
    }

    #[test]
    fn big_endian_scan() {
        let mut b = vec![0u8; 16];
        b[4..8].copy_from_slice(&4242i32.to_be_bytes());
        let cfg = ScanConfig {
            types: vec![ScanType::I32],
            endians: vec![Endianness::Little, Endianness::Big],
            alignment: Alignment::Natural,
            ..ScanConfig::default()
        };
        let mut s = ScanSession::new(cfg);
        let n = s.first_scan(&Node::Bytes(b), &ScanFilter::Exact(ScalarValue::Int(4242)), "a").unwrap();
        assert_eq!(n, 1);
        assert_eq!(s.locations()[0].to_string(), "$@0x4:i32be");
    }

    #[test]
    fn ascii_decimal_raw_scan() {
        let b = b"{gold:1500,hp:20}".to_vec();
        let cfg = ScanConfig { types: vec![ScanType::AsciiDecimal], ..ScanConfig::default() };
        let mut s = ScanSession::new(cfg);
        let n = s.first_scan(&Node::Bytes(b), &ScanFilter::Exact(ScalarValue::Int(1500)), "a").unwrap();
        assert_eq!(n, 1);
        let mut b2 = Node::Bytes(b"{gold:1300,hp:20}".to_vec());
        let n = s.next_scan(&b2, &ScanFilter::DecreasedBy(ScalarValue::Int(200)), "b").unwrap();
        assert_eq!(n, 1);
        let loc = s.locations()[0].clone();
        loc.write(&mut b2, ScalarValue::Int(9999)).unwrap();
        assert_eq!(b2, Node::Bytes(b"{gold:9999,hp:20}".to_vec()));
    }
}
