//! View-model of the visual tree diff: per-node change status of both sides
//! (computed with [`savelens_core::diff`]), fuzzy search filtering and
//! cherry-pick previews.

use std::collections::{HashMap, HashSet};

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use savelens_core::{diff, Change, ChangeKind, NoAliases, Node, NodePath, SemanticDiff};

/// Change status of one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeStatus {
    Same,
    Added,
    Removed,
    Modified,
    /// A container with changes somewhere below.
    Contains,
}

/// Which tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// Both trees and their change annotations.
#[derive(Debug, Clone)]
pub struct TreeDiffModel {
    pub left: Node,
    pub right: Node,
    pub diff: SemanticDiff,
    pub lines: Vec<String>,
    left_status: HashMap<String, NodeStatus>,
    right_status: HashMap<String, NodeStatus>,
}

fn mark(map: &mut HashMap<String, NodeStatus>, path: &NodePath, status: NodeStatus) {
    map.insert(path.to_string(), status);
    let mut cur = path.parent();
    while let Some(p) = cur {
        let key = p.to_string();
        let entry = map.entry(key).or_insert(NodeStatus::Contains);
        if *entry == NodeStatus::Same {
            *entry = NodeStatus::Contains;
        }
        cur = p.parent();
    }
}

impl TreeDiffModel {
    /// Diffs `left` (older revision) against `right` (working copy).
    pub fn build(left: Node, right: Node) -> Self {
        let d = diff(&left, &right);
        let mut left_status = HashMap::new();
        let mut right_status = HashMap::new();
        for c in &d.changes {
            match &c.kind {
                ChangeKind::Added { .. } => mark(&mut right_status, &c.path, NodeStatus::Added),
                ChangeKind::Removed { .. } => mark(&mut left_status, &c.path, NodeStatus::Removed),
                ChangeKind::Modified { .. } | ChangeKind::TypeChanged { .. } | ChangeKind::BytesChanged { .. } => {
                    mark(&mut left_status, &c.path, NodeStatus::Modified);
                    mark(&mut right_status, &c.path, NodeStatus::Modified);
                }
            }
        }
        let lines = d.changelog(&NoAliases);
        TreeDiffModel { left, right, diff: d, lines, left_status, right_status }
    }

    /// Status of a node.
    pub fn status(&self, side: Side, path: &NodePath) -> NodeStatus {
        let map = match side {
            Side::Left => &self.left_status,
            Side::Right => &self.right_status,
        };
        map.get(&path.to_string()).copied().unwrap_or(NodeStatus::Same)
    }

    /// Number of changes.
    pub fn change_count(&self) -> usize {
        self.diff.changes.len()
    }

    /// Changelog lines that cherry-picking `paths` from the left tree would apply.
    pub fn preview_pick(&self, paths: &[NodePath]) -> Vec<String> {
        let mut out = Vec::new();
        for p in paths {
            if self.left.get(p).is_none() {
                out.push(format!("{p}: does not exist in the source revision"));
                continue;
            }
            if self.right.get(p).is_none() {
                out.push(format!("{p}: missing in the working copy (cannot be created by a replace)"));
                continue;
            }
            // Reverse direction: what changes in WORKING when it takes the left value.
            let changes: Vec<Change> =
                diff(&self.right, &self.left).changes.into_iter().filter(|c| p.is_prefix_of(&c.path)).collect();
            if changes.is_empty() {
                out.push(format!("{p}: already identical"));
            } else {
                let sub = SemanticDiff { changes, ..SemanticDiff::default() };
                out.extend(sub.changelog(&NoAliases));
            }
        }
        out
    }
}

/// Paths (as strings) that stay visible for a fuzzy `query`: every node whose
/// `path = value` text matches, plus all its ancestors. `None` = no filter.
pub fn visible_paths(root: &Node, query: &str) -> Option<HashSet<String>> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    let pattern = Pattern::parse(q, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let mut out = HashSet::new();
    root.walk(|path, node| {
        let text = if node.is_container() { path.to_string() } else { format!("{path} = {}", node.preview(64)) };
        if pattern.score(Utf32Str::new(&text, &mut buf), &mut matcher).is_some() {
            out.insert(path.to_string());
            let mut cur = path.parent();
            while let Some(p) = cur {
                out.insert(p.to_string());
                cur = p.parent();
            }
        }
    });
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(gold: i64, items: &[&str]) -> Node {
        Node::map_from([(
            "Player",
            Node::map_from([
                ("Gold", Node::I64(gold)),
                ("Inventory", Node::Array(items.iter().map(|s| Node::String((*s).into())).collect())),
            ]),
        )])
    }

    #[test]
    fn statuses_and_preview() {
        let m = TreeDiffModel::build(tree(1200, &["Sword", "Shield"]), tree(1500, &["Sword", "Shield"]));
        let gold = NodePath::parse("Player.Gold").unwrap();
        assert_eq!(m.status(Side::Left, &gold), NodeStatus::Modified);
        assert_eq!(m.status(Side::Right, &NodePath::parse("Player").unwrap()), NodeStatus::Contains);
        assert_eq!(m.status(Side::Right, &NodePath::parse("Player.Inventory").unwrap()), NodeStatus::Same);
        let pv = m.preview_pick(&[gold]);
        assert!(pv.iter().any(|l| l.contains("1500 -> 1200")), "{pv:?}");
        let pv = m.preview_pick(&[NodePath::parse("Player.Inventory").unwrap()]);
        assert!(pv[0].contains("already identical"));
    }

    #[test]
    fn fuzzy_filter_keeps_ancestors() {
        let t = tree(1200, &["Sword"]);
        let v = visible_paths(&t, "gold").unwrap();
        assert!(v.contains("Player.Gold"));
        assert!(v.contains("Player"));
        assert!(!v.contains("Player.Inventory"));
        assert!(visible_paths(&t, "  ").is_none());
    }
}
