//! Whole-stack layer attribution (`review.stack.attribution`).
//!
//! A whole-stack review shows one diff from the stack's base (the merge base of the trunk and
//! the top layer) to the top layer's head. To label hunks and to publish a comment to the right
//! pull request, each line of that diff is attributed to the layer that wrote it.
//!
//! The file is followed from the stack's base through each layer's head in order. Each step
//! applies the zero-context diff between consecutive heads (for a restacked stack, exactly the
//! layer's own diff): lines it adds are attributed to that layer, lines it removes stop being
//! tracked, and every other line keeps its origin while its position is mapped forward through
//! the step's hunks. At the top, every line originates either from the base (unchanged by the
//! stack) or from the last layer that wrote it; every base line was either kept or removed by
//! exactly one layer.
//!
//! Work is bounded per file: one blob read plus one `git diff -U0` per layer, limited to the
//! file's old and new paths, and at most [`MAX_ATTRIBUTION_LINES`] tracked lines. How many layers
//! a stack has is the client's choice (`[plugins.config."bcode.code_review".stack]`), not a limit
//! of this operation.

use super::{DataError, git};
use bcode_code_review_models::{
    ReviewStackAttributionRequest, ReviewStackAttributionResponse, ReviewStackLineAttribution,
};
use bcode_git::models::{DiffFile, DiffFileStatus};
use std::path::Path;

/// Maximum lines tracked for one file.
pub const MAX_ATTRIBUTION_LINES: usize = 2_000_000;

/// Where a line of the file at some layer head came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// Line `n` (1-based) of the file at the stack's base.
    Base(u32),
    /// Written by the layer at this index, as line `n` (1-based) of that layer's head.
    Layer(usize, u32),
}

/// The file as followed through the stack: its path at the current head (`None` while it does
/// not exist) and the origin of each of its lines.
struct Tracked {
    path: Option<String>,
    lines: Vec<Origin>,
}

/// Attribute one file of a resolved stack's whole-stack diff to its layers.
pub fn attribution(
    root: &Path,
    request: &ReviewStackAttributionRequest,
) -> Result<ReviewStackAttributionResponse, DataError> {
    let stack = &request.stack;
    if stack.layers.is_empty() {
        return Err(DataError::InvalidRequest(
            "the stack has no layers".to_string(),
        ));
    }
    git::validate_oid(&stack.base_oid)?;
    for layer in &stack.layers {
        git::validate_oid(&layer.head_oid)?;
    }
    git::validate_relative_path(&request.path)?;
    let base_path = request.old_path.as_deref().unwrap_or(&request.path);
    git::validate_relative_path(base_path)?;

    let mut response = ReviewStackAttributionResponse {
        path: request.path.clone(),
        new_ranges: Vec::new(),
        old_ranges: Vec::new(),
        binary: false,
    };
    let Some(mut tracked) = base_file(root, &stack.base_oid, base_path)? else {
        response.binary = true;
        return Ok(response);
    };
    let base_lines = tracked.lines.len();
    // Which layer removed each base line, and the line's number in the head below that layer.
    let mut removed_by: Vec<Option<(usize, u32)>> = vec![None; base_lines];

    let pathspecs: Vec<&str> = if base_path == request.path {
        vec![base_path]
    } else {
        vec![base_path, request.path.as_str()]
    };
    let mut previous = stack.base_oid.as_str();
    for (index, layer) in stack.layers.iter().enumerate() {
        let files = step_diff(root, previous, &layer.head_oid, &pathspecs)?;
        previous = &layer.head_oid;
        let Some(file) = step_file(&tracked, &files) else {
            continue;
        };
        if file.is_binary {
            response.binary = true;
            return Ok(response);
        }
        apply(&mut tracked, file, index, &mut removed_by)?;
        tracked.path = path_after(file);
    }

    let number = |index: usize| stack.layers[index].number;
    response.new_ranges = ranges(tracked.lines.iter().map(|origin| match origin {
        Origin::Layer(index, line) => Some((number(*index), *line)),
        Origin::Base(_) => None,
    }));
    response.old_ranges = ranges(
        removed_by
            .iter()
            .map(|removed| removed.map(|(index, line)| (number(index), line))),
    );
    Ok(response)
}

/// The file at the stack's base: absent (no lines), or its line origins. `None` when binary.
fn base_file(root: &Path, base: &str, path: &str) -> Result<Option<Tracked>, DataError> {
    let spec = format!("{base}:{path}");
    let Some(blob) = git::optional_output(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &spec,
        ],
    )?
    .map(|stdout| String::from_utf8_lossy(&stdout).trim().to_string())
    .filter(|oid| !oid.is_empty()) else {
        return Ok(Some(Tracked {
            path: None,
            lines: Vec::new(),
        }));
    };
    git::validate_oid(&blob)?;
    let content = git::output(root, &["cat-file", "blob", &blob])?;
    if content[..content.len().min(8_000)].contains(&0) {
        return Ok(None);
    }
    let count = line_count(&content);
    if count > MAX_ATTRIBUTION_LINES {
        return Err(DataError::TooLarge(format!(
            "{path} has {count} lines; attribution is limited to {MAX_ATTRIBUTION_LINES}"
        )));
    }
    let count = u32::try_from(count).unwrap_or(u32::MAX);
    Ok(Some(Tracked {
        path: Some(path.to_string()),
        lines: (1..=count).map(Origin::Base).collect(),
    }))
}

fn line_count(content: &[u8]) -> usize {
    // Splitting on newlines yields one more piece than there are newlines.
    let newlines = content.split(|byte| *byte == b'\n').count() - 1;
    newlines + usize::from(content.last().is_some_and(|byte| *byte != b'\n'))
}

/// The zero-context diff of the file's paths between two consecutive heads.
fn step_diff(
    root: &Path,
    from: &str,
    to: &str,
    pathspecs: &[&str],
) -> Result<Vec<DiffFile>, DataError> {
    let mut args = vec![
        "diff",
        "--unified=0",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--find-renames",
        from,
        to,
        "--",
    ];
    args.extend_from_slice(pathspecs);
    let output = git::output(root, &args)?;
    bcode_git::diff::parse_unified_diff(&String::from_utf8_lossy(&output))
        .map_err(|error| DataError::Git(format!("failed to parse diff: {error}")))
}

/// The diff entry that continues the tracked file: the entry leaving its current path, or, while
/// the file does not exist, the entry that adds it.
fn step_file<'a>(tracked: &Tracked, files: &'a [DiffFile]) -> Option<&'a DiffFile> {
    tracked.path.as_deref().map_or_else(
        || {
            files
                .iter()
                .find(|file| file.status == DiffFileStatus::Added)
        },
        |path| {
            files.iter().find(|file| {
                file.status != DiffFileStatus::Added && file.old_path.as_deref() == Some(path)
            })
        },
    )
}

/// The file's path after a step: none once deleted.
fn path_after(file: &DiffFile) -> Option<String> {
    if file.status == DiffFileStatus::Deleted {
        None
    } else {
        file.new_path.clone()
    }
}

/// Apply one step's hunks: keep untouched lines (shifted), drop removed lines (recording which
/// layer removed base lines, and where they were in the head below it), and insert added lines
/// attributed to `layer` at their line in its head.
fn apply(
    tracked: &mut Tracked,
    file: &DiffFile,
    layer: usize,
    removed_by: &mut [Option<(usize, u32)>],
) -> Result<(), DataError> {
    let inconsistent = || {
        DataError::Git(format!(
            "diff hunks do not match the tracked file {}",
            tracked.path.as_deref().unwrap_or("(new file)")
        ))
    };
    let old = &tracked.lines;
    let mut next = Vec::with_capacity(old.len());
    let mut cursor = 0usize;
    for hunk in &file.hunks {
        let old_start = usize::try_from(hunk.old_start).map_err(|_| inconsistent())?;
        let old_count = usize::try_from(hunk.old_count).map_err(|_| inconsistent())?;
        let new_count = usize::try_from(hunk.new_count).map_err(|_| inconsistent())?;
        // With zero context, an empty old side names the line after which lines are inserted.
        let keep_until = if old_count == 0 {
            old_start
        } else {
            old_start.checked_sub(1).ok_or_else(inconsistent)?
        };
        let removed_end = keep_until + old_count;
        if keep_until < cursor || removed_end > old.len() {
            return Err(inconsistent());
        }
        next.extend_from_slice(&old[cursor..keep_until]);
        for (offset, origin) in old[keep_until..removed_end].iter().enumerate() {
            if let Origin::Base(line) = origin
                && let Some(slot) = usize::try_from(*line)
                    .ok()
                    .and_then(|line| line.checked_sub(1))
                    .and_then(|index| removed_by.get_mut(index))
            {
                let below = u32::try_from(keep_until + offset + 1).unwrap_or(u32::MAX);
                slot.get_or_insert((layer, below));
            }
        }
        let first = next.len();
        next.extend((0..new_count).map(|offset| {
            Origin::Layer(layer, u32::try_from(first + offset + 1).unwrap_or(u32::MAX))
        }));
        if next.len() > MAX_ATTRIBUTION_LINES {
            return Err(DataError::TooLarge(format!(
                "attribution is limited to {MAX_ATTRIBUTION_LINES} lines"
            )));
        }
        cursor = removed_end;
    }
    next.extend_from_slice(&old[cursor..]);
    tracked.lines = next;
    Ok(())
}

/// Merge consecutive lines with the same layer (and consecutive lines in that layer's diff)
/// into inclusive 1-based ranges.
fn ranges(lines: impl Iterator<Item = Option<(u64, u32)>>) -> Vec<ReviewStackLineAttribution> {
    let mut ranges: Vec<ReviewStackLineAttribution> = Vec::new();
    for (index, origin) in lines.enumerate() {
        let Some((layer, layer_line)) = origin else {
            continue;
        };
        let line = u32::try_from(index + 1).unwrap_or(u32::MAX);
        match ranges.last_mut() {
            Some(last)
                if last.layer == layer
                    && last.end + 1 == line
                    && last.layer_line_of(last.end).map(|end| end + 1) == Some(layer_line) =>
            {
                last.end = line;
            }
            _ => ranges.push(ReviewStackLineAttribution {
                start: line,
                end: line,
                layer,
                layer_line: Some(layer_line),
            }),
        }
    }
    ranges
}
