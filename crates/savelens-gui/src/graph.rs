//! Layout of the commit graph shown in the timeline: one lane per branch,
//! one row per commit (newest first) and the per-row line segments needed to
//! draw parent edges with rows of variable height.

use std::collections::HashMap;

use savelens_cas::{BranchRecord, CommitId, CommitRecord};

/// A commit placed in the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphNode {
    pub commit: CommitId,
    pub row: usize,
    pub lane: usize,
    pub parent: Option<CommitId>,
}

/// Edge from a commit (child) to its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphEdge {
    pub child_row: usize,
    pub child_lane: usize,
    pub parent_row: usize,
    pub parent_lane: usize,
}

/// Part of an edge drawn inside one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment {
    /// From the node of this row (at `from` lane) down to the bottom of the
    /// row at `to` lane (diagonal when the lanes differ).
    Leave { from: usize, to: usize },
    /// Full-height vertical line at `lane` (edge passing through the row).
    Through { lane: usize },
    /// From the top of the row down to the node of this row at `lane`.
    Arrive { lane: usize },
}

/// A branch label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchHead {
    pub name: String,
    pub row: usize,
    /// Lane of the head commit's node.
    pub lane: usize,
    pub is_current: bool,
}

/// The complete layout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphLayout {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// Branch name owning each lane.
    pub lanes: Vec<String>,
    pub heads: Vec<BranchHead>,
    /// Segments to draw in each row.
    pub row_segments: Vec<Vec<Segment>>,
}

impl GraphLayout {
    /// Lane of a branch.
    pub fn lane_of(&self, branch: &str) -> Option<usize> {
        self.lanes.iter().position(|l| l == branch)
    }

    /// Row of a commit.
    pub fn row_of(&self, commit: CommitId) -> Option<usize> {
        self.nodes.iter().position(|n| n.commit == commit)
    }

    pub fn lane_count(&self) -> usize {
        self.lanes.len().max(1)
    }

    /// Branch labels at a row.
    pub fn heads_at(&self, row: usize) -> impl Iterator<Item = &BranchHead> {
        self.heads.iter().filter(move |h| h.row == row)
    }
}

/// Computes the layout. `commits` may be in any order; rows are newest
/// (highest id) first. Lanes are ordered by the oldest commit of each branch,
/// so the branch history started on keeps lane 0.
pub fn layout(commits: &[CommitRecord], branches: &[BranchRecord], current_branch: &str) -> GraphLayout {
    let mut sorted: Vec<&CommitRecord> = commits.iter().collect();
    sorted.sort_by_key(|c| std::cmp::Reverse(c.id));

    // Lane order: by the oldest commit of each branch, then branches without commits.
    let mut first_seen: Vec<(CommitId, String)> = Vec::new();
    for c in sorted.iter().rev() {
        if !first_seen.iter().any(|(_, b)| *b == c.branch) {
            first_seen.push((c.id, c.branch.clone()));
        }
    }
    let mut lanes: Vec<String> = first_seen.into_iter().map(|(_, b)| b).collect();
    for b in branches {
        if !lanes.contains(&b.name) {
            lanes.push(b.name.clone());
        }
    }
    let lane_index: HashMap<&str, usize> = lanes.iter().enumerate().map(|(i, l)| (l.as_str(), i)).collect();

    let nodes: Vec<GraphNode> = sorted
        .iter()
        .enumerate()
        .map(|(row, c)| GraphNode {
            commit: c.id,
            row,
            lane: lane_index.get(c.branch.as_str()).copied().unwrap_or(0),
            parent: c.parent,
        })
        .collect();
    let row_by_id: HashMap<CommitId, usize> = nodes.iter().map(|n| (n.commit, n.row)).collect();

    let mut edges = Vec::new();
    let mut row_segments: Vec<Vec<Segment>> = vec![Vec::new(); nodes.len()];
    for n in &nodes {
        let Some(parent) = n.parent else { continue };
        let Some(&prow) = row_by_id.get(&parent) else { continue };
        if prow <= n.row {
            // Parents are always older (smaller id ➡ larger row); anything else is corrupt data.
            continue;
        }
        let plane = nodes[prow].lane;
        edges.push(GraphEdge { child_row: n.row, child_lane: n.lane, parent_row: prow, parent_lane: plane });
        row_segments[n.row].push(Segment::Leave { from: n.lane, to: plane });
        for segs in row_segments.iter_mut().take(prow).skip(n.row + 1) {
            if !segs.contains(&Segment::Through { lane: plane }) {
                segs.push(Segment::Through { lane: plane });
            }
        }
        if !row_segments[prow].contains(&Segment::Arrive { lane: plane }) {
            row_segments[prow].push(Segment::Arrive { lane: plane });
        }
    }

    let mut heads = Vec::new();
    for b in branches {
        let Some(head) = b.head else { continue };
        let Some(&row) = row_by_id.get(&head) else { continue };
        heads.push(BranchHead {
            name: b.name.clone(),
            row,
            lane: nodes[row].lane,
            is_current: b.name == current_branch,
        });
    }

    GraphLayout { nodes, edges, lanes, heads, row_segments }
}

#[cfg(test)]
mod tests {
    use super::*;
    use savelens_cas::CommitKind;

    fn c(id: i64, parent: Option<i64>, branch: &str) -> CommitRecord {
        CommitRecord {
            id,
            game_id: 1,
            parent,
            branch: branch.into(),
            kind: CommitKind::Manual,
            message: String::new(),
            created_at: id,
            summary: String::new(),
        }
    }

    #[test]
    fn branched_history() {
        // main: 1 - 2 - 4 ; exp: 2 - 3 - 5
        let commits = vec![
            c(1, None, "main"),
            c(2, Some(1), "main"),
            c(3, Some(2), "exp"),
            c(4, Some(2), "main"),
            c(5, Some(3), "exp"),
        ];
        let branches = vec![
            BranchRecord { name: "main".into(), head: Some(4), created_at: 0 },
            BranchRecord { name: "exp".into(), head: Some(5), created_at: 0 },
        ];
        let l = layout(&commits, &branches, "exp");
        assert_eq!(l.lanes, vec!["main".to_string(), "exp".to_string()]);
        assert_eq!(l.nodes.iter().map(|n| n.commit).collect::<Vec<_>>(), vec![5, 4, 3, 2, 1]);
        assert_eq!(l.edges.len(), 4);
        // 3 -> 2 crosses lanes; passes through row of 4? No: 3 is row 2, 2 is row 3: adjacent.
        let e32 = l.edges.iter().find(|e| e.child_row == 2).unwrap();
        assert_eq!((e32.child_lane, e32.parent_lane), (1, 0));
        // 4 (row 1) -> 2 (row 3) passes through row 2.
        assert!(l.row_segments[2].contains(&Segment::Through { lane: 0 }));
        let exp = l.heads.iter().find(|h| h.name == "exp").unwrap();
        assert!(exp.is_current);
        assert_eq!(exp.lane, 1);
        assert_eq!(exp.row, 0);
    }

    #[test]
    fn empty_and_orphans() {
        let l = layout(&[], &[], "main");
        assert!(l.nodes.is_empty());
        assert_eq!(l.lane_count(), 1);
        // A parent outside the set is ignored.
        let l = layout(&[c(7, Some(3), "main")], &[], "main");
        assert!(l.edges.is_empty());
    }
}
