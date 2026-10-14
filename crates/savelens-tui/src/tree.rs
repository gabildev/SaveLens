//! Flattened, collapsible view of a semantic tree and fuzzy search over it.

use std::collections::HashSet;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};
use savelens_core::{Node, NodeKind, NodePath, PathSeg};

/// One visible row of the tree.
#[derive(Debug, Clone, PartialEq)]
pub struct TreeRow {
    pub path: NodePath,
    pub depth: usize,
    pub label: String,
    pub kind: NodeKind,
    pub container: bool,
    pub expanded: bool,
    pub children: usize,
    pub preview: String,
}

/// Key of a path in the expanded set.
pub fn key(path: &NodePath) -> String {
    path.to_string()
}

fn seg_label(seg: Option<&PathSeg>) -> String {
    match seg {
        None => "$".to_string(),
        Some(PathSeg::Key(k)) => k.clone(),
        Some(PathSeg::Index(i)) => format!("[{i}]"),
    }
}

fn children(node: &Node) -> Vec<(PathSeg, &Node)> {
    match node {
        Node::Map(m) => m.iter().map(|(k, v)| (PathSeg::Key(k.clone()), v)).collect(),
        Node::Array(a) => a.iter().enumerate().map(|(i, v)| (PathSeg::Index(i), v)).collect(),
        _ => Vec::new(),
    }
}

/// Visible rows given the set of expanded paths.
pub fn flatten(root: &Node, expanded: &HashSet<String>) -> Vec<TreeRow> {
    let mut out = Vec::new();
    let mut path = NodePath::root();
    push_rows(root, &mut path, 0, expanded, &mut out);
    out
}

fn push_rows(node: &Node, path: &mut NodePath, depth: usize, expanded: &HashSet<String>, out: &mut Vec<TreeRow>) {
    let container = node.is_container();
    let is_open = container && expanded.contains(&key(path));
    out.push(TreeRow {
        path: path.clone(),
        depth,
        label: seg_label(path.last()),
        kind: node.kind(),
        container,
        expanded: is_open,
        children: node.child_count(),
        preview: node.preview(80),
    });
    if is_open {
        for (seg, child) in children(node) {
            path.push(seg);
            push_rows(child, path, depth + 1, expanded, out);
            path.pop();
        }
    }
}

/// Marks every ancestor of `path` (and the root) as expanded.
pub fn expand_to(path: &NodePath, expanded: &mut HashSet<String>) {
    let mut cur = path.parent();
    while let Some(p) = cur {
        expanded.insert(key(&p));
        cur = p.parent();
    }
    expanded.insert(key(&NodePath::root()));
}

/// Fuzzy search over every node path and leaf value. Results are ordered by
/// score (best first), ties in document order.
pub fn search(root: &Node, query: &str) -> Vec<NodePath> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let mut entries: Vec<(NodePath, String)> = Vec::new();
    let mut path = NodePath::root();
    collect(root, &mut path, &mut entries);
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let haystacks: Vec<Haystack> = entries.iter().enumerate().map(|(i, (_, s))| Haystack { idx: i, text: s }).collect();
    pattern.match_list(haystacks, &mut matcher).into_iter().map(|(h, _)| entries[h.idx].0.clone()).collect()
}

struct Haystack<'a> {
    idx: usize,
    text: &'a str,
}

impl AsRef<str> for Haystack<'_> {
    fn as_ref(&self) -> &str {
        self.text
    }
}

fn collect(node: &Node, path: &mut NodePath, out: &mut Vec<(NodePath, String)>) {
    if !path.is_root() {
        let text = if node.is_container() { path.to_string() } else { format!("{path} = {}", node.preview(60)) };
        out.push((path.clone(), text));
    }
    for (seg, child) in children(node) {
        path.push(seg);
        collect(child, path, out);
        path.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Node {
        Node::map_from([
            (
                "Player",
                Node::map_from([
                    ("Gold", Node::I64(1500)),
                    ("Inventory", Node::Array(vec![Node::String("Sword".into()), Node::String("Shield".into())])),
                ]),
            ),
            ("Version", Node::I32(3)),
        ])
    }

    #[test]
    fn flatten_respects_expansion() {
        let root = sample();
        let mut exp = HashSet::new();
        assert_eq!(flatten(&root, &exp).len(), 1);
        exp.insert("$".to_string());
        let rows = flatten(&root, &exp);
        assert_eq!(rows.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), vec!["$", "Player", "Version"]);
        exp.insert("Player".to_string());
        exp.insert("Player.Inventory".to_string());
        let rows = flatten(&root, &exp);
        assert_eq!(rows.len(), 7);
        assert_eq!(rows[4].label, "[0]");
        assert_eq!(rows[4].depth, 3);
        assert!(rows[3].expanded);
    }

    #[test]
    fn search_finds_paths_and_values() {
        let root = sample();
        let hits = search(&root, "gold");
        assert_eq!(hits.first().map(|p| p.to_string()), Some("Player.Gold".to_string()));
        let hits = search(&root, "Shield");
        assert_eq!(hits.first().map(|p| p.to_string()), Some("Player.Inventory[1]".to_string()));
        assert!(search(&root, "   ").is_empty());
        assert!(search(&root, "zzzzqqq").is_empty());
        let mut exp = HashSet::new();
        expand_to(&NodePath::parse("Player.Inventory[1]").unwrap(), &mut exp);
        assert!(exp.contains("$") && exp.contains("Player") && exp.contains("Player.Inventory"));
    }
}
