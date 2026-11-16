//! Browse listings of selections: files, folders, globs, and the repository as they are now.
//!
//! A selection lists, with its selectors' pathspecs and each exclusion's:
//!
//! 1. tracked files (`git ls-files --stage`) that are in the working tree (not `--deleted`);
//! 2. with untracked files, those that are not ignored (`--others --exclude-standard`);
//! 3. with ignored files, those too (`--others --ignored --exclude-standard`);
//! 4. every file a selector names directly that none of these found (an ignored file), when it
//!    is a regular file or link inside the repository.
//!
//! A nested repository that is not a submodule (an untracked clone, a vendored checkout) is
//! printed by `--others` as its folder with a trailing `/`: it is not a file and is left out, as
//! the path index leaves it out.
//!
//! Each git listing is sorted, so the first `max_files` paths of the merge come from the first
//! `max_files` of each: the rest is counted, never held.

use super::{IndexListing, MODE_SYMLINK, browse_entry, git, stage_entry, totals};
use crate::data::{DataError, fingerprint};
use bcode_code_review_models::{
    BrowseSelection, BrowseUntracked, PathSelector, REVIEW_FILES_MAX_FILES_LIMIT, ReviewIndexEntry,
    ReviewIndexFlag,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

/// List `selection` in the working tree of `root`.
pub(super) fn listing(
    root: &Path,
    source_id: &str,
    selection: &BrowseSelection,
    key: &str,
) -> Result<Arc<IndexListing>, DataError> {
    let keep = usize::try_from(selection.max_files).unwrap_or(usize::MAX);
    let mut pathspecs: Vec<String> = selection
        .include
        .iter()
        .map(PathSelector::pathspec)
        .collect();
    pathspecs.extend(selection.exclude.iter().map(PathSelector::exclude_pathspec));
    let run_keeping = |options: &[&str], keep: usize| {
        let mut args = vec!["ls-files", "-z"];
        args.extend(options);
        args.push("--");
        args.extend(pathspecs.iter().map(String::as_str));
        git::limited_file_records(git::magic_command(root, &args), keep)
    };
    let run = |options: &[&str]| run_keeping(options, keep);
    // Tracked files deleted from the working tree are not there now: left out.
    let deleted = run_keeping(
        &["--deleted"],
        usize::try_from(REVIEW_FILES_MAX_FILES_LIMIT).unwrap_or(usize::MAX),
    )?;
    let gone: BTreeSet<&str> = deleted.records.iter().map(String::as_str).collect();
    let tracked = run_keeping(&["--stage"], keep.saturating_add(gone.len()))?;
    let untracked = if selection.untracked == BrowseUntracked::Tracked {
        git::LimitedRecords::default()
    } else {
        run(&["--others", "--exclude-standard"])?
    };
    let ignored = if selection.untracked == BrowseUntracked::All {
        run(&["--others", "--ignored", "--exclude-standard"])?
    } else {
        git::LimitedRecords::default()
    };
    let mut entries: BTreeMap<String, ReviewIndexEntry> = BTreeMap::new();
    for record in &tracked.records {
        if let Some(entry) = stage_entry(source_id, record)
            && !gone.contains(entry.path.as_str())
        {
            entries.entry(entry.path.clone()).or_insert(entry);
        }
    }
    for (records, flag) in [
        (&untracked.records, ReviewIndexFlag::Untracked),
        (&ignored.records, ReviewIndexFlag::Ignored),
    ] {
        for path in records {
            if !entries.contains_key(path) {
                let mode = working_tree_mode(root, path);
                entries.insert(
                    path.clone(),
                    browse_entry(source_id, path, mode, Some(flag)),
                );
            }
        }
    }
    // A conflicted file is staged more than once and deleted once per stage: close enough for
    // a count.
    let total = tracked.total.saturating_sub(deleted.total)
        + untracked.total
        + ignored.total
        + add_named_files(root, source_id, selection, &mut entries);
    let total = u32::try_from(total).unwrap_or(u32::MAX);
    let mut entries: Vec<ReviewIndexEntry> = entries.into_values().collect();
    let cut = [&tracked, &untracked, &ignored, &deleted]
        .iter()
        .any(|listing| listing.total > listing.records.len() as u64);
    let untruncated = (cut || entries.len() > keep).then_some(total);
    entries.truncate(keep);
    let (additions, deletions) = totals(&entries);
    let listed: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    let listed = listed.join("\0");
    Ok(Arc::new(IndexListing {
        fingerprint: fingerprint(&[
            key.as_bytes(),
            &tracked.digest,
            &deleted.digest,
            &untracked.digest,
            &ignored.digest,
            listed.as_bytes(),
        ]),
        entries,
        additions,
        deletions,
        untruncated,
    }))
}

/// Add every file a selector of `selection` names directly that `entries` lacks (an ignored
/// file: a file named directly is reviewed whether or not git ignores it). Returns how many.
fn add_named_files(
    root: &Path,
    source_id: &str,
    selection: &BrowseSelection,
    entries: &mut BTreeMap<String, ReviewIndexEntry>,
) -> u64 {
    let mut added = 0;
    for path in selection
        .include
        .iter()
        .filter(|selector| selector.is_explicit_file())
        .filter_map(PathSelector::path)
    {
        let excluded = selection
            .exclude
            .iter()
            .any(|excluded| covers(excluded, path));
        if entries.contains_key(path)
            || excluded
            || crate::data::paths::working_tree_file(root, path).is_none()
        {
            continue;
        }
        let mode = working_tree_mode(root, path);
        entries.insert(
            path.to_owned(),
            browse_entry(source_id, path, mode, Some(ReviewIndexFlag::Ignored)),
        );
        added += 1;
    }
    added
}

/// Whether the exclusion `excluded` (a file or folder) covers `path`.
fn covers(excluded: &PathSelector, path: &str) -> bool {
    match excluded {
        PathSelector::File { path: file } => file == path,
        PathSelector::Folder { path: folder } => path
            .strip_prefix(folder.as_str())
            .is_some_and(|rest| rest.starts_with('/')),
        PathSelector::FileRange { .. } | PathSelector::Glob { .. } => false,
    }
}

/// The git mode of a working-tree file: a link, an executable, or a regular file.
fn working_tree_mode(root: &Path, path: &str) -> &'static str {
    let Ok(metadata) = std::fs::symlink_metadata(root.join(path)) else {
        return "100644";
    };
    if metadata.is_symlink() {
        return MODE_SYMLINK;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 != 0 {
            return "100755";
        }
    }
    "100644"
}
