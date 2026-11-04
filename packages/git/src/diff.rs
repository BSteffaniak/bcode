//! Unified diff parsing.

use bcode_git_models::{DiffFile, DiffFileStatus, DiffHunk, DiffLine, DiffLineKind};

use crate::GitError;

/// Parse `git diff` output into files, hunks and lines.
///
/// Lines are split on `\n` only, so carriage returns in CRLF content are preserved and patches
/// rebuilt from the result apply byte-for-byte.
///
/// # Errors
/// * [`GitError::Parse`] for malformed hunk headers.
pub fn parse_unified_diff(diff: &str) -> Result<Vec<DiffFile>, GitError> {
    let mut files = Vec::new();
    let mut file: Option<DiffFile> = None;
    let mut hunk: Option<DiffHunk> = None;
    let mut old_line = 0u32;
    let mut new_line = 0u32;
    let mut lines = diff.split('\n').peekable();

    while let Some(line) = lines.next() {
        if line.is_empty() && lines.peek().is_none() {
            break;
        }
        if line.starts_with("diff --git ") || line.starts_with("diff --cc ") {
            finish_hunk(file.as_mut(), hunk.take());
            files.extend(file.take());
            file = Some(file_from_diff_line(line));
            continue;
        }
        let Some(current) = file.as_mut() else {
            continue;
        };

        if hunk.is_none() {
            if !line.starts_with("@@") {
                apply_header_line(current, line);
                continue;
            }
        } else if let Some(rest) = line.strip_prefix('\\') {
            // `\ No newline at end of file` applies to the previous line.
            if rest.contains("No newline")
                && let Some(last) = hunk.as_mut().and_then(|hunk| hunk.lines.last_mut())
            {
                last.no_newline = true;
            }
            continue;
        }

        if line.starts_with("@@ ") {
            finish_hunk(Some(current), hunk.take());
            let parsed = parse_hunk_header(line)?;
            old_line = parsed.old_start;
            new_line = parsed.new_start;
            hunk = Some(parsed);
            continue;
        }

        let Some(current_hunk) = hunk.as_mut() else {
            continue;
        };
        let (kind, content) = match line.as_bytes().first() {
            Some(b'+') => (DiffLineKind::Added, &line[1..]),
            Some(b'-') => (DiffLineKind::Removed, &line[1..]),
            Some(b' ') => (DiffLineKind::Context, &line[1..]),
            None => (DiffLineKind::Context, ""),
            _ => {
                // A non-diff line ends the hunk section; treat as a header line of a new section.
                finish_hunk(Some(current), hunk.take());
                apply_header_line(current, line);
                continue;
            }
        };
        let (old, new) = match kind {
            DiffLineKind::Added => {
                current.additions = current.additions.saturating_add(1);
                new_line = new_line.saturating_add(1);
                (None, Some(new_line - 1))
            }
            DiffLineKind::Removed => {
                current.deletions = current.deletions.saturating_add(1);
                old_line = old_line.saturating_add(1);
                (Some(old_line - 1), None)
            }
            DiffLineKind::Context => {
                old_line = old_line.saturating_add(1);
                new_line = new_line.saturating_add(1);
                (Some(old_line - 1), Some(new_line - 1))
            }
        };
        current_hunk.lines.push(DiffLine {
            kind,
            old_line: old,
            new_line: new,
            content: content.to_owned(),
            no_newline: false,
        });
    }

    finish_hunk(file.as_mut(), hunk.take());
    files.extend(file.take());
    Ok(files)
}

fn apply_header_line(file: &mut DiffFile, line: &str) {
    file.header.push(line.to_owned());
    if let Some(mode) = line.strip_prefix("new file mode ") {
        file.status = DiffFileStatus::Added;
        file.new_mode = Some(mode.to_owned());
    } else if let Some(mode) = line.strip_prefix("deleted file mode ") {
        file.status = DiffFileStatus::Deleted;
        file.old_mode = Some(mode.to_owned());
    } else if let Some(mode) = line.strip_prefix("old mode ") {
        file.old_mode = Some(mode.to_owned());
    } else if let Some(mode) = line.strip_prefix("new mode ") {
        file.new_mode = Some(mode.to_owned());
    } else if let Some(path) = line.strip_prefix("rename from ") {
        file.old_path = Some(unquote(path));
        file.status = DiffFileStatus::Renamed;
    } else if let Some(path) = line.strip_prefix("rename to ") {
        file.new_path = Some(unquote(path));
        file.status = DiffFileStatus::Renamed;
    } else if let Some(path) = line.strip_prefix("copy from ") {
        file.old_path = Some(unquote(path));
        file.status = DiffFileStatus::Copied;
    } else if let Some(path) = line.strip_prefix("copy to ") {
        file.new_path = Some(unquote(path));
        file.status = DiffFileStatus::Copied;
    } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
        file.is_binary = true;
    } else if let Some(path) = line.strip_prefix("--- ") {
        file.old_path = diff_side_path(path, "a/");
    } else if let Some(path) = line.strip_prefix("+++ ") {
        file.new_path = diff_side_path(path, "b/");
    }
}

fn diff_side_path(path: &str, prefix: &str) -> Option<String> {
    let path = path.strip_suffix('\t').unwrap_or(path);
    if path == "/dev/null" {
        return None;
    }
    let path = unquote(path);
    Some(
        path.strip_prefix(prefix)
            .map_or_else(|| path.clone(), str::to_owned),
    )
}

fn file_from_diff_line(line: &str) -> DiffFile {
    let rest = line
        .strip_prefix("diff --git ")
        .or_else(|| line.strip_prefix("diff --cc "))
        .unwrap_or_default();
    let (old_path, new_path) = split_diff_git_paths(rest);
    DiffFile {
        old_path,
        new_path,
        status: DiffFileStatus::Modified,
        old_mode: None,
        new_mode: None,
        additions: 0,
        deletions: 0,
        is_binary: false,
        header: vec![line.to_owned()],
        hunks: Vec::new(),
    }
}

/// Split `a/<old> b/<new>`. Unquoted paths with spaces are ambiguous, so prefer equal halves.
fn split_diff_git_paths(rest: &str) -> (Option<String>, Option<String>) {
    if rest.starts_with('"') {
        let mut parts = split_quoted(rest);
        let old = parts.next().map(|path| strip_side(&path, "a/"));
        let new = parts.next().map(|path| strip_side(&path, "b/"));
        return (old, new);
    }
    if rest.len() % 2 == 1 {
        let half = rest.len() / 2;
        if rest.is_char_boundary(half)
            && let (left, right) = rest.split_at(half)
            && let Some(old) = left.strip_prefix("a/")
            && let Some(new) = right.strip_prefix(" b/")
            && old == new
        {
            return (Some(old.to_owned()), Some(new.to_owned()));
        }
    }
    match rest.split_once(" b/") {
        Some((old, new)) => (
            Some(old.strip_prefix("a/").unwrap_or(old).to_owned()),
            Some(new.to_owned()),
        ),
        None => (None, None),
    }
}

fn strip_side(path: &str, prefix: &str) -> String {
    path.strip_prefix(prefix).unwrap_or(path).to_owned()
}

fn split_quoted(text: &str) -> impl Iterator<Item = String> + '_ {
    let mut rest = text;
    std::iter::from_fn(move || {
        rest = rest.trim_start();
        if rest.is_empty() {
            return None;
        }
        if rest.starts_with('"') {
            let bytes = rest.as_bytes();
            let mut index = 1;
            while index < bytes.len() {
                match bytes[index] {
                    b'\\' => index += 2,
                    b'"' => break,
                    _ => index += 1,
                }
            }
            let end = (index + 1).min(rest.len());
            let token = unquote(&rest[..end]);
            rest = &rest[end..];
            Some(token)
        } else {
            let end = rest.find(' ').unwrap_or(rest.len());
            let token = rest[..end].to_owned();
            rest = &rest[end..];
            Some(token)
        }
    })
}

/// Decode a C-style quoted git path. Unquoted input is returned unchanged.
#[must_use]
pub fn unquote(path: &str) -> String {
    let Some(inner) = path
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return path.to_owned();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let mut chars = inner.bytes().peekable();
    while let Some(byte) = chars.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        match chars.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'r') => bytes.push(b'\r'),
            Some(b'a') => bytes.push(0x07),
            Some(b'b') => bytes.push(0x08),
            Some(b'f') => bytes.push(0x0c),
            Some(b'v') => bytes.push(0x0b),
            Some(digit @ b'0'..=b'7') => {
                let mut value = u32::from(digit - b'0');
                for _ in 0..2 {
                    if let Some(next @ b'0'..=b'7') = chars.peek().copied() {
                        value = value * 8 + u32::from(next - b'0');
                        chars.next();
                    }
                }
                bytes.push(u8::try_from(value & 0xff).unwrap_or_default());
            }
            Some(other) => bytes.push(other),
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Parse a `@@ -a,b +c,d @@ heading` hunk header.
///
/// # Errors
/// * [`GitError::Parse`] when the ranges are missing or not numeric.
pub fn parse_hunk_header(line: &str) -> Result<DiffHunk, GitError> {
    let invalid = || GitError::Parse(format!("invalid hunk header: {line}"));
    let rest = line.strip_prefix("@@ ").ok_or_else(invalid)?;
    let (ranges, heading) = rest.split_once(" @@").ok_or_else(invalid)?;
    let mut ranges = ranges.split_whitespace();
    let (old_start, old_count) = parse_range(ranges.next().ok_or_else(invalid)?, '-')?;
    let (new_start, new_count) = parse_range(ranges.next().ok_or_else(invalid)?, '+')?;
    let heading = heading.trim();
    Ok(DiffHunk {
        old_start,
        old_count,
        new_start,
        new_count,
        heading: (!heading.is_empty()).then(|| heading.to_owned()),
        lines: Vec::new(),
    })
}

fn parse_range(range: &str, prefix: char) -> Result<(u32, u32), GitError> {
    let range = range
        .strip_prefix(prefix)
        .ok_or_else(|| GitError::Parse(format!("hunk range missing '{prefix}': {range}")))?;
    let (start, count) = range.split_once(',').unwrap_or((range, "1"));
    let parse = |value: &str| {
        value
            .parse::<u32>()
            .map_err(|error| GitError::Parse(format!("invalid hunk range '{value}': {error}")))
    };
    Ok((parse(start)?, parse(count)?))
}

fn finish_hunk(file: Option<&mut DiffFile>, hunk: Option<DiffHunk>) {
    if let (Some(file), Some(hunk)) = (file, hunk) {
        file.hunks.push(hunk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modified_file_with_crlf_and_no_newline() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,2 @@ fn main\n keep\r\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n";
        let files = parse_unified_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!(file.path(), "src/a.rs");
        assert_eq!(file.status, DiffFileStatus::Modified);
        assert_eq!((file.additions, file.deletions), (1, 1));
        let hunk = &file.hunks[0];
        assert_eq!(hunk.heading.as_deref(), Some("fn main"));
        assert_eq!(hunk.lines[0].content, "keep\r");
        assert!(hunk.lines[1].no_newline);
        assert!(hunk.lines[2].no_newline);
        assert_eq!(hunk.lines[2].new_line, Some(2));
        assert_eq!(file.header.len(), 4);
    }

    #[test]
    fn parses_rename_new_delete_and_binary() {
        let diff = "diff --git a/old name.txt b/new name.txt\nsimilarity index 90%\nrename from old name.txt\nrename to new name.txt\ndiff --git a/n.txt b/n.txt\nnew file mode 100644\nindex 0..1\n--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1 @@\n+x\ndiff --git a/img.png b/img.png\nindex 1..2 100644\nBinary files a/img.png and b/img.png differ\ndiff --git a/gone b/gone\ndeleted file mode 100755\n--- a/gone\n+++ /dev/null\n@@ -1 +0,0 @@\n-y\n";
        let files = parse_unified_diff(diff).unwrap();
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].status, DiffFileStatus::Renamed);
        assert_eq!(files[0].old_path.as_deref(), Some("old name.txt"));
        assert_eq!(files[0].path(), "new name.txt");
        assert_eq!(files[1].status, DiffFileStatus::Added);
        assert_eq!(files[1].old_path, None);
        assert_eq!(files[1].hunks[0].new_count, 1);
        assert!(files[2].is_binary);
        assert_eq!(files[3].status, DiffFileStatus::Deleted);
        assert_eq!(files[3].path(), "gone");
        assert_eq!(files[3].old_mode.as_deref(), Some("100755"));
    }

    #[test]
    fn unquotes_c_style_paths() {
        assert_eq!(unquote("\"a\\tb\\\"c\\303\\251\""), "a\tb\"cé");
        assert_eq!(unquote("plain"), "plain");
        let (old, new) = split_diff_git_paths("\"a/x y\" \"b/x y\"");
        assert_eq!(old.as_deref(), Some("x y"));
        assert_eq!(new.as_deref(), Some("x y"));
    }

    #[test]
    fn rejects_malformed_hunk_headers() {
        assert!(parse_hunk_header("@@ nonsense @@").is_err());
        let hunk = parse_hunk_header("@@ -3 +4,0 @@").unwrap();
        assert_eq!(
            (
                hunk.old_start,
                hunk.old_count,
                hunk.new_start,
                hunk.new_count
            ),
            (3, 1, 4, 0)
        );
    }
}
