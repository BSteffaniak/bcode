//! Guide context: the repository files, notes, and template every guide of a repository reads
//! (see [`super::config`]).
//!
//! [`context_paths`] lists the files the configuration names, confined to the repository; the
//! code review plugin declares them as reads when it prepares a call, so path rules decide them
//! before anything is read. [`read_context`] then reads only those files, within the byte bound,
//! from the working tree or at the review's head, and digests what it read so a later refresh
//! can tell when the context changed.

use super::cite::read_file;
use super::config::{ContextAt, GuideConfig, MAX_CONTEXT_ENTRIES};
use crate::data::{ReviewData, git, grep};
use bcode_code_review_models::RevisionRef;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::io::Read as _;
use std::path::Path;

/// Files the context reads at most.
pub const MAX_CONTEXT_FILES: usize = 64;
/// Bytes of a file inspected to tell whether it is binary.
const BINARY_SNIFF_BYTES: usize = 8_000;

/// One context file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextFile {
    /// Repository-relative path.
    pub path: String,
    /// Its size in bytes, when it was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Its content, as much as fit the bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Whether the content was cut.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// Why it could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// The context a guide reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuideContext {
    /// Context files, in configuration order.
    pub files: Vec<ContextFile>,
    /// Configured notes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Where the files were read: `worktree` or `head`.
    pub read_at: &'static str,
    /// Bytes of content included.
    pub included_bytes: usize,
    /// Bytes of content included at most.
    pub max_bytes: usize,
    /// SHA-256 (hex) of what was read, the notes, and the template.
    pub digest: String,
    /// What in the configuration could not be used.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

/// The repository-relative files the configuration names, and what could not be used.
///
/// These are its context files, then the files under its context directories as the working
/// tree lists them (tracked and untracked, not ignored), at most [`MAX_CONTEXT_FILES`], without
/// repeats.
#[must_use]
pub fn context_paths(repo_root: &Path, config: &GuideConfig) -> (Vec<String>, Vec<String>) {
    let mut paths: Vec<String> = Vec::new();
    let mut problems = Vec::new();
    let add = |paths: &mut Vec<String>, path: String| {
        if !paths.contains(&path) {
            paths.push(path);
        }
    };
    for entry in config.context_files.iter().take(MAX_CONTEXT_ENTRIES) {
        match confined(entry) {
            Some(path) => add(&mut paths, path),
            None => problems.push(format!(
                "guide.context_files: {entry:?} must be a path inside the repository; skipped"
            )),
        }
    }
    for entry in config.context_dirs.iter().take(MAX_CONTEXT_ENTRIES) {
        let Some(directory) = confined(entry) else {
            problems.push(format!(
                "guide.context_dirs: {entry:?} must be a directory inside the repository; skipped"
            ));
            continue;
        };
        let prefix = format!("{directory}/");
        match grep::list_paths(
            repo_root,
            &RevisionRef::WorkingTree,
            MAX_CONTEXT_FILES,
            |path| path.starts_with(&prefix),
        ) {
            Ok((found, total)) => {
                if found.is_empty() {
                    problems.push(format!("guide.context_dirs: {directory} has no files"));
                }
                if usize::try_from(total).unwrap_or(usize::MAX) > found.len() {
                    problems.push(format!(
                        "guide.context_dirs: {directory} has {total} files; the first {} are read",
                        found.len()
                    ));
                }
                for path in found {
                    add(&mut paths, path);
                }
            }
            Err(error) => problems.push(format!(
                "guide.context_dirs: {directory} cannot be listed: {error}"
            )),
        }
    }
    if paths.len() > MAX_CONTEXT_FILES {
        problems.push(format!(
            "guide: at most {MAX_CONTEXT_FILES} context files are read; the rest are skipped"
        ));
        paths.truncate(MAX_CONTEXT_FILES);
    }
    (paths, problems)
}

/// `entry` as a repository-relative path, when it stays inside the repository.
fn confined(entry: &str) -> Option<String> {
    let path = entry.trim().trim_start_matches("./").trim_end_matches('/');
    git::validate_relative_path(path).ok()?;
    Some(path.to_owned())
}

/// Read the context: `paths` (from [`context_paths`], as the call declared them).
///
/// Files are read from the working tree or, with `context_at = "head"`, at `head`; at most
/// `max_context_bytes` of content in all, files past the bound listed with their sizes only.
#[must_use]
pub fn read_context(
    data: &ReviewData,
    repo_root: &Path,
    config: &GuideConfig,
    paths: &[String],
    head: Option<&RevisionRef>,
    template: &str,
    mut problems: Vec<String>,
) -> GuideContext {
    let at_head = match (config.context_at, head) {
        (ContextAt::Head, Some(head)) if !head.is_live() => Some(head),
        _ => None,
    };
    let mut remaining = config.max_context_bytes;
    let mut digest = Sha256::new();
    let mut files = Vec::with_capacity(paths.len());
    for path in paths.iter().take(MAX_CONTEXT_FILES) {
        let read = at_head.map_or_else(
            || read_worktree(repo_root, path, remaining),
            |revision| read_at(data, repo_root, revision, path, remaining),
        );
        let file = match read {
            Ok(Some((bytes, content, truncated))) => {
                remaining = remaining.saturating_sub(content.len());
                digest.update(path.as_bytes());
                digest.update([0]);
                digest.update(content.as_bytes());
                digest.update([0]);
                ContextFile {
                    path: path.clone(),
                    bytes: Some(bytes),
                    content: (!content.is_empty() || bytes == 0).then_some(content),
                    truncated,
                    problem: None,
                }
            }
            Ok(None) => ContextFile {
                path: path.clone(),
                bytes: None,
                content: None,
                truncated: false,
                problem: Some("not found".to_owned()),
            },
            Err(problem) => ContextFile {
                path: path.clone(),
                bytes: None,
                content: None,
                truncated: false,
                problem: Some(problem),
            },
        };
        files.push(file);
    }
    if paths.len() > MAX_CONTEXT_FILES {
        problems.push(format!(
            "at most {MAX_CONTEXT_FILES} context files are read"
        ));
    }
    if let Some(notes) = &config.notes {
        digest.update(b"notes\0");
        digest.update(notes.as_bytes());
    }
    digest.update(b"\0template\0");
    digest.update(template.as_bytes());
    GuideContext {
        included_bytes: config.max_context_bytes - remaining,
        max_bytes: config.max_context_bytes,
        files,
        notes: config.notes.clone(),
        read_at: if at_head.is_some() {
            "head"
        } else {
            "worktree"
        },
        digest: hex(&digest.finalize()),
        problems,
    }
}

/// A file read: its size, as much content as fit, and whether it was cut; `None` when absent.
type ContextRead = Result<Option<(u64, String, bool)>, String>;

fn read_worktree(repo_root: &Path, path: &str, room: usize) -> ContextRead {
    let full = repo_root.join(path);
    let Ok(metadata) = std::fs::metadata(&full) else {
        return Ok(None);
    };
    if !metadata.is_file() {
        return Err("not a file".to_owned());
    }
    // A symbolic link must not lead the read outside the repository.
    let canonical_root = repo_root
        .canonicalize()
        .map_err(|error| format!("the repository cannot be resolved: {error}"))?;
    let resolved = full
        .canonicalize()
        .map_err(|error| format!("cannot be resolved: {error}"))?;
    if !resolved.starts_with(&canonical_root) {
        return Err("leads outside the repository".to_owned());
    }
    let bytes = metadata.len();
    let mut content = Vec::new();
    std::fs::File::open(&resolved)
        .and_then(|file| {
            file.take(u64::try_from(room.max(BINARY_SNIFF_BYTES)).unwrap_or(u64::MAX))
                .read_to_end(&mut content)
        })
        .map_err(|error| format!("cannot be read: {error}"))?;
    if content
        .iter()
        .take(BINARY_SNIFF_BYTES)
        .any(|byte| *byte == 0)
    {
        return Err("binary".to_owned());
    }
    content.truncate(room);
    let text = String::from_utf8_lossy(&content);
    let text = super::config::cut(&text, room);
    let truncated = u64::try_from(text.len()).unwrap_or(u64::MAX) < bytes;
    Ok(Some((bytes, text, truncated)))
}

fn read_at(
    data: &ReviewData,
    repo_root: &Path,
    revision: &RevisionRef,
    path: &str,
    room: usize,
) -> ContextRead {
    let Some(lines) = read_file(data, repo_root, revision, path)? else {
        return Ok(None);
    };
    let text = lines.join("\n");
    let bytes = u64::try_from(text.len()).unwrap_or(u64::MAX);
    let kept = super::config::cut(&text, room);
    let truncated = kept.len() < text.len();
    Ok(Some((bytes, kept, truncated)))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::fixture::GitFixture;

    fn repo() -> GitFixture {
        let fixture = GitFixture::empty();
        for (path, text) in [
            (".bcode/review-guide.md", "Focus on the wire format.\n"),
            ("docs/adr/001.md", "Use token buckets.\n"),
            ("docs/adr/002.md", &"x".repeat(100)),
            ("src/lib.rs", "fn main() {}\n"),
        ] {
            let full = fixture.root().join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("dir");
            std::fs::write(full, text).expect("write");
        }
        fixture.run(&["add", "."]);
        fixture.run(&["commit", "-q", "-m", "files"]);
        fixture
    }

    fn config(max_bytes: usize) -> GuideConfig {
        GuideConfig {
            context_files: vec![".bcode/review-guide.md".to_owned(), "../escape".to_owned()],
            context_dirs: vec!["docs/adr/".to_owned()],
            max_context_bytes: max_bytes,
            notes: Some("Backwards compatibility matters.".to_owned()),
            ..GuideConfig::default()
        }
    }

    #[test]
    fn configured_files_and_directories_are_listed_inside_the_repository() {
        let fixture = repo();
        let (paths, problems) = context_paths(fixture.root(), &config(1024));
        assert_eq!(
            paths,
            [
                ".bcode/review-guide.md",
                "docs/adr/001.md",
                "docs/adr/002.md"
            ]
        );
        assert!(
            problems.iter().any(|problem| problem.contains("../escape")),
            "{problems:?}"
        );
    }

    #[test]
    fn content_is_bounded_listed_by_size_and_digested() {
        let fixture = repo();
        let config = config(40);
        let (paths, problems) = context_paths(fixture.root(), &config);
        let data = ReviewData::default();
        let context = read_context(
            &data,
            fixture.root(),
            &config,
            &paths,
            None,
            "default",
            problems,
        );
        assert_eq!(context.included_bytes, 40);
        assert_eq!(
            context.files[0].content.as_deref(),
            Some("Focus on the wire format.\n")
        );
        let last = context.files.last().expect("a file");
        assert_eq!(last.bytes, Some(100));
        assert!(
            last.content.is_none(),
            "past the bound only the size is listed"
        );
        let again = read_context(
            &data,
            fixture.root(),
            &config,
            &paths,
            None,
            "default",
            Vec::new(),
        );
        assert_eq!(context.digest, again.digest);
        std::fs::write(
            fixture.root().join("docs/adr/001.md"),
            "Use leaky buckets.\n",
        )
        .expect("edit");
        let changed = read_context(
            &data,
            fixture.root(),
            &config,
            &paths,
            None,
            "default",
            Vec::new(),
        );
        assert_ne!(
            context.digest, changed.digest,
            "a changed file changes the digest"
        );
    }

    #[test]
    fn context_at_head_reads_the_pinned_commit() {
        let fixture = repo();
        let head = RevisionRef::commit(fixture.git(&["rev-parse", "HEAD"]).trim());
        std::fs::write(fixture.root().join(".bcode/review-guide.md"), "changed\n").expect("edit");
        let config = GuideConfig {
            context_at: ContextAt::Head,
            ..config(1024)
        };
        let paths = vec![".bcode/review-guide.md".to_owned()];
        let context = read_context(
            &ReviewData::default(),
            fixture.root(),
            &config,
            &paths,
            Some(&head),
            "default",
            Vec::new(),
        );
        assert_eq!(context.read_at, "head");
        assert_eq!(
            context.files[0].content.as_deref(),
            Some("Focus on the wire format.")
        );
    }
}
