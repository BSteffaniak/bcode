//! Patch construction for staging, unstaging and discarding selected diff lines.
//!
//! A patch built here is applied with `git apply`, which validates context against the target,
//! so a stale selection fails safely instead of corrupting the index or worktree.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use bcode_git_models::{DiffFile, DiffLineId, DiffLineKind};

use crate::GitError;

/// How the patch will be applied relative to the diff it was built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchDirection {
    /// Apply forward: the diff's old side must match the target. Used to stage lines from an
    /// unstaged diff into the index.
    Forward,
    /// Apply in reverse (`git apply -R`): the diff's new side must match the target. Used to
    /// unstage lines from a staged diff or discard lines from the worktree.
    Reverse,
}

/// Build a patch containing only the selected changed lines of `file`.
///
/// Unselected changes are neutralised so that the side the patch is applied against stays
/// unchanged: for [`PatchDirection::Forward`] unselected removals become context and
/// unselected additions are dropped; for [`PatchDirection::Reverse`] the opposite.
///
/// Returns `Ok(None)` when the selection contains no changed lines.
///
/// # Errors
/// * [`GitError::Unsupported`] for binary files.
pub fn build_patch(
    file: &DiffFile,
    selection: &BTreeSet<DiffLineId>,
    direction: PatchDirection,
) -> Result<Option<String>, GitError> {
    if file.is_binary {
        return Err(GitError::Unsupported(
            "binary files cannot be partially staged".into(),
        ));
    }
    let mut body = String::new();
    // Running difference between the new and old side of preceding emitted hunks.
    let mut offset: i64 = 0;
    let mut any = false;

    for (hunk_index, hunk) in file.hunks.iter().enumerate() {
        let selected = |line: usize| {
            selection.contains(&DiffLineId {
                hunk: hunk_index,
                line,
            })
        };
        if !hunk
            .lines
            .iter()
            .enumerate()
            .any(|(index, line)| line.kind != DiffLineKind::Context && selected(index))
        {
            continue;
        }

        let mut out: Vec<(char, &str, bool)> = Vec::with_capacity(hunk.lines.len());
        for (index, line) in hunk.lines.iter().enumerate() {
            let keep = selected(index);
            let marker = match (line.kind, direction, keep) {
                (DiffLineKind::Added, _, true) => Some('+'),
                (DiffLineKind::Removed, _, true) => Some('-'),
                (DiffLineKind::Added, PatchDirection::Forward, false)
                | (DiffLineKind::Removed, PatchDirection::Reverse, false) => None,
                (DiffLineKind::Context, _, _)
                | (DiffLineKind::Removed, PatchDirection::Forward, false)
                | (DiffLineKind::Added, PatchDirection::Reverse, false) => Some(' '),
            };
            if let Some(marker) = marker {
                out.push((marker, line.content.as_str(), line.no_newline));
            }
        }
        fix_no_newline_context(&mut out);

        let old_count = out.iter().filter(|(marker, ..)| *marker != '+').count();
        let new_count = out.iter().filter(|(marker, ..)| *marker != '-').count();
        let (old_start, new_start) = match direction {
            PatchDirection::Forward => {
                let old_start = i64::from(hunk.old_start);
                (
                    old_start,
                    anchored_start(old_start + offset, old_count, new_count),
                )
            }
            PatchDirection::Reverse => {
                let new_start = i64::from(hunk.new_start);
                (
                    anchored_start(new_start - offset, new_count, old_count),
                    new_start,
                )
            }
        };
        let _ = writeln!(
            body,
            "@@ -{} +{} @@",
            range(old_start.max(0), old_count),
            range(new_start.max(0), new_count)
        );
        for (marker, content, no_newline) in &out {
            body.push(*marker);
            body.push_str(content);
            body.push('\n');
            if *no_newline {
                body.push_str("\\ No newline at end of file\n");
            }
        }
        offset += i64::try_from(new_count).unwrap_or_default()
            - i64::try_from(old_count).unwrap_or_default();
        any = true;
    }

    if !any {
        return Ok(None);
    }
    let mut patch = String::new();
    for line in &file.header {
        patch.push_str(line);
        patch.push('\n');
    }
    if !file.header.iter().any(|line| line.starts_with("--- ")) {
        // Mode-only or intent-to-add headers may omit file markers; hunks require them.
        let old = file
            .old_path
            .as_deref()
            .map_or_else(|| "/dev/null".to_owned(), |path| format!("a/{path}"));
        let new = file
            .new_path
            .as_deref()
            .map_or_else(|| "/dev/null".to_owned(), |path| format!("b/{path}"));
        let _ = writeln!(patch, "--- {old}\n+++ {new}");
    }
    patch.push_str(&body);
    Ok(Some(patch))
}

/// A `\ No newline` marker is only valid on the final line of each side. When a removed final
/// line is neutralised into context but more lines follow on the new side, it must instead be
/// expressed as removed-without-newline plus added-with-newline.
fn fix_no_newline_context(out: &mut Vec<(char, &str, bool)>) {
    let mut index = 0;
    while index < out.len() {
        let (marker, content, no_newline) = out[index];
        let followed_on_new_side = out[index + 1..].iter().any(|(next, ..)| *next != '-');
        let followed_on_old_side = out[index + 1..].iter().any(|(next, ..)| *next != '+');
        if marker == ' ' && no_newline && (followed_on_new_side || followed_on_old_side) {
            if followed_on_new_side && !followed_on_old_side {
                out[index] = ('-', content, true);
                out.insert(index + 1, ('+', content, false));
                index += 1;
            } else if followed_on_old_side && !followed_on_new_side {
                out[index] = ('-', content, false);
                out.insert(index + 1, ('+', content, true));
                index += 1;
            }
        }
        index += 1;
    }
}

/// Start line for the side derived from the anchored side. Empty ranges name the line before
/// the change, so moving between empty and nonempty ranges shifts the start by one.
const fn anchored_start(start: i64, anchored_count: usize, derived_count: usize) -> i64 {
    match (anchored_count == 0, derived_count == 0) {
        (true, false) => start + 1,
        (false, true) => start - 1,
        _ => start,
    }
}

fn range(start: i64, count: usize) -> String {
    if count == 1 {
        start.to_string()
    } else {
        format!("{start},{count}")
    }
}

/// Every changed line in a hunk, for whole-hunk selection.
#[must_use]
pub fn hunk_selection(file: &DiffFile, hunk: usize) -> BTreeSet<DiffLineId> {
    file.hunks.get(hunk).map_or_else(BTreeSet::new, |found| {
        found
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.kind != DiffLineKind::Context)
            .map(|(line, _)| DiffLineId { hunk, line })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse_unified_diff;

    fn file(diff: &str) -> DiffFile {
        parse_unified_diff(diff).unwrap().remove(0)
    }

    fn ids(pairs: &[(usize, usize)]) -> BTreeSet<DiffLineId> {
        pairs
            .iter()
            .map(|&(hunk, line)| DiffLineId { hunk, line })
            .collect()
    }

    const DIFF: &str = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -10,2 +10,3 @@\n x\n+y\n z\n";

    #[test]
    fn forward_patch_keeps_selected_addition_and_neutralises_removal() {
        let patch = build_patch(&file(DIFF), &ids(&[(0, 2)]), PatchDirection::Forward)
            .unwrap()
            .unwrap();
        assert!(
            patch.ends_with("@@ -1,3 +1,4 @@\n a\n b\n+B\n c\n"),
            "{patch}"
        );
    }

    #[test]
    fn reverse_patch_keeps_unselected_additions_as_context() {
        let patch = build_patch(&file(DIFF), &ids(&[(0, 1)]), PatchDirection::Reverse)
            .unwrap()
            .unwrap();
        assert!(
            patch.ends_with("@@ -1,4 +1,3 @@\n a\n-b\n B\n c\n"),
            "{patch}"
        );
    }

    #[test]
    fn later_hunks_are_offset_by_earlier_emitted_hunks() {
        let patch = build_patch(
            &file(DIFF),
            &ids(&[(0, 2), (1, 1)]),
            PatchDirection::Forward,
        )
        .unwrap()
        .unwrap();
        assert!(patch.contains("@@ -10,2 +11,3 @@"), "{patch}");
    }

    #[test]
    fn empty_and_context_only_selections_build_nothing() {
        assert_eq!(
            build_patch(&file(DIFF), &ids(&[(0, 0)]), PatchDirection::Forward).unwrap(),
            None
        );
    }

    #[test]
    fn neutralised_final_line_without_newline_is_rewritten() {
        let diff = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b\n\\ No newline at end of file\n";
        let patch = build_patch(&file(diff), &ids(&[(0, 1)]), PatchDirection::Forward)
            .unwrap()
            .unwrap();
        assert!(
            patch.ends_with(
                "@@ -1 +1,2 @@\n-a\n\\ No newline at end of file\n+a\n+b\n\\ No newline at end of file\n"
            ),
            "{patch}"
        );
    }
}
