//! Commit graph lanes for topologically ordered history.

use bcode_git::models::Commit;

/// One cell of the graph prefix for a commit row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cell {
    /// Empty lane.
    Empty,
    /// A lane passing through.
    Line(usize),
    /// The commit itself.
    Commit {
        /// Lane color index.
        lane: usize,
        /// Whether the commit has several parents.
        merge: bool,
    },
    /// A lane merging into the commit's lane from the right.
    MergeIn(usize),
    /// A lane branching out of the commit's lane to the right.
    BranchOut(usize),
}

/// Graph cells per commit, in input order. Input must be newest-first topological order.
#[must_use]
pub fn layout(commits: &[Commit]) -> Vec<Vec<Cell>> {
    // Each lane holds the commit id it is waiting for, plus a stable color id.
    let mut lanes: Vec<Option<(String, usize)>> = Vec::new();
    let mut next_color = 0usize;
    let mut rows = Vec::with_capacity(commits.len());

    for commit in commits {
        let mut color = || {
            next_color += 1;
            next_color - 1
        };
        let column = lanes
            .iter()
            .position(|lane| lane.as_ref().is_some_and(|(oid, _)| *oid == commit.oid))
            .unwrap_or_else(|| {
                let free = lanes.iter().position(Option::is_none);
                let lane = Some((commit.oid.clone(), color()));
                if let Some(index) = free {
                    lanes[index] = lane;
                    index
                } else {
                    lanes.push(lane);
                    lanes.len() - 1
                }
            });
        let lane_color = lanes[column].as_ref().map_or(0, |(_, color)| *color);

        let mut cells: Vec<Cell> = lanes
            .iter()
            .map(|lane| {
                lane.as_ref()
                    .map_or(Cell::Empty, |(_, color)| Cell::Line(*color))
            })
            .collect();
        cells[column] = Cell::Commit {
            lane: lane_color,
            merge: commit.is_merge(),
        };
        // Other lanes waiting for this commit converge here.
        for (index, lane) in lanes.iter_mut().enumerate() {
            if index != column
                && let Some((oid, color)) = lane
                && *oid == commit.oid
            {
                cells[index] = Cell::MergeIn(*color);
                *lane = None;
            }
        }

        match commit.parents.split_first() {
            Some((first, rest)) => {
                lanes[column] = Some((first.clone(), lane_color));
                for parent in rest {
                    if lanes
                        .iter()
                        .any(|lane| lane.as_ref().is_some_and(|(oid, _)| oid == parent))
                    {
                        continue;
                    }
                    let lane = Some((parent.clone(), color()));
                    let index = if let Some(free) = lanes.iter().position(Option::is_none) {
                        lanes[free] = lane;
                        free
                    } else {
                        lanes.push(lane);
                        lanes.len() - 1
                    };
                    let parent_color = lanes[index].as_ref().map_or(0, |(_, color)| *color);
                    if index >= cells.len() {
                        cells.resize(index + 1, Cell::Empty);
                    }
                    cells[index] = Cell::BranchOut(parent_color);
                }
            }
            None => lanes[column] = None,
        }
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
        while cells.last() == Some(&Cell::Empty) {
            cells.pop();
        }
        rows.push(cells);
    }
    rows
}

/// Glyph for a cell.
#[must_use]
pub const fn glyph(cell: Cell) -> &'static str {
    match cell {
        Cell::Empty => "  ",
        Cell::Line(_) => "│ ",
        Cell::Commit { merge: false, .. } => "● ",
        Cell::Commit { merge: true, .. } => "◉ ",
        Cell::MergeIn(_) => "┘ ",
        Cell::BranchOut(_) => "┐ ",
    }
}

/// Color index for a cell.
#[must_use]
pub const fn color(cell: Cell) -> Option<usize> {
    match cell {
        Cell::Empty => None,
        Cell::Line(color)
        | Cell::Commit { lane: color, .. }
        | Cell::MergeIn(color)
        | Cell::BranchOut(color) => Some(color),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(oid: &str, parents: &[&str]) -> Commit {
        Commit {
            oid: oid.into(),
            short_oid: oid.into(),
            parents: parents.iter().map(|parent| (*parent).into()).collect(),
            author_name: String::new(),
            author_email: String::new(),
            author_time: 0,
            commit_time: 0,
            refs: Vec::new(),
            summary: String::new(),
        }
    }

    #[test]
    fn linear_history_uses_one_lane() {
        let rows = layout(&[commit("c", &["b"]), commit("b", &["a"]), commit("a", &[])]);
        assert!(rows.iter().all(|row| row.len() == 1));
    }

    #[test]
    fn merges_branch_out_and_converge() {
        // m merges f into b; f and b share parent a.
        let rows = layout(&[
            commit("m", &["b", "f"]),
            commit("f", &["a"]),
            commit("b", &["a"]),
            commit("a", &[]),
        ]);
        assert!(matches!(rows[0][0], Cell::Commit { merge: true, .. }));
        assert!(matches!(rows[0][1], Cell::BranchOut(_)));
        assert!(matches!(rows[1][1], Cell::Commit { merge: false, .. }));
        assert!(matches!(rows[2][0], Cell::Commit { .. }));
        assert!(matches!(rows[3][0], Cell::Commit { .. }));
        assert!(rows[3].iter().any(|cell| matches!(cell, Cell::MergeIn(_))));
    }
}
