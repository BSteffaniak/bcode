//! Parsers for machine-readable git output.

use std::collections::BTreeMap;
use std::path::PathBuf;

use bcode_git_models::{
    Branch, BranchStatus, ChangeKind, ChangedFile, Commit, ConflictKind, DiffFileStatus, LineStats,
    Progress, ReflogEntry, Remote, RemoteBranch, StashEntry, StatusEntry, StatusEntryKind,
    StatusSnapshot, Submodule, SubmoduleState, Tag, Worktree,
};

use crate::GitError;

/// Field separator used in custom `--format` strings.
pub const FIELD: char = '\u{1f}';
/// Record separator used in custom `--format` strings.
pub const RECORD: char = '\u{1e}';

/// Parse `git status --porcelain=v2 --branch -z` output.
///
/// # Errors
/// * [`GitError::Parse`] for records with missing fields.
pub fn parse_status(output: &str) -> Result<StatusSnapshot, GitError> {
    let mut snapshot = StatusSnapshot::default();
    let mut records = output.split('\0').filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        let invalid = || GitError::Parse(format!("invalid status record: {record}"));
        if let Some(header) = record.strip_prefix("# ") {
            parse_branch_header(&mut snapshot.branch, header);
            continue;
        }
        let (tag, rest) = record.split_once(' ').ok_or_else(invalid)?;
        let entry = match tag {
            "1" => {
                let fields: Vec<&str> = rest.splitn(8, ' ').collect();
                let [xy, sub, _, _, _, _, _, path] = fields[..] else {
                    return Err(invalid());
                };
                tracked_entry(xy, sub, path, None, StatusEntryKind::Ordinary)
            }
            "2" => {
                let fields: Vec<&str> = rest.splitn(9, ' ').collect();
                let [xy, sub, _, _, _, _, _, _, path] = fields[..] else {
                    return Err(invalid());
                };
                let orig = records.next().ok_or_else(invalid)?;
                tracked_entry(
                    xy,
                    sub,
                    path,
                    Some(orig.to_owned()),
                    StatusEntryKind::RenamedOrCopied,
                )
            }
            "u" => {
                let fields: Vec<&str> = rest.splitn(10, ' ').collect();
                let [xy, sub, _, _, _, _, _, _, _, path] = fields[..] else {
                    return Err(invalid());
                };
                let conflict = ConflictKind::from_codes(xy).ok_or_else(invalid)?;
                tracked_entry(xy, sub, path, None, StatusEntryKind::Unmerged(conflict))
            }
            "?" | "!" => StatusEntry {
                path: rest.to_owned(),
                orig_path: None,
                kind: if tag == "?" {
                    StatusEntryKind::Untracked
                } else {
                    StatusEntryKind::Ignored
                },
                index: ChangeKind::Unmodified,
                worktree: ChangeKind::Unmodified,
                submodule: false,
            },
            _ => return Err(invalid()),
        };
        snapshot.entries.push(entry);
    }
    Ok(snapshot)
}

fn parse_branch_header(branch: &mut BranchStatus, header: &str) {
    let Some((key, value)) = header.split_once(' ') else {
        return;
    };
    match key {
        "branch.oid" => branch.oid = (value != "(initial)").then(|| value.to_owned()),
        "branch.head" => branch.head = (value != "(detached)").then(|| value.to_owned()),
        "branch.upstream" => branch.upstream = Some(value.to_owned()),
        "branch.ab" => {
            for part in value.split_whitespace() {
                if let Some(ahead) = part.strip_prefix('+') {
                    branch.ahead = ahead.parse().unwrap_or_default();
                } else if let Some(behind) = part.strip_prefix('-') {
                    branch.behind = behind.parse().unwrap_or_default();
                }
            }
        }
        _ => {}
    }
}

fn tracked_entry(
    xy: &str,
    sub: &str,
    path: &str,
    orig_path: Option<String>,
    kind: StatusEntryKind,
) -> StatusEntry {
    let mut codes = xy.chars();
    let index = codes
        .next()
        .map_or(ChangeKind::Unmodified, ChangeKind::from_code);
    let worktree = codes
        .next()
        .map_or(ChangeKind::Unmodified, ChangeKind::from_code);
    StatusEntry {
        path: path.to_owned(),
        orig_path,
        kind,
        index,
        worktree,
        submodule: sub.starts_with('S'),
    }
}

/// `git log` format matching [`parse_log`].
pub const LOG_FORMAT: &str = "--format=%H%x1f%h%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%ct%x1f%D%x1f%s%x1e";

/// Parse output produced with [`LOG_FORMAT`].
///
/// # Errors
/// * [`GitError::Parse`] for records with missing fields.
pub fn parse_log(output: &str) -> Result<Vec<Commit>, GitError> {
    records(output)
        .map(|record| {
            let fields: Vec<&str> = record.splitn(9, FIELD).collect();
            let [oid, short, parents, name, email, at, ct, refs, summary] = fields[..] else {
                return Err(GitError::Parse(format!("invalid log record: {record}")));
            };
            Ok(Commit {
                oid: oid.to_owned(),
                short_oid: short.to_owned(),
                parents: parents.split_whitespace().map(str::to_owned).collect(),
                author_name: name.to_owned(),
                author_email: email.to_owned(),
                author_time: at.parse().unwrap_or_default(),
                commit_time: ct.parse().unwrap_or_default(),
                refs: refs
                    .split(", ")
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect(),
                summary: summary.to_owned(),
            })
        })
        .collect()
}

/// `for-each-ref` format matching [`parse_branches`].
pub const BRANCH_FORMAT: &str = "--format=%(HEAD)%1f%(refname:short)%1f%(objectname)%1f%(upstream:short)%1f%(upstream:track,nobracket)%1f%(committerdate:unix)%1f%(worktreepath)%1f%(contents:subject)%1e";

/// Parse branches produced with [`BRANCH_FORMAT`].
///
/// # Errors
/// * [`GitError::Parse`] for records with missing fields.
pub fn parse_branches(
    output: &str,
    current_worktree: &std::path::Path,
) -> Result<Vec<Branch>, GitError> {
    records(output)
        .map(|record| {
            let fields: Vec<&str> = record.splitn(8, FIELD).collect();
            let [head, name, oid, upstream, track, time, worktree, summary] = fields[..] else {
                return Err(GitError::Parse(format!("invalid branch record: {record}")));
            };
            let (ahead, behind, gone) = parse_track(track);
            let worktree = (!worktree.is_empty())
                .then(|| PathBuf::from(worktree))
                .filter(|path| !same_path(path, current_worktree));
            Ok(Branch {
                name: name.to_owned(),
                oid: oid.to_owned(),
                is_head: head == "*",
                upstream: (!upstream.is_empty()).then(|| upstream.to_owned()),
                ahead,
                behind,
                upstream_gone: gone,
                commit_time: time.parse().unwrap_or_default(),
                summary: summary.to_owned(),
                worktree,
            })
        })
        .collect()
}

fn same_path(left: &std::path::Path, right: &std::path::Path) -> bool {
    let canonical =
        |path: &std::path::Path| path.canonicalize().unwrap_or_else(|_| path.to_owned());
    canonical(left) == canonical(right)
}

fn parse_track(track: &str) -> (u32, u32, bool) {
    if track == "gone" {
        return (0, 0, true);
    }
    let mut ahead = 0;
    let mut behind = 0;
    for part in track.split(", ") {
        if let Some(value) = part.strip_prefix("ahead ") {
            ahead = value.parse().unwrap_or_default();
        } else if let Some(value) = part.strip_prefix("behind ") {
            behind = value.parse().unwrap_or_default();
        }
    }
    (ahead, behind, false)
}

/// `for-each-ref` format matching [`parse_remote_branches`].
pub const REMOTE_BRANCH_FORMAT: &str =
    "--format=%(refname)%1f%(objectname)%1f%(committerdate:unix)%1f%(contents:subject)%1e";

/// Parse remote-tracking branches produced with [`REMOTE_BRANCH_FORMAT`]. Symbolic `HEAD`
/// entries are skipped.
///
/// # Errors
/// * [`GitError::Parse`] for records with missing fields.
pub fn parse_remote_branches(output: &str) -> Result<Vec<RemoteBranch>, GitError> {
    let mut branches = Vec::new();
    for record in records(output) {
        let fields: Vec<&str> = record.splitn(4, FIELD).collect();
        let [refname, oid, time, summary] = fields[..] else {
            return Err(GitError::Parse(format!("invalid remote record: {record}")));
        };
        let Some(rest) = refname.strip_prefix("refs/remotes/") else {
            continue;
        };
        let Some((remote, name)) = rest.split_once('/') else {
            continue;
        };
        if name == "HEAD" {
            continue;
        }
        branches.push(RemoteBranch {
            remote: remote.to_owned(),
            name: name.to_owned(),
            oid: oid.to_owned(),
            commit_time: time.parse().unwrap_or_default(),
            summary: summary.to_owned(),
        });
    }
    Ok(branches)
}

/// `for-each-ref` format matching [`parse_tags`].
pub const TAG_FORMAT: &str = "--format=%(refname:short)%1f%(objecttype)%1f%(objectname)%1f%(*objectname)%1f%(creatordate:unix)%1f%(contents:subject)%1e";

/// Parse tags produced with [`TAG_FORMAT`].
///
/// # Errors
/// * [`GitError::Parse`] for records with missing fields.
pub fn parse_tags(output: &str) -> Result<Vec<Tag>, GitError> {
    records(output)
        .map(|record| {
            let fields: Vec<&str> = record.splitn(6, FIELD).collect();
            let [name, kind, oid, peeled, time, summary] = fields[..] else {
                return Err(GitError::Parse(format!("invalid tag record: {record}")));
            };
            let annotated = kind == "tag";
            Ok(Tag {
                name: name.to_owned(),
                target: if annotated && !peeled.is_empty() {
                    peeled.to_owned()
                } else {
                    oid.to_owned()
                },
                annotated,
                summary: summary.to_owned(),
                time: time.parse().unwrap_or_default(),
            })
        })
        .collect()
}

/// `stash list` format matching [`parse_stashes`].
pub const STASH_FORMAT: &str = "--format=%H%x1f%ct%x1f%gs%x1e";

/// Parse stash entries produced with [`STASH_FORMAT`].
///
/// # Errors
/// * [`GitError::Parse`] for records with missing fields.
pub fn parse_stashes(output: &str) -> Result<Vec<StashEntry>, GitError> {
    records(output)
        .enumerate()
        .map(|(index, record)| {
            let fields: Vec<&str> = record.splitn(3, FIELD).collect();
            let [oid, time, message] = fields[..] else {
                return Err(GitError::Parse(format!("invalid stash record: {record}")));
            };
            Ok(StashEntry {
                index,
                oid: oid.to_owned(),
                message: message.to_owned(),
                time: time.parse().unwrap_or_default(),
            })
        })
        .collect()
}

/// `log -g` format matching [`parse_reflog`].
pub const REFLOG_FORMAT: &str = "--format=%gd%x1f%H%x1f%h%x1f%ct%x1f%gs%x1e";

/// Parse reflog entries produced with [`REFLOG_FORMAT`].
///
/// # Errors
/// * [`GitError::Parse`] for records with missing fields.
pub fn parse_reflog(output: &str) -> Result<Vec<ReflogEntry>, GitError> {
    records(output)
        .map(|record| {
            let fields: Vec<&str> = record.splitn(5, FIELD).collect();
            let [selector, oid, short, time, message] = fields[..] else {
                return Err(GitError::Parse(format!("invalid reflog record: {record}")));
            };
            Ok(ReflogEntry {
                selector: selector.to_owned(),
                oid: oid.to_owned(),
                short_oid: short.to_owned(),
                message: message.to_owned(),
                time: time.parse().unwrap_or_default(),
            })
        })
        .collect()
}

/// Parse `git remote -v` output.
#[must_use]
pub fn parse_remotes(output: &str) -> Vec<Remote> {
    let mut remotes: Vec<Remote> = Vec::new();
    for line in output.lines() {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(url), Some(kind)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let existing = remotes.iter().position(|remote| remote.name == name);
        let index = existing.unwrap_or_else(|| {
            remotes.push(Remote {
                name: name.to_owned(),
                fetch_url: None,
                push_url: None,
            });
            remotes.len() - 1
        });
        match kind {
            "(fetch)" => remotes[index].fetch_url = Some(url.to_owned()),
            "(push)" => remotes[index].push_url = Some(url.to_owned()),
            _ => {}
        }
    }
    remotes
}

/// Parse `git worktree list --porcelain -z` output.
#[must_use]
pub fn parse_worktrees(output: &str) -> Vec<Worktree> {
    let mut worktrees = Vec::new();
    let mut current: Option<Worktree> = None;
    for field in output.split('\0') {
        if field.is_empty() {
            if let Some(worktree) = current.take() {
                worktrees.push(worktree);
            }
            continue;
        }
        let (key, value) = field.split_once(' ').unwrap_or((field, ""));
        if key == "worktree" {
            if let Some(worktree) = current.take() {
                worktrees.push(worktree);
            }
            current = Some(Worktree {
                path: PathBuf::from(value),
                head: None,
                branch: None,
                bare: false,
                detached: false,
                locked: false,
                prunable: false,
                is_main: worktrees.is_empty(),
            });
            continue;
        }
        let Some(worktree) = current.as_mut() else {
            continue;
        };
        match key {
            "HEAD" => worktree.head = Some(value.to_owned()),
            "branch" => {
                worktree.branch = Some(
                    value
                        .strip_prefix("refs/heads/")
                        .unwrap_or(value)
                        .to_owned(),
                );
            }
            "bare" => worktree.bare = true,
            "detached" => worktree.detached = true,
            "locked" => worktree.locked = true,
            "prunable" => worktree.prunable = true,
            _ => {}
        }
    }
    worktrees.extend(current);
    worktrees
}

/// Parse `git submodule status` output.
#[must_use]
pub fn parse_submodules(output: &str) -> Vec<Submodule> {
    output
        .lines()
        .filter(|line| line.len() > 1)
        .filter_map(|line| {
            let state = match line.as_bytes()[0] {
                b'-' => SubmoduleState::Uninitialized,
                b'+' => SubmoduleState::Modified,
                b'U' => SubmoduleState::Conflicted,
                _ => SubmoduleState::Current,
            };
            let mut parts = line[1..].splitn(2, ' ');
            let oid = parts.next()?.to_owned();
            let rest = parts.next()?;
            let (path, describe) = match rest.rsplit_once(" (") {
                Some((path, describe)) => (
                    path.to_owned(),
                    Some(describe.trim_end_matches(')').to_owned()),
                ),
                None => (rest.to_owned(), None),
            };
            Some(Submodule {
                path,
                oid,
                state,
                describe,
            })
        })
        .collect()
}

/// Parse `--name-status -z` output.
#[must_use]
pub fn parse_name_status(output: &str) -> Vec<ChangedFile> {
    let mut files = Vec::new();
    let mut fields = output.split('\0').filter(|field| !field.is_empty());
    while let Some(code) = fields.next() {
        let status = match code.as_bytes().first() {
            Some(b'A') => DiffFileStatus::Added,
            Some(b'D') => DiffFileStatus::Deleted,
            Some(b'R') => DiffFileStatus::Renamed,
            Some(b'C') => DiffFileStatus::Copied,
            _ => DiffFileStatus::Modified,
        };
        let Some(first) = fields.next() else {
            break;
        };
        let (old_path, path) = if matches!(status, DiffFileStatus::Renamed | DiffFileStatus::Copied)
        {
            let Some(second) = fields.next() else {
                break;
            };
            (Some(first.to_owned()), second.to_owned())
        } else {
            (None, first.to_owned())
        };
        files.push(ChangedFile {
            path,
            old_path,
            status,
        });
    }
    files
}

/// Parse `--numstat -z` output into counts by (new) path.
///
/// Records are `<added>\t<removed>\t<path>\0`, or `<added>\t<removed>\t\0<old>\0<new>\0` for
/// renames and copies; binary files report `-` for both counts.
#[must_use]
pub fn parse_numstat(output: &str) -> BTreeMap<String, LineStats> {
    let mut stats = BTreeMap::new();
    let mut fields = output.split('\0');
    while let Some(record) = fields.next() {
        let mut parts = record.trim_start_matches('\n').splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let path = if path.is_empty() {
            // A rename: the old path, then the new one.
            let (Some(_), Some(new)) = (fields.next(), fields.next()) else {
                break;
            };
            new
        } else {
            path
        };
        let counts = match (added.parse(), removed.parse()) {
            (Ok(added), Ok(removed)) => LineStats::text(added, removed),
            _ => LineStats::binary(),
        };
        let entry: &mut LineStats = stats.entry(path.to_owned()).or_default();
        *entry = entry.combined(counts);
    }
    stats
}

/// The latest progress a git command printed, from its terminal text.
///
/// That is the last line among the final few reading `[remote: ]<phase>: <n>%`, followed by
/// nothing, ` (…` or `,`. Git redraws its progress line in place, so terminal text holds only
/// the latest state of each phase.
#[must_use]
pub fn parse_progress(text: &str) -> Option<Progress> {
    text.lines().rev().take(8).find_map(|line| {
        let line = line.trim();
        let line = line.strip_prefix("remote:").map_or(line, str::trim_start);
        let (phase, rest) = line.split_once(": ")?;
        let digits = rest.trim_start();
        let (number, after) = digits.split_once('%')?;
        if !(after.is_empty() || after.starts_with(" (") || after.starts_with(',')) {
            return None;
        }
        let percent = number
            .parse::<u8>()
            .ok()
            .filter(|percent| *percent <= 100)?;
        (!phase.is_empty()).then(|| Progress {
            phase: phase.to_owned(),
            percent,
        })
    })
}

fn records(output: &str) -> impl Iterator<Item = &str> {
    output
        .split(RECORD)
        .map(|record| record.trim_start_matches('\n'))
        .filter(|record| !record.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numstat_with_renames_and_binary_files() {
        let stats = parse_numstat("3\t1\tsrc/a b.rs\0-\t-\tlogo.png\x005\t0\t\0old.rs\0new.rs\0");
        assert_eq!(stats.len(), 3);
        assert_eq!(stats["src/a b.rs"], LineStats::text(3, 1));
        assert_eq!(stats["logo.png"], LineStats::binary());
        assert_eq!(stats["new.rs"], LineStats::text(5, 0));
        assert!(parse_numstat("").is_empty());
    }

    #[test]
    fn parses_porcelain_v2_status() {
        let output = "# branch.oid abc\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 a a src/a b.rs\0\
2 R. N... 100644 100644 100644 a a R100 new.rs\0old.rs\0\
u UU N... 100644 100644 100644 100644 a b c conflict.rs\0\
1 .A N... 000000 000000 100644 0 0 ita.rs\0\
? untracked file.txt\0";
        let status = parse_status(output).unwrap();
        assert_eq!(status.branch.head.as_deref(), Some("main"));
        assert_eq!((status.branch.ahead, status.branch.behind), (2, 1));
        assert_eq!(status.entries.len(), 5);
        assert_eq!(status.entries[0].path, "src/a b.rs");
        assert!(status.entries[0].has_unstaged());
        assert_eq!(status.entries[1].orig_path.as_deref(), Some("old.rs"));
        assert!(status.entries[1].has_staged());
        assert!(status.entries[2].is_conflicted());
        assert!(status.entries[3].is_intent_to_add());
        assert!(status.entries[4].is_untracked());
    }

    #[test]
    fn parses_detached_and_initial_headers() {
        let status = parse_status("# branch.oid (initial)\0# branch.head (detached)\0").unwrap();
        assert_eq!(status.branch.oid, None);
        assert_eq!(status.branch.head, None);
    }

    #[test]
    fn parses_log_records_with_unicode_and_refs() {
        let output = "abc\u{1f}ab\u{1f}p1 p2\u{1f}名前\u{1f}e@x\u{1f}10\u{1f}11\u{1f}HEAD -> main, tag: v1\u{1f}subject \u{1f} with sep\u{1e}\n";
        let commits = parse_log(output).unwrap();
        assert_eq!(commits.len(), 1);
        assert!(commits[0].is_merge());
        assert_eq!(commits[0].refs, ["HEAD -> main", "tag: v1"]);
        assert_eq!(commits[0].summary, "subject \u{1f} with sep");
    }

    #[test]
    fn parses_name_status_with_renames() {
        let files = parse_name_status("M\0a.rs\0R090\0old.rs\0new.rs\0A\0n e w\0");
        assert_eq!(files.len(), 3);
        assert_eq!(files[1].old_path.as_deref(), Some("old.rs"));
        assert_eq!(files[1].path, "new.rs");
        assert_eq!(files[2].path, "n e w");
        assert_eq!(files[2].status, DiffFileStatus::Added);
    }

    #[test]
    fn parses_tracking_states() {
        assert_eq!(parse_track("ahead 3, behind 2"), (3, 2, false));
        assert_eq!(parse_track("gone"), (0, 0, true));
        assert_eq!(parse_track(""), (0, 0, false));
    }

    #[test]
    fn parses_remotes_worktrees_and_submodules() {
        let remotes = parse_remotes("origin\tgit@x:a.git (fetch)\norigin\tgit@x:a.git (push)\n");
        assert_eq!(remotes.len(), 1);
        assert!(remotes[0].push_url.is_some());
        let worktrees = parse_worktrees(
            "worktree /a\0HEAD abc\0branch refs/heads/main\0\0worktree /b\0HEAD def\0detached\0locked\0\0",
        );
        assert_eq!(worktrees.len(), 2);
        assert!(worktrees[0].is_main);
        assert_eq!(worktrees[0].branch.as_deref(), Some("main"));
        assert!(worktrees[1].detached && worktrees[1].locked);
        let submodules = parse_submodules(" abc lib/x (v1.0)\n-def lib/y\n");
        assert_eq!(submodules[0].describe.as_deref(), Some("v1.0"));
        assert_eq!(submodules[1].state, SubmoduleState::Uninitialized);
    }

    #[test]
    fn reads_the_latest_progress_line() {
        let text = "Enumerating objects: 5, done.\nCounting objects: 100% (5/5), done.\n\
remote: Compressing objects:  40% (2/5)\n";
        assert_eq!(
            parse_progress(text),
            Some(Progress {
                phase: "Compressing objects".into(),
                percent: 40,
            })
        );
        let pushed = "Writing objects: 100% (3/3), 280 bytes | 280.00 KiB/s, done.\nTo ../origin.git\n   abc..def  main -> main";
        assert_eq!(
            parse_progress(pushed).map(|progress| progress.percent),
            Some(100)
        );
        assert_eq!(parse_progress("Already up to date.\nfatal: 50%: odd"), None);
        assert_eq!(parse_progress(""), None);
    }
}
