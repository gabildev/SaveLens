//! Semantic diff engine.
//!
//! Compares two `Node` trees using their Merkle trees to skip identical
//! sub-trees, aligns arrays with an LCS over child hashes, and renders human
//! changelogs such as `Player.Gold: 1200 -> 1500 (+300)`.

use serde::{Deserialize, Serialize};

use crate::merkle::{MerkleTree, BYTES_BLOCK_SIZE};
use crate::node::{format_float, Node, ScalarValue};
use crate::path::{NodePath, PathSeg};

/// Diff tuning knobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffOptions {
    /// Maximum `n*m` cells for the LCS alignment of arrays; beyond it arrays
    /// are compared index by index.
    pub lcs_cell_budget: usize,
    /// Maximum byte ranges reported per byte blob.
    pub max_byte_ranges: usize,
    /// Changed byte runs separated by at most this many equal bytes are merged.
    pub byte_merge_gap: usize,
}

impl Default for DiffOptions {
    fn default() -> Self {
        DiffOptions { lcs_cell_budget: 1_000_000, max_byte_ranges: 256, byte_merge_gap: 3 }
    }
}

/// A contiguous changed range inside a byte blob.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ByteRange {
    pub offset: u64,
    #[serde(with = "crate::raw::serde_hex_bytes")]
    pub old: Vec<u8>,
    #[serde(with = "crate::raw::serde_hex_bytes")]
    pub new: Vec<u8>,
    /// For small changes, the enclosing aligned 32-bit little-endian word
    /// interpreted as `i32` before and after (`offset, old, new`).
    pub word_view: Option<(u64, i32, i32)>,
}

/// What happened to a node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum ChangeKind {
    Added {
        new: Node,
    },
    Removed {
        old: Node,
    },
    Modified {
        old: Node,
        new: Node,
    },
    TypeChanged {
        old: Node,
        new: Node,
    },
    BytesChanged {
        old_len: u64,
        new_len: u64,
        ranges: Vec<ByteRange>,
        /// True when more ranges changed than were reported.
        truncated: bool,
    },
}

/// One change at one path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub path: NodePath,
    pub kind: ChangeKind,
}

/// Counters of a diff.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffStats {
    pub added: usize,
    pub removed: usize,
    pub modified: usize,
    pub nodes_visited: usize,
    pub subtrees_skipped: usize,
}

/// Result of comparing two trees.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SemanticDiff {
    pub changes: Vec<Change>,
    pub stats: DiffStats,
}

/// Something that can map a path to a user alias (e.g. the cheat table).
pub trait AliasResolver {
    fn alias(&self, path: &NodePath) -> Option<String>;
}

/// Resolver that never returns aliases.
pub struct NoAliases;

impl AliasResolver for NoAliases {
    fn alias(&self, _path: &NodePath) -> Option<String> {
        None
    }
}

impl<F: Fn(&NodePath) -> Option<String>> AliasResolver for F {
    fn alias(&self, path: &NodePath) -> Option<String> {
        self(path)
    }
}

/// Diffs two trees with default options.
pub fn diff(old: &Node, new: &Node) -> SemanticDiff {
    diff_with(old, new, &DiffOptions::default())
}

/// Diffs two trees.
pub fn diff_with(old: &Node, new: &Node, opts: &DiffOptions) -> SemanticDiff {
    let ta = MerkleTree::build(old);
    let tb = MerkleTree::build(new);
    diff_trees(old, &ta, new, &tb, opts)
}

/// Diffs two trees whose Merkle trees were already computed (e.g. cached).
pub fn diff_trees(old: &Node, ta: &MerkleTree, new: &Node, tb: &MerkleTree, opts: &DiffOptions) -> SemanticDiff {
    let mut ctx = Ctx { opts, out: SemanticDiff::default() };
    let mut path = NodePath::root();
    ctx.node(old, ta, new, tb, &mut path);
    ctx.out
}

struct Ctx<'o> {
    opts: &'o DiffOptions,
    out: SemanticDiff,
}

impl Ctx<'_> {
    fn push(&mut self, path: &NodePath, kind: ChangeKind) {
        match &kind {
            ChangeKind::Added { .. } => self.out.stats.added += 1,
            ChangeKind::Removed { .. } => self.out.stats.removed += 1,
            _ => self.out.stats.modified += 1,
        }
        self.out.changes.push(Change { path: path.clone(), kind });
    }

    fn node(&mut self, a: &Node, ta: &MerkleTree, b: &Node, tb: &MerkleTree, path: &mut NodePath) {
        self.out.stats.nodes_visited += 1;
        if ta.hash == tb.hash {
            self.out.stats.subtrees_skipped += 1;
            return;
        }
        match (a, b) {
            (Node::Map(ma), Node::Map(mb)) => {
                for (i, (k, vb)) in mb.iter().enumerate() {
                    path.push(PathSeg::Key(k.clone()));
                    match ma.get_full(k) {
                        Some((j, _, va)) => self.node(va, &ta.children[j], vb, &tb.children[i], path),
                        None => self.push(path, ChangeKind::Added { new: vb.clone() }),
                    }
                    path.pop();
                }
                for (k, va) in ma.iter() {
                    if !mb.contains_key(k) {
                        path.push(PathSeg::Key(k.clone()));
                        self.push(path, ChangeKind::Removed { old: va.clone() });
                        path.pop();
                    }
                }
            }
            (Node::Array(xa), Node::Array(xb)) => self.array(xa, ta, xb, tb, path),
            (Node::Bytes(x), Node::Bytes(y)) => self.bytes(x, ta, y, tb, path),
            (Node::RawRegion(x), Node::RawRegion(y)) => self.bytes(&x.bytes, ta, &y.bytes, tb, path),
            (a, b) if a.kind() == b.kind() => self.push(path, ChangeKind::Modified { old: a.clone(), new: b.clone() }),
            (a, b) => self.push(path, ChangeKind::TypeChanged { old: a.clone(), new: b.clone() }),
        }
    }

    fn array(&mut self, a: &[Node], ta: &MerkleTree, b: &[Node], tb: &MerkleTree, path: &mut NodePath) {
        let ha: Vec<_> = ta.children.iter().map(|c| c.hash).collect();
        let hb: Vec<_> = tb.children.iter().map(|c| c.hash).collect();
        // Same length: index-wise comparison keeps paths stable (most edits are in place).
        if a.len() == b.len() {
            for i in 0..a.len() {
                path.push(PathSeg::Index(i));
                self.node(&a[i], &ta.children[i], &b[i], &tb.children[i], path);
                path.pop();
            }
            return;
        }
        let ops = align(&ha, &hb, self.opts.lcs_cell_budget);
        // Walk ops; pair runs of deletes with runs of inserts as modifications.
        let mut k = 0;
        while k < ops.len() {
            match ops[k] {
                AlignOp::Match(_, _) => k += 1,
                _ => {
                    let mut dels = Vec::new();
                    let mut ins = Vec::new();
                    while k < ops.len() {
                        match ops[k] {
                            AlignOp::Delete(i) => dels.push(i),
                            AlignOp::Insert(j) => ins.push(j),
                            AlignOp::Match(_, _) => break,
                        }
                        k += 1;
                    }
                    let paired = dels.len().min(ins.len());
                    for p in 0..paired {
                        let (i, j) = (dels[p], ins[p]);
                        path.push(PathSeg::Index(j));
                        self.node(&a[i], &ta.children[i], &b[j], &tb.children[j], path);
                        path.pop();
                    }
                    for &i in &dels[paired..] {
                        path.push(PathSeg::Index(i));
                        self.push(path, ChangeKind::Removed { old: a[i].clone() });
                        path.pop();
                    }
                    for &j in &ins[paired..] {
                        path.push(PathSeg::Index(j));
                        self.push(path, ChangeKind::Added { new: b[j].clone() });
                        path.pop();
                    }
                }
            }
        }
    }

    fn bytes(&mut self, a: &[u8], ta: &MerkleTree, b: &[u8], tb: &MerkleTree, path: &NodePath) {
        let mut ranges = Vec::new();
        let common = a.len().min(b.len());
        // Use block hashes to skip identical blocks when both blobs are blocked.
        let block_ok = !ta.children.is_empty() && !tb.children.is_empty();
        let mut start = 0usize;
        let mut truncated = false;
        while start < common {
            let end = (start + BYTES_BLOCK_SIZE).min(common);
            let bi = start / BYTES_BLOCK_SIZE;
            let same_block = block_ok
                && end - start == BYTES_BLOCK_SIZE
                && ta.children.get(bi).map(|c| c.hash) == tb.children.get(bi).map(|c| c.hash);
            if !same_block {
                collect_ranges(a, b, start, end, self.opts.byte_merge_gap, &mut ranges);
            }
            if ranges.len() > self.opts.max_byte_ranges {
                ranges.truncate(self.opts.max_byte_ranges);
                truncated = true;
                break;
            }
            start = end;
        }
        // Merge ranges that touch across block boundaries.
        let ranges = merge_adjacent(ranges, self.opts.byte_merge_gap);
        let ranges = ranges.into_iter().map(|(s, e)| make_range(a, b, s, e)).collect();
        self.push(
            path,
            ChangeKind::BytesChanged { old_len: a.len() as u64, new_len: b.len() as u64, ranges, truncated },
        );
    }
}

fn collect_ranges(a: &[u8], b: &[u8], start: usize, end: usize, gap: usize, out: &mut Vec<(usize, usize)>) {
    let mut i = start;
    while i < end {
        if a[i] != b[i] {
            let s = i;
            let mut e = i + 1;
            let mut j = e;
            while j < end {
                if a[j] != b[j] {
                    e = j + 1;
                    j += 1;
                } else if j - e < gap {
                    j += 1;
                } else {
                    break;
                }
            }
            out.push((s, e));
            i = e;
        } else {
            i += 1;
        }
    }
}

fn merge_adjacent(ranges: Vec<(usize, usize)>, gap: usize) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (s, e) in ranges {
        if let Some(last) = out.last_mut() {
            if s <= last.1 + gap {
                last.1 = last.1.max(e);
                continue;
            }
        }
        out.push((s, e));
    }
    out
}

fn make_range(a: &[u8], b: &[u8], s: usize, e: usize) -> ByteRange {
    let word_view = if e - s <= 4 {
        let ws = s - s % 4;
        let we = ws + 4;
        if e <= we && we <= a.len() && we <= b.len() {
            let wa = i32::from_le_bytes([a[ws], a[ws + 1], a[ws + 2], a[ws + 3]]);
            let wb = i32::from_le_bytes([b[ws], b[ws + 1], b[ws + 2], b[ws + 3]]);
            Some((ws as u64, wa, wb))
        } else {
            None
        }
    } else {
        None
    };
    ByteRange { offset: s as u64, old: a[s..e].to_vec(), new: b[s..e].to_vec(), word_view }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AlignOp {
    Match(usize, usize),
    Delete(usize),
    Insert(usize),
}

/// Aligns two hash sequences: common prefix/suffix, then LCS within budget,
/// otherwise positional pairing.
fn align<T: PartialEq>(a: &[T], b: &[T], budget: usize) -> Vec<AlignOp> {
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf] {
        suf += 1;
    }
    let mut ops: Vec<AlignOp> = (0..pre).map(|i| AlignOp::Match(i, i)).collect();
    let ma = &a[pre..a.len() - suf];
    let mb = &b[pre..b.len() - suf];
    let (n, m) = (ma.len(), mb.len());
    if n.saturating_mul(m) <= budget && n > 0 && m > 0 {
        // Classic LCS dynamic programming over the middle section.
        let w = m + 1;
        let mut dp = vec![0u32; (n + 1) * (m + 1)];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                dp[i * w + j] = if ma[i] == mb[j] {
                    dp[(i + 1) * w + j + 1] + 1
                } else {
                    dp[(i + 1) * w + j].max(dp[i * w + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n && j < m {
            if ma[i] == mb[j] {
                ops.push(AlignOp::Match(pre + i, pre + j));
                i += 1;
                j += 1;
            } else if dp[(i + 1) * w + j] >= dp[i * w + j + 1] {
                ops.push(AlignOp::Delete(pre + i));
                i += 1;
            } else {
                ops.push(AlignOp::Insert(pre + j));
                j += 1;
            }
        }
        while i < n {
            ops.push(AlignOp::Delete(pre + i));
            i += 1;
        }
        while j < m {
            ops.push(AlignOp::Insert(pre + j));
            j += 1;
        }
    } else {
        // Positional: deletes of the extra old items / inserts of new ones,
        // paired by position for the overlapping part.
        for i in 0..n {
            ops.push(AlignOp::Delete(pre + i));
        }
        for j in 0..m {
            ops.push(AlignOp::Insert(pre + j));
        }
    }
    for k in 0..suf {
        ops.push(AlignOp::Match(a.len() - suf + k, b.len() - suf + k));
    }
    ops
}

/// Formats a numeric delta with an explicit sign (`+300`, `-12.5`).
pub fn format_delta(delta: ScalarValue) -> String {
    match delta {
        ScalarValue::Int(i) if i >= 0 => format!("+{i}"),
        ScalarValue::Int(i) => format!("{i}"),
        ScalarValue::Float(f) => {
            let rounded = (f * 1e6).round() / 1e6;
            let s = if rounded.fract() == 0.0 && rounded.abs() < 1e15 {
                format!("{}", rounded as i64)
            } else {
                let t = format!("{rounded:.6}");
                t.trim_end_matches('0').trim_end_matches('.').to_string()
            };
            if rounded >= 0.0 {
                format!("+{s}")
            } else {
                s
            }
        }
    }
}

fn scalar_text(n: &Node) -> String {
    match n {
        Node::F32(v) => crate::node::format_f32(*v),
        Node::F64(v) => format_float(*v),
        other => other.preview(60),
    }
}

impl Change {
    /// Label for this change: the user alias, or the full path.
    fn label(&self, aliases: &dyn AliasResolver) -> String {
        aliases.alias(&self.path).unwrap_or_else(|| self.path.to_string())
    }

    /// Numeric delta for modified numeric leaves.
    pub fn numeric_delta(&self) -> Option<ScalarValue> {
        if let ChangeKind::Modified { old, new } = &self.kind {
            let (o, n) = (old.as_scalar_lenient()?, new.as_scalar_lenient()?);
            if o.is_nan() || n.is_nan() {
                return None;
            }
            return Some(n.delta(o));
        }
        None
    }

    /// Full one-line description (`Player.Gold: 1200 -> 1500 (+300)`).
    pub fn describe(&self) -> String {
        self.describe_with(&NoAliases)
    }

    /// Like [`Change::describe`] but using user aliases.
    pub fn describe_with(&self, aliases: &dyn AliasResolver) -> String {
        let label = self.label(aliases);
        match &self.kind {
            ChangeKind::Added { new } => format!("+ {label} = {}", new.preview(60)),
            ChangeKind::Removed { old } => format!("- {label} (was {})", old.preview(60)),
            ChangeKind::Modified { old, new } => match self.numeric_delta() {
                Some(d) => {
                    format!("{label}: {} -> {} ({})", scalar_text(old), scalar_text(new), format_delta(d))
                }
                _ => format!("{label}: {} -> {}", old.preview(60), new.preview(60)),
            },
            ChangeKind::TypeChanged { old, new } => {
                format!("{label}: {} ({}) -> {} ({})", old.preview(40), old.kind(), new.preview(40), new.kind())
            }
            ChangeKind::BytesChanged { old_len, new_len, ranges, truncated } => {
                let mut s = format!("{label}: {} byte range(s) changed", ranges.len());
                if *truncated {
                    s.push_str(" (truncated)");
                }
                if old_len != new_len {
                    s.push_str(&format!(", size {old_len} -> {new_len}"));
                }
                for r in ranges.iter().take(4) {
                    match r.word_view {
                        Some((off, o, n)) => s.push_str(&format!(
                            "; @{off:#06x} i32 {o} -> {n} ({})",
                            format_delta(ScalarValue::Int(n as i128 - o as i128))
                        )),
                        None => s.push_str(&format!("; @{:#06x} {} bytes", r.offset, r.old.len().max(r.new.len()))),
                    }
                }
                s
            }
        }
    }

    /// Compact effect for summaries (`+500 Gold`, `Main_02 -> Completed`).
    pub fn short_effect(&self, aliases: &dyn AliasResolver) -> String {
        let label = aliases.alias(&self.path).unwrap_or_else(|| self.path.leaf_label());
        match &self.kind {
            ChangeKind::Modified { new, .. } => match self.numeric_delta() {
                Some(d) => format!("{} {label}", format_delta(d)),
                None => format!("{label} -> {}", new.preview(24)),
            },
            ChangeKind::Added { .. } => format!("+{label}"),
            ChangeKind::Removed { .. } => format!("-{label}"),
            ChangeKind::TypeChanged { new, .. } => format!("{label} -> {}", new.preview(24)),
            ChangeKind::BytesChanged { ranges, .. } => {
                format!("{label} ~{} bytes", ranges.iter().map(|r| r.new.len()).sum::<usize>())
            }
        }
    }
}

impl SemanticDiff {
    /// True when nothing changed.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Number of changes.
    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// Full changelog, one line per change.
    pub fn changelog(&self, aliases: &dyn AliasResolver) -> Vec<String> {
        self.changes.iter().map(|c| c.describe_with(aliases)).collect()
    }

    /// One-line summary (`+500 Gold, +1 Level, Main_02 -> "Completed" (+3 more)`).
    ///
    /// Aliased changes are listed first, then numeric changes, then the rest.
    pub fn summary(&self, aliases: &dyn AliasResolver, max_items: usize) -> String {
        if self.changes.is_empty() {
            return "no changes".to_string();
        }
        let mut ranked: Vec<(u8, &Change)> = self
            .changes
            .iter()
            .map(|c| {
                let rank = if aliases.alias(&c.path).is_some() {
                    0
                } else if c.numeric_delta().is_some() {
                    1
                } else {
                    2
                };
                (rank, c)
            })
            .collect();
        ranked.sort_by_key(|(r, _)| *r);
        let shown: Vec<String> = ranked.iter().take(max_items.max(1)).map(|(_, c)| c.short_effect(aliases)).collect();
        let rest = self.changes.len().saturating_sub(shown.len());
        if rest > 0 {
            format!("{} (+{rest} more)", shown.join(", "))
        } else {
            shown.join(", ")
        }
    }

    /// Changes whose path is inside `prefix`.
    pub fn under<'a>(&'a self, prefix: &'a NodePath) -> impl Iterator<Item = &'a Change> + 'a {
        self.changes.iter().filter(move |c| prefix.is_prefix_of(&c.path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::RawRegion;

    fn save(gold: i32, quest: &str, items: &[&str]) -> Node {
        Node::map_from([
            (
                "Player",
                Node::map_from([
                    ("Gold", Node::I32(gold)),
                    ("Level", Node::U8(3)),
                    ("Inventory", Node::Array(items.iter().map(|s| Node::String(s.to_string())).collect())),
                ]),
            ),
            ("Quests", Node::map_from([("Main_02", Node::String(quest.into()))])),
        ])
    }

    #[test]
    fn identical_is_empty() {
        let a = save(1, "Active", &["a"]);
        let d = diff(&a, &a.clone());
        assert!(d.is_empty());
        assert_eq!(d.stats.subtrees_skipped, 1);
    }

    #[test]
    fn numeric_and_string_changes() {
        let a = save(1200, "Active", &["Sword"]);
        let b = save(1500, "Completed", &["Sword"]);
        let d = diff(&a, &b);
        let log = d.changelog(&NoAliases);
        assert_eq!(
            log,
            vec![
                "Player.Gold: 1200 -> 1500 (+300)".to_string(),
                "Quests.Main_02: \"Active\" -> \"Completed\"".to_string(),
            ]
        );
        let alias = |p: &NodePath| (p.to_string() == "Player.Gold").then(|| "Oro".to_string());
        assert_eq!(d.summary(&alias, 5), "+300 Oro, Main_02 -> \"Completed\"");
    }

    #[test]
    fn array_insert_detected_with_lcs() {
        let a = save(1, "x", &["A", "B", "C", "D"]);
        let b = save(1, "x", &["A", "X", "B", "C", "D"]);
        let d = diff(&a, &b);
        assert_eq!(d.changes.len(), 1);
        assert_eq!(d.changes[0].path.to_string(), "Player.Inventory[1]");
        assert!(matches!(d.changes[0].kind, ChangeKind::Added { .. }));
    }

    #[test]
    fn array_remove_and_modify() {
        let a = save(1, "x", &["A", "B", "C"]);
        let b = save(1, "x", &["A", "Z"]);
        let d = diff(&a, &b);
        // B -> Z modification paired, C removed.
        assert_eq!(d.stats.modified, 1);
        assert_eq!(d.stats.removed, 1);
    }

    #[test]
    fn map_key_added_removed() {
        let a = Node::map_from([("a", Node::I32(1)), ("b", Node::I32(2))]);
        let b = Node::map_from([("a", Node::I32(1)), ("c", Node::I32(3))]);
        let d = diff(&a, &b);
        let log = d.changelog(&NoAliases);
        assert!(log.contains(&"+ c = 3".to_string()));
        assert!(log.contains(&"- b (was 2)".to_string()));
    }

    #[test]
    fn type_change() {
        let a = Node::map_from([("a", Node::I32(1))]);
        let b = Node::map_from([("a", Node::String("1".into()))]);
        let d = diff(&a, &b);
        assert!(matches!(d.changes[0].kind, ChangeKind::TypeChanged { .. }));
    }

    #[test]
    fn float_delta_formatting() {
        assert_eq!(format_delta(ScalarValue::Float(0.5)), "+0.5");
        assert_eq!(format_delta(ScalarValue::Float(-2.0)), "-2");
        assert_eq!(format_delta(ScalarValue::Int(0)), "+0");
        let a = Node::map_from([("hp", Node::F32(100.0))]);
        let b = Node::map_from([("hp", Node::F32(87.5))]);
        assert_eq!(diff(&a, &b).changelog(&NoAliases)[0], "hp: 100.0 -> 87.5 (-12.5)");
    }

    #[test]
    fn raw_bytes_ranges_with_word_view() {
        let mut bytes = vec![0u8; 10_000];
        bytes[4096 + 8..4096 + 12].copy_from_slice(&1200i32.to_le_bytes());
        let mut changed = bytes.clone();
        changed[4096 + 8..4096 + 12].copy_from_slice(&1500i32.to_le_bytes());
        changed[9000] = 1;
        let a = Node::RawRegion(RawRegion::new(bytes));
        let b = Node::RawRegion(RawRegion::new(changed));
        let d = diff(&a, &b);
        assert_eq!(d.changes.len(), 1);
        match &d.changes[0].kind {
            ChangeKind::BytesChanged { ranges, .. } => {
                assert_eq!(ranges.len(), 2);
                assert_eq!(ranges[0].word_view, Some((4104, 1200, 1500)));
                assert_eq!(ranges[1].offset, 9000);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(d.changes[0].describe().contains("i32 1200 -> 1500 (+300)"));
    }

    #[test]
    fn bytes_size_change() {
        let a = Node::Bytes(vec![1, 2, 3]);
        let b = Node::Bytes(vec![1, 9, 3, 4]);
        let d = diff(&a, &b);
        let s = d.changes[0].describe();
        assert!(s.contains("size 3 -> 4"), "{s}");
    }

    #[test]
    fn align_positional_when_over_budget() {
        let a = [1, 2, 3];
        let b = [4, 5];
        let ops = align(&a, &b, 1);
        assert_eq!(ops.iter().filter(|o| matches!(o, AlignOp::Delete(_))).count(), 3);
        assert_eq!(ops.iter().filter(|o| matches!(o, AlignOp::Insert(_))).count(), 2);
    }

    #[test]
    fn summary_truncates() {
        let a = Node::map_from((0..10).map(|i| (format!("k{i}"), Node::I32(i))));
        let b = Node::map_from((0..10).map(|i| (format!("k{i}"), Node::I32(i + 1))));
        let s = diff(&a, &b).summary(&NoAliases, 3);
        assert_eq!(s, "+1 k0, +1 k1, +1 k2 (+7 more)");
    }
}
