//! Surgical cherry-pick: restore selected sub-trees of an older snapshot into
//! the current save while keeping everything else.

use serde::{Deserialize, Serialize};

use crate::diff::{diff, SemanticDiff};
use crate::error::{CoreError, CoreResult};
use crate::node::{Node, NodeKind};
use crate::path::{NodePath, PathSeg};

/// How a selected sub-tree is merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CherryPickMode {
    /// The destination sub-tree is replaced by the source sub-tree.
    Replace,
    /// Maps are merged recursively: source values win, keys only present in
    /// the destination are kept. Arrays and scalars are replaced.
    DeepMerge,
}

/// Cherry-pick options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CherryPickOptions {
    pub mode: CherryPickMode,
    /// Allow a node to change its type (e.g. `I32` → `String`). Off by default
    /// because most binary codecs cannot re-type a field.
    pub allow_type_change: bool,
    /// Allow structural changes (new/removed map keys, different array lengths).
    pub allow_structure_change: bool,
    /// Create the selected path in the destination when it does not exist
    /// (its parent must exist).
    pub create_missing: bool,
}

impl Default for CherryPickOptions {
    fn default() -> Self {
        CherryPickOptions {
            mode: CherryPickMode::Replace,
            allow_type_change: false,
            allow_structure_change: true,
            create_missing: false,
        }
    }
}

/// Outcome of a cherry-pick.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CherryPickReport {
    /// Paths that were applied.
    pub applied: Vec<NodePath>,
    /// Paths that were already identical.
    pub unchanged: Vec<NodePath>,
    /// Semantic diff between the old destination and the result.
    pub diff: SemanticDiff,
}

/// Checks that `src` can replace `dst` without changing leaf types (and,
/// unless allowed, without structural changes). Returns the first problem.
pub fn check_compatible(dst: &Node, src: &Node, opts: &CherryPickOptions, path: &mut NodePath) -> Result<(), (NodePath, String)> {
    let (dk, sk) = (dst.kind(), src.kind());
    if dk != sk {
        // Byte containers are interchangeable.
        let bytesish = |k: NodeKind| matches!(k, NodeKind::Bytes | NodeKind::RawRegion);
        if !(opts.allow_type_change || bytesish(dk) && bytesish(sk)) {
            return Err((path.clone(), format!("type differs: destination is {dk}, source is {sk}")));
        }
        return Ok(());
    }
    match (dst, src) {
        (Node::Map(dm), Node::Map(sm)) => {
            if !opts.allow_structure_change {
                if let Some(k) = sm.keys().find(|k| !dm.contains_key(*k)) {
                    return Err((path.key(k.clone()), "key does not exist in destination".into()));
                }
                if opts.mode == CherryPickMode::Replace {
                    if let Some(k) = dm.keys().find(|k| !sm.contains_key(*k)) {
                        return Err((path.key(k.clone()), "key would be removed from destination".into()));
                    }
                }
            }
            for (k, sv) in sm {
                if let Some(dv) = dm.get(k) {
                    path.push(PathSeg::Key(k.clone()));
                    check_compatible(dv, sv, opts, path)?;
                    path.pop();
                }
            }
            Ok(())
        }
        (Node::Array(da), Node::Array(sa)) => {
            if !opts.allow_structure_change && da.len() != sa.len() {
                return Err((
                    path.clone(),
                    format!("array length differs ({} vs {})", da.len(), sa.len()),
                ));
            }
            // Elements are checked against the destination element at the same
            // index, or against the first destination element (homogeneous arrays).
            for (i, sv) in sa.iter().enumerate() {
                let Some(dv) = da.get(i).or_else(|| da.first()) else { continue };
                path.push(PathSeg::Index(i));
                check_compatible(dv, sv, opts, path)?;
                path.pop();
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn deep_merge(dst: &mut Node, src: &Node) {
    match (dst, src) {
        (Node::Map(dm), Node::Map(sm)) => {
            for (k, sv) in sm {
                match dm.get_mut(k) {
                    Some(dv) => deep_merge(dv, sv),
                    None => {
                        dm.insert(k.clone(), sv.clone());
                    }
                }
            }
        }
        (d, s) => *d = s.clone(),
    }
}

/// Copies the sub-trees at `paths` from `source` into `dest`.
///
/// All paths are validated before anything is modified: either every path is
/// applied or `dest` is left untouched.
pub fn cherry_pick(dest: &mut Node, source: &Node, paths: &[NodePath], opts: &CherryPickOptions) -> CoreResult<CherryPickReport> {
    if paths.is_empty() {
        return Err(CoreError::CherryPick {
            path: "$".into(),
            reason: "no paths selected".into(),
        });
    }
    // Reject overlapping selections (ambiguous order).
    for (i, a) in paths.iter().enumerate() {
        for b in paths.iter().skip(i + 1) {
            if a.is_prefix_of(b) || b.is_prefix_of(a) {
                return Err(CoreError::CherryPick {
                    path: a.to_string(),
                    reason: format!("overlaps with selected path `{b}`"),
                });
            }
        }
    }
    let original = dest.clone();
    let mut work = dest.clone();
    let mut applied = Vec::new();
    let mut unchanged = Vec::new();
    for path in paths {
        let src = source.resolve(path).map_err(|e| CoreError::CherryPick {
            path: path.to_string(),
            reason: format!("not present in source snapshot: {e}"),
        })?;
        match work.get(path) {
            Some(dst) => {
                let mut p = path.clone();
                check_compatible(dst, src, opts, &mut p).map_err(|(at, reason)| CoreError::CherryPick {
                    path: at.to_string(),
                    reason,
                })?;
                if crate::merkle::bit_equal(dst, src) {
                    unchanged.push(path.clone());
                    continue;
                }
                let slot = work.resolve_mut(path)?;
                match opts.mode {
                    CherryPickMode::Replace => *slot = src.clone(),
                    CherryPickMode::DeepMerge => deep_merge(slot, src),
                }
                applied.push(path.clone());
            }
            None => {
                if !opts.create_missing {
                    return Err(CoreError::CherryPick {
                        path: path.to_string(),
                        reason: "path does not exist in destination (enable create_missing)".into(),
                    });
                }
                work.set(path, src.clone(), true).map_err(|e| CoreError::CherryPick {
                    path: path.to_string(),
                    reason: format!("cannot create: {e}"),
                })?;
                applied.push(path.clone());
            }
        }
    }
    let d = diff(&original, &work);
    *dest = work;
    Ok(CherryPickReport {
        applied,
        unchanged,
        diff: d,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save(gold: i32, items: &[&str], quest: &str) -> Node {
        Node::map_from([
            (
                "Player",
                Node::map_from([
                    ("Gold", Node::I32(gold)),
                    (
                        "Inventory",
                        Node::Array(items.iter().map(|s| Node::String(s.to_string())).collect()),
                    ),
                ]),
            ),
            ("Quests", Node::map_from([("Main", Node::String(quest.into()))])),
        ])
    }

    #[test]
    fn restores_only_selected_subtree() {
        let old = save(100, &["Sword", "Shield", "Potion"], "Active");
        let mut cur = save(900, &["Stick"], "Completed");
        let rep = cherry_pick(
            &mut cur,
            &old,
            &[NodePath::parse("Player.Inventory").unwrap()],
            &CherryPickOptions::default(),
        )
        .unwrap();
        assert_eq!(rep.applied.len(), 1);
        assert_eq!(cur, save(900, &["Sword", "Shield", "Potion"], "Completed"));
        assert!(!rep.diff.is_empty());
    }

    #[test]
    fn type_change_rejected_atomically() {
        let mut src = save(1, &[], "x");
        src.set(&NodePath::parse("Player.Gold").unwrap(), Node::String("lots".into()), false)
            .unwrap();
        let mut dst = save(2, &["a"], "y");
        let before = dst.clone();
        let err = cherry_pick(
            &mut dst,
            &src,
            &[NodePath::parse("Quests").unwrap(), NodePath::parse("Player").unwrap()],
            &CherryPickOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(err, CoreError::CherryPick { .. }));
        assert_eq!(dst, before, "destination must be untouched on failure");
    }

    #[test]
    fn deep_merge_keeps_destination_only_keys() {
        let src = Node::map_from([("S", Node::map_from([("a", Node::I32(1))]))]);
        let mut dst = Node::map_from([("S", Node::map_from([("a", Node::I32(5)), ("b", Node::I32(6))]))]);
        let opts = CherryPickOptions {
            mode: CherryPickMode::DeepMerge,
            ..CherryPickOptions::default()
        };
        cherry_pick(&mut dst, &src, &[NodePath::parse("S").unwrap()], &opts).unwrap();
        assert_eq!(dst, Node::map_from([("S", Node::map_from([("a", Node::I32(1)), ("b", Node::I32(6))]))]));
    }

    #[test]
    fn structure_change_can_be_forbidden() {
        let src = save(1, &["a", "b"], "x");
        let mut dst = save(1, &["a"], "x");
        let opts = CherryPickOptions {
            allow_structure_change: false,
            ..CherryPickOptions::default()
        };
        assert!(cherry_pick(&mut dst, &src, &[NodePath::parse("Player.Inventory").unwrap()], &opts).is_err());
    }

    #[test]
    fn overlapping_paths_rejected() {
        let src = save(1, &[], "x");
        let mut dst = save(2, &[], "x");
        let err = cherry_pick(
            &mut dst,
            &src,
            &[NodePath::parse("Player").unwrap(), NodePath::parse("Player.Gold").unwrap()],
            &CherryPickOptions::default(),
        );
        assert!(err.is_err());
    }

    #[test]
    fn missing_paths() {
        let src = Node::map_from([("New", Node::I32(1)), ("A", Node::I32(1))]);
        let mut dst = Node::map_from([("A", Node::I32(1))]);
        let p = NodePath::parse("New").unwrap();
        assert!(cherry_pick(&mut dst, &src, &[p.clone()], &CherryPickOptions::default()).is_err());
        let opts = CherryPickOptions {
            create_missing: true,
            ..CherryPickOptions::default()
        };
        cherry_pick(&mut dst, &src, &[p.clone()], &opts).unwrap();
        assert_eq!(dst.get(&p), Some(&Node::I32(1)));
        let rep = cherry_pick(&mut dst, &src, &[NodePath::parse("A").unwrap()], &opts).unwrap();
        assert_eq!(rep.unchanged.len(), 1);
        assert!(cherry_pick(&mut dst, &src, &[NodePath::parse("Nope").unwrap()], &opts).is_err());
    }
}
