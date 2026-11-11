//! Git command execution and argument validation for the V2 data layer.

use super::ServiceError;
use std::path::{Component, Path};
use std::process::{Command, Stdio};

/// Global options applied to every service git invocation.
///
/// * `core.quotepath=false` keeps non-ASCII paths unescaped in non-`-z` output.
/// * `--literal-pathspecs` treats every path argument literally (no pathspec magic or globs).
///
/// Optional locks stay enabled on purpose: working-tree diffs and `git status` may then write
/// refreshed stat information back to the index, so later calls do not re-hash every file.
const GLOBAL_ARGS: [&str; 3] = ["-c", "core.quotepath=false", "--literal-pathspecs"];

/// Build a git command for `repo_root` with the service's global options.
pub fn command(repo_root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .args(GLOBAL_ARGS)
        .args(args)
        .current_dir(repo_root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Run git and return its standard output.
///
/// # Errors
///
/// * [`ServiceError::Io`] when git cannot be started.
/// * [`ServiceError::Git`] when git exits unsuccessfully.
pub fn output(repo_root: &Path, args: &[&str]) -> Result<Vec<u8>, ServiceError> {
    let output = command(repo_root, args).output()?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    Err(ServiceError::Git(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

/// Run git and return its standard output, or `None` when it exits unsuccessfully without
/// writing to standard error (for example `rev-parse --verify --quiet` on a missing revision).
///
/// # Errors
///
/// * [`ServiceError::Io`] when git cannot be started.
/// * [`ServiceError::Git`] when git fails with an error message.
pub fn optional_output(repo_root: &Path, args: &[&str]) -> Result<Option<Vec<u8>>, ServiceError> {
    let output = command(repo_root, args).output()?;
    if output.status.success() {
        return Ok(Some(output.stdout));
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        return Ok(None);
    }
    Err(ServiceError::Git(stderr))
}

/// Resolve a revision expression to a full commit id, or `None` when it does not exist.
///
/// # Errors
///
/// * [`ServiceError::InvalidRequest`] when the revision looks like an option.
/// * [`ServiceError::Io`] / [`ServiceError::Git`] when git fails unexpectedly.
pub fn resolve_commit(repo_root: &Path, rev: &str) -> Result<Option<String>, ServiceError> {
    validate_revision(rev)?;
    let spec = format!("{rev}^{{commit}}");
    Ok(optional_output(
        repo_root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &spec,
        ],
    )?
    .map(|stdout| String::from_utf8_lossy(&stdout).trim().to_string())
    .filter(|oid| !oid.is_empty()))
}

/// Return the merge base of two commits, or `None` when they share no history.
///
/// # Errors
///
/// * [`ServiceError::Io`] / [`ServiceError::Git`] when git fails unexpectedly.
pub fn merge_base(
    repo_root: &Path,
    left: &str,
    right: &str,
) -> Result<Option<String>, ServiceError> {
    validate_oid(left)?;
    validate_oid(right)?;
    Ok(optional_output(repo_root, &["merge-base", left, right])?
        .map(|stdout| String::from_utf8_lossy(&stdout).trim().to_string())
        .filter(|oid| !oid.is_empty()))
}

/// Return the object id of the empty tree in this repository's hash format.
///
/// # Errors
///
/// * [`ServiceError::Io`] / [`ServiceError::Git`] when git fails.
pub fn empty_tree_oid(repo_root: &Path) -> Result<String, ServiceError> {
    let mut child = command(repo_root, &["hash-object", "-t", "tree", "--stdin"])
        .stdin(Stdio::piped())
        .spawn()?;
    // Dropping stdin closes it, so git hashes empty input.
    drop(child.stdin.take());
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(ServiceError::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Reject revision expressions that git could parse as options.
///
/// # Errors
///
/// * [`ServiceError::InvalidRequest`] for empty revisions, revisions starting with `-`, and
///   revisions containing control characters.
pub fn validate_revision(rev: &str) -> Result<(), ServiceError> {
    if rev.is_empty() || rev.starts_with('-') || rev.chars().any(char::is_control) {
        return Err(ServiceError::InvalidRequest(format!(
            "invalid revision: {rev:?}"
        )));
    }
    Ok(())
}

/// Require a hexadecimal object id (abbreviated ids of at least 4 digits are accepted).
///
/// # Errors
///
/// * [`ServiceError::InvalidRequest`] when `oid` is not 4 to 64 hexadecimal digits.
pub fn validate_oid(oid: &str) -> Result<(), ServiceError> {
    if (4..=64).contains(&oid.len()) && oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(());
    }
    Err(ServiceError::InvalidRequest(format!(
        "invalid object id: {oid:?}"
    )))
}

/// Require a confined repository-relative path: non-empty, relative, without `.`/`..` or `.git`
/// components, and without NUL or newline characters.
///
/// # Errors
///
/// * [`ServiceError::PathRejected`] when the path violates any of these rules.
pub fn validate_relative_path(path: &str) -> Result<(), ServiceError> {
    let rejected = |reason: &str| Err(ServiceError::PathRejected(format!("{path:?}: {reason}")));
    if path.is_empty() {
        return rejected("empty path");
    }
    if path.contains(['\0', '\n', '\r']) {
        return rejected("control character in path");
    }
    // `Path::components` drops interior `.` segments, so check the raw segments too.
    if path
        .split(['/', '\\'])
        .any(|segment| segment == "." || segment == "..")
    {
        return rejected("relative directory component");
    }
    // Git never tracks paths inside a `.git` directory; reading them would expose repository
    // internals such as configuration.
    if path
        .split(['/', '\\'])
        .any(|segment| segment.eq_ignore_ascii_case(".git"))
    {
        return rejected("repository metadata component");
    }
    let candidate = Path::new(path);
    if candidate.is_absolute() || path.starts_with('/') || path.starts_with('\\') {
        return rejected("absolute path");
    }
    for component in candidate.components() {
        match component {
            Component::Normal(_) => {}
            Component::ParentDir => return rejected("parent directory component"),
            Component::CurDir => return rejected("current directory component"),
            Component::RootDir | Component::Prefix(_) => return rejected("absolute path"),
        }
    }
    Ok(())
}

/// Whether an object id consists only of zeros (git's marker for "no object").
#[must_use]
pub fn is_null_oid(oid: &str) -> bool {
    !oid.is_empty() && oid.bytes().all(|byte| byte == b'0')
}

/// Split NUL-terminated output into lossy UTF-8 tokens, dropping the trailing empty token.
#[must_use]
pub fn split_nul(bytes: &[u8]) -> Vec<String> {
    let mut tokens: Vec<String> = bytes
        .split(|byte| *byte == 0)
        .map(|token| String::from_utf8_lossy(token).into_owned())
        .collect();
    if tokens.last().is_some_and(String::is_empty) {
        tokens.pop();
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_reject_escapes_and_absolute_forms() {
        for path in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a/./b",
            "a\nb",
            "a\0b",
            ".git/config",
            "sub/.GIT/HEAD",
        ] {
            assert!(
                matches!(
                    validate_relative_path(path),
                    Err(ServiceError::PathRejected(_))
                ),
                "{path:?}"
            );
        }
        for path in [
            "a.rs",
            "dir/sub/file.txt",
            "docs/unicode 漢字.txt",
            "..hidden",
            ".github/workflows/ci.yml",
        ] {
            assert!(validate_relative_path(path).is_ok(), "{path:?}");
        }
    }

    #[test]
    fn revisions_and_oids_reject_option_injection() {
        assert!(validate_revision("--output=/tmp/x").is_err());
        assert!(validate_revision("main~2").is_ok());
        assert!(validate_oid("--all").is_err());
        assert!(validate_oid("abc").is_err());
        assert!(validate_oid("deadbeef").is_ok());
    }
}
