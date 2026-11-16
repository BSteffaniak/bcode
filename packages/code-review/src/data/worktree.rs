//! Uncommitted-change marks (`review.worktree.marks`): the lines of working-tree files that
//! differ from `HEAD`, staged or not.
//!
//! One request runs at most three git processes whatever its size: `rev-parse` for `HEAD`,
//! `ls-files` for which files are tracked, and one `git diff -U0 HEAD` for the tracked files
//! not already cached. Answers are cached per file by `HEAD`, the index, and the file's size and
//! modification time, in a bounded cache.

use super::paths::{index_stamp, working_tree_file};
use super::{DataError, git};
use crate::history::delta::{ZeroHunk, zero_hunk_sections};
use bcode_code_review_models::{
    REVIEW_WORKTREE_MARKS_MAX_BYTES, REVIEW_WORKTREE_MARKS_MAX_PATHS, RemovedSince,
    ReviewWorktreeFileMarks, ReviewWorktreeMarksRequest, ReviewWorktreeMarksResponse,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Files whose marks are kept.
const MAX_CACHED_FILES: usize = 1_024;

/// What a cached answer depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MarksKey {
    root: PathBuf,
    path: String,
    head: Option<String>,
    index: Option<(u64, SystemTime)>,
    size: u64,
    modified: Option<SystemTime>,
}

/// Bounded cache of per-file marks.
#[derive(Debug, Default)]
pub struct MarksCache {
    entries: VecDeque<(MarksKey, Arc<ReviewWorktreeFileMarks>)>,
}

impl MarksCache {
    fn get(&mut self, key: &MarksKey) -> Option<Arc<ReviewWorktreeFileMarks>> {
        let position = self.entries.iter().position(|(cached, _)| cached == key)?;
        let entry = self.entries.remove(position)?;
        let marks = Arc::clone(&entry.1);
        self.entries.push_back(entry);
        Some(marks)
    }

    fn insert(&mut self, key: MarksKey, marks: Arc<ReviewWorktreeFileMarks>) {
        self.entries
            .retain(|(cached, _)| cached.path != key.path || cached.root != key.root);
        if self.entries.len() >= MAX_CACHED_FILES {
            self.entries.pop_front();
        }
        self.entries.push_back((key, marks));
    }
}

/// Mark the requested files' lines changed since `HEAD`.
pub fn marks(
    cache: &Mutex<MarksCache>,
    root: &Path,
    request: &ReviewWorktreeMarksRequest,
) -> Result<ReviewWorktreeMarksResponse, DataError> {
    if request.paths.len() > REVIEW_WORKTREE_MARKS_MAX_PATHS {
        return Err(DataError::InvalidRequest(format!(
            "at most {REVIEW_WORKTREE_MARKS_MAX_PATHS} paths per request"
        )));
    }
    for path in &request.paths {
        git::validate_relative_path(path)?;
    }
    let head = git::resolve_commit(root, "HEAD")?;
    let index = index_stamp(root);
    let mut answers: BTreeMap<String, Arc<ReviewWorktreeFileMarks>> = BTreeMap::new();
    let mut wanted: Vec<(String, MarksKey)> = Vec::new();
    for path in &request.paths {
        if answers.contains_key(path) || wanted.iter().any(|(wanted, _)| wanted == path) {
            continue;
        }
        let Some(metadata) = working_tree_file(root, path) else {
            answers.insert(
                path.clone(),
                Arc::new(ReviewWorktreeFileMarks {
                    path: path.clone(),
                    missing: true,
                    ..ReviewWorktreeFileMarks::default()
                }),
            );
            continue;
        };
        if metadata.len() > REVIEW_WORKTREE_MARKS_MAX_BYTES {
            answers.insert(
                path.clone(),
                Arc::new(ReviewWorktreeFileMarks {
                    path: path.clone(),
                    too_large: true,
                    ..ReviewWorktreeFileMarks::default()
                }),
            );
            continue;
        }
        let key = MarksKey {
            root: root.to_path_buf(),
            path: path.clone(),
            head: head.clone(),
            index,
            size: metadata.len(),
            modified: metadata.modified().ok(),
        };
        let cached = lock(cache).get(&key);
        match cached {
            Some(marks) => {
                answers.insert(path.clone(), marks);
            }
            None => wanted.push((path.clone(), key)),
        }
    }
    if !wanted.is_empty() {
        let mut computed = compute(root, head.as_deref(), &wanted)?;
        let fresh: Vec<(MarksKey, Arc<ReviewWorktreeFileMarks>)> = wanted
            .into_iter()
            .map(|(path, key)| {
                let marks = computed
                    .remove(&path)
                    .unwrap_or_else(|| ReviewWorktreeFileMarks {
                        path,
                        ..ReviewWorktreeFileMarks::default()
                    });
                (key, Arc::new(marks))
            })
            .collect();
        for (key, marks) in &fresh {
            answers.insert(key.path.clone(), Arc::clone(marks));
        }
        let mut cache = lock(cache);
        for (key, marks) in fresh {
            cache.insert(key, marks);
        }
    }
    Ok(ReviewWorktreeMarksResponse {
        files: request
            .paths
            .iter()
            .filter_map(|path| answers.get(path).map(|marks| (**marks).clone()))
            .collect(),
    })
}

/// The marks of `wanted` files with git.
fn compute(
    root: &Path,
    head: Option<&str>,
    wanted: &[(String, MarksKey)],
) -> Result<BTreeMap<String, ReviewWorktreeFileMarks>, DataError> {
    let paths: Vec<&str> = wanted.iter().map(|(path, _)| path.as_str()).collect();
    let mut args = vec!["ls-files", "-z", "--"];
    args.extend(paths.iter().copied());
    let tracked: BTreeSet<String> = git::split_nul(&git::output(root, &args)?)
        .into_iter()
        .collect();
    let mut answers = BTreeMap::new();
    let mut diffed = Vec::new();
    for path in paths {
        if tracked.contains(path) {
            diffed.push(path);
        } else {
            answers.insert(
                path.to_owned(),
                ReviewWorktreeFileMarks {
                    path: path.to_owned(),
                    untracked: true,
                    ..ReviewWorktreeFileMarks::default()
                },
            );
        }
    }
    if diffed.is_empty() {
        return Ok(answers);
    }
    let base = match head {
        Some(head) => head.to_owned(),
        None => git::empty_tree_oid(root)?,
    };
    let mut args = vec![
        "diff",
        "-U0",
        "--no-renames",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        base.as_str(),
        "--",
    ];
    args.extend(diffed.iter().copied());
    let output = String::from_utf8_lossy(&git::output(root, &args)?).into_owned();
    let mut sections: BTreeMap<String, Vec<ZeroHunk>> =
        zero_hunk_sections(&output).into_iter().collect();
    for path in diffed {
        let hunks = sections.remove(path).unwrap_or_default();
        answers.insert(path.to_owned(), file_marks(path, &hunks));
    }
    Ok(answers)
}

/// Marks from one file's `HEAD` → working tree hunks: added and changed lines, the text a
/// changed line had at `HEAD`, and removed lines as a marker before the line they preceded.
fn file_marks(path: &str, hunks: &[ZeroHunk]) -> ReviewWorktreeFileMarks {
    let mut marks = ReviewWorktreeFileMarks {
        path: path.to_owned(),
        ..ReviewWorktreeFileMarks::default()
    };
    for hunk in hunks {
        marks.additions = marks.additions.saturating_add(hunk.new_count);
        marks.deletions = marks.deletions.saturating_add(hunk.old_count);
        if hunk.new_count > 0 {
            marks
                .added
                .push((hunk.new_start, hunk.new_start + hunk.new_count - 1));
        }
        let paired = usize::try_from(hunk.new_count)
            .unwrap_or(usize::MAX)
            .min(hunk.removed.len());
        for (offset, text) in hunk.removed.iter().take(paired).enumerate() {
            let line = hunk.new_start + u32::try_from(offset).unwrap_or(u32::MAX);
            marks.was.push((line, text.clone()));
        }
        if hunk.removed.len() > paired {
            marks.removed.push(RemovedSince {
                before_new_line: hunk.new_before() + 1 + hunk.new_count,
                lines: hunk.removed[paired..].to_vec(),
            });
        }
    }
    marks
}

fn lock(cache: &Mutex<MarksCache>) -> std::sync::MutexGuard<'_, MarksCache> {
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
