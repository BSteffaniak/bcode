//! Git command execution and argument validation for repository reads.

use super::DataError;
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

/// Git processes running at once from this process: enough to keep every core busy, few enough
/// that a burst of background work (indexing every layer of a stack) cannot starve the machine or
/// the interactive request queued behind it for long.
fn process_slots() -> usize {
    std::thread::available_parallelism()
        .map_or(4, std::num::NonZeroUsize::get)
        .clamp(2, 16)
}

/// A counting semaphore bounding concurrent git processes.
struct Slots {
    used: std::sync::Mutex<usize>,
    freed: std::sync::Condvar,
}

/// One running git process's slot; freed on drop.
struct Slot;

static SLOTS: Slots = Slots {
    used: std::sync::Mutex::new(0),
    freed: std::sync::Condvar::new(),
};

impl Slot {
    /// Wait for a free slot.
    fn acquire() -> Self {
        let max = process_slots();
        let mut used = SLOTS
            .used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *used >= max {
            used = SLOTS
                .freed
                .wait(used)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *used += 1;
        Self
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        {
            let mut used = SLOTS
                .used
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *used = used.saturating_sub(1);
        }
        SLOTS.freed.notify_one();
    }
}

/// Git processes started per repository (canonical path) and their subcommands, in tests: what
/// budgets assert, because how many processes a path starts does not depend on the machine.
#[cfg(test)]
static SPAWNED: std::sync::Mutex<std::collections::BTreeMap<std::path::PathBuf, Vec<String>>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// The subcommands of the git processes started for `repo_root` so far, in order.
#[cfg(test)]
pub fn spawned(repo_root: &Path) -> Vec<String> {
    let root = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    SPAWNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&root)
        .cloned()
        .unwrap_or_default()
}

/// Global options for commands whose every pathspec carries explicit magic (`:(literal)…`,
/// `:(glob)…`, `:(exclude,literal)…`): [`GLOBAL_ARGS`] without `--literal-pathspecs`, which
/// would read the magic itself as part of a path.
const MAGIC_GLOBAL_ARGS: [&str; 2] = ["-c", "core.quotepath=false"];

/// Build a git command for `repo_root` with the service's global options.
pub fn command(repo_root: &Path, args: &[&str]) -> Command {
    build(repo_root, &GLOBAL_ARGS, args)
}

/// Build a git command whose pathspecs all carry explicit magic (see [`MAGIC_GLOBAL_ARGS`]).
/// Never pass a plain path to it: it would be read as a pattern.
pub fn magic_command(repo_root: &Path, args: &[&str]) -> Command {
    build(repo_root, &MAGIC_GLOBAL_ARGS, args)
}

fn build(repo_root: &Path, globals: &[&str], args: &[&str]) -> Command {
    #[cfg(test)]
    {
        let root = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
        SPAWNED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(root)
            .or_default()
            .push(args.first().copied().unwrap_or_default().to_owned());
    }
    let mut command = Command::new("git");
    command
        .args(globals)
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
/// * [`DataError::Io`] when git cannot be started.
/// * [`DataError::Git`] when git exits unsuccessfully.
pub fn output(repo_root: &Path, args: &[&str]) -> Result<Vec<u8>, DataError> {
    let _slot = Slot::acquire();
    let output = command(repo_root, args).output()?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    Err(DataError::Git(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

/// The first NUL-terminated records of a git command's output, how many it printed in all, and
/// a digest of all of it.
#[derive(Debug, Default)]
pub struct LimitedRecords {
    /// The first records, at most the limit asked for.
    pub records: Vec<String>,
    /// Records printed in all.
    pub total: u64,
    /// SHA-256 of the whole output.
    pub digest: Vec<u8>,
}

/// Run `command` (built by [`command`] or [`magic_command`]) and keep its first `keep`
/// NUL-terminated records; the rest is read, counted and digested, never held, so a listing of
/// millions of paths costs no more memory than `keep` of them.
///
/// # Errors
///
/// * [`DataError::Io`] when git cannot be started or read.
/// * [`DataError::Git`] when git exits unsuccessfully.
pub fn limited_records(command: Command, keep: usize) -> Result<LimitedRecords, DataError> {
    limited_records_where(command, keep, |_| true)
}

/// [`limited_records`] of the records that name files: `git ls-files --others` prints a nested
/// repository (an untracked clone, say) as one record ending in `/`, which is digested but
/// neither kept nor counted.
///
/// # Errors
///
/// As [`limited_records`].
pub fn limited_file_records(command: Command, keep: usize) -> Result<LimitedRecords, DataError> {
    limited_records_where(command, keep, |record| !record.ends_with(b"/"))
}

/// Count the file records (not ending in `/`) `command` prints, up to `cap`: past it git is
/// stopped and the answer is `(cap, true)`, so counting a huge ignored folder costs at most
/// `cap` records.
///
/// # Errors
///
/// * [`DataError::Io`] when git cannot be started or read.
/// * [`DataError::Git`] when git exits unsuccessfully before the cap.
pub fn count_file_records(mut command: Command, cap: u64) -> Result<(u64, bool), DataError> {
    use std::io::BufRead as _;
    let _slot = Slot::acquire();
    command.stderr(std::process::Stdio::null());
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| DataError::Git("git stdout unavailable".to_owned()))?;
    let mut count = 0_u64;
    for record in std::io::BufReader::new(stdout).split(0) {
        if record?.ends_with(b"/") {
            continue;
        }
        count += 1;
        if count > cap {
            let _ = child.kill();
            let _ = child.wait();
            return Ok((cap, true));
        }
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(DataError::Git(format!("git exited with {status}")));
    }
    Ok((count, false))
}

/// [`limited_records`] of the records `accept` takes: the rest are digested but neither kept
/// nor counted.
///
/// # Errors
///
/// As [`limited_records`].
pub fn limited_records_matching(
    command: Command,
    keep: usize,
    accept: impl Fn(&[u8]) -> bool,
) -> Result<LimitedRecords, DataError> {
    limited_records_where(command, keep, accept)
}

fn limited_records_where(
    mut command: Command,
    keep: usize,
    accept: impl Fn(&[u8]) -> bool,
) -> Result<LimitedRecords, DataError> {
    use sha2::Digest as _;
    use std::io::{BufRead as _, Read as _};
    let _slot = Slot::acquire();
    let mut child = command.spawn()?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| DataError::Git("git stderr unavailable".to_owned()))?;
    // Read on another thread so a chatty stderr cannot fill its pipe and stall stdout.
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| DataError::Git("git stdout unavailable".to_owned()))?;
    let mut hasher = sha2::Sha256::new();
    let mut result = LimitedRecords::default();
    for record in std::io::BufReader::new(stdout).split(0) {
        let record = record?;
        hasher.update(&record);
        hasher.update([0]);
        if !accept(&record) {
            continue;
        }
        result.total += 1;
        if result.records.len() < keep {
            result
                .records
                .push(String::from_utf8_lossy(&record).into_owned());
        }
    }
    let status = child.wait()?;
    let errors = errors.join().unwrap_or_default();
    if !status.success() {
        return Err(DataError::Git(errors.trim().to_owned()));
    }
    result.digest = hasher.finalize().to_vec();
    Ok(result)
}

/// The first `keep` records of `command`'s output, split at `separator`, and whether it printed
/// more: past `keep` git is stopped, so a search with millions of matches costs no more than
/// `keep` of them. An exit status of 1 with no output (nothing matched, as `git grep` says) is an
/// empty answer, not an error.
///
/// # Errors
///
/// * [`DataError::Io`] when git cannot be started or read.
/// * [`DataError::Git`] when git exits unsuccessfully otherwise.
pub fn first_records(
    mut command: Command,
    keep: usize,
    separator: u8,
) -> Result<(Vec<String>, bool), DataError> {
    use std::io::{BufRead as _, Read as _};
    let _slot = Slot::acquire();
    let mut child = command.spawn()?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| DataError::Git("git stderr unavailable".to_owned()))?;
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| DataError::Git("git stdout unavailable".to_owned()))?;
    let mut records = Vec::new();
    let mut more = false;
    for record in std::io::BufReader::new(stdout).split(separator) {
        let record = record?;
        if records.len() == keep {
            more = true;
            break;
        }
        records.push(String::from_utf8_lossy(&record).into_owned());
    }
    if more {
        let _ = child.kill();
        let _ = child.wait();
        let _ = errors.join();
        return Ok((records, true));
    }
    let status = child.wait()?;
    let errors = errors.join().unwrap_or_default();
    if status.success()
        || (status.code() == Some(1) && records.is_empty() && errors.trim().is_empty())
    {
        return Ok((records, false));
    }
    Err(DataError::Git(errors.trim().to_owned()))
}

/// Require a confined glob: non-empty, relative, without `.`/`..` or `.git` segments, and
/// without NUL or newline characters.
///
/// # Errors
///
/// * [`DataError::PathRejected`] when the glob violates any of these rules.
pub fn validate_glob(pattern: &str) -> Result<(), DataError> {
    let rejected = |reason: &str| Err(DataError::PathRejected(format!("{pattern:?}: {reason}")));
    if pattern.is_empty() {
        return rejected("empty glob");
    }
    if pattern.contains(['\0', '\n', '\r']) {
        return rejected("control character in glob");
    }
    if pattern.starts_with('/') || pattern.starts_with(':') {
        return rejected("absolute glob");
    }
    if pattern
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return rejected("relative directory component");
    }
    if pattern
        .split('/')
        .any(|segment| segment.eq_ignore_ascii_case(".git"))
    {
        return rejected("repository metadata component");
    }
    Ok(())
}

/// Run git with extra environment variables and return its standard output.
///
/// # Errors
///
/// * [`DataError::Io`] when git cannot be started.
/// * [`DataError::Git`] when git exits unsuccessfully.
pub fn output_with_env(
    repo_root: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> Result<Vec<u8>, DataError> {
    let _slot = Slot::acquire();
    let mut command = command(repo_root, args);
    command.envs(env.iter().copied());
    let output = command.output()?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    Err(DataError::Git(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

/// Run git with `input` on its standard input and return its standard output.
///
/// # Errors
///
/// * [`DataError::Io`] when git cannot be started or written to.
/// * [`DataError::Git`] when git exits unsuccessfully.
pub fn output_with_input(
    repo_root: &Path,
    args: &[&str],
    input: &str,
) -> Result<Vec<u8>, DataError> {
    use std::io::Write as _;
    let _slot = Slot::acquire();
    let mut child = command(repo_root, args).stdin(Stdio::piped()).spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| DataError::Git("git stdin unavailable".to_owned()))?;
    let input = input.to_owned();
    // Written on another thread so a large input cannot fill both pipes and stall.
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child.wait_with_output()?;
    writer
        .join()
        .map_err(|_| DataError::Git("git stdin writer panicked".to_owned()))??;
    if output.status.success() {
        return Ok(output.stdout);
    }
    Err(DataError::Git(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

/// Run git and return its standard output, or `None` when it exits unsuccessfully without
/// writing to standard error (for example `rev-parse --verify --quiet` on a missing revision).
///
/// # Errors
///
/// * [`DataError::Io`] when git cannot be started.
/// * [`DataError::Git`] when git fails with an error message.
pub fn optional_output(repo_root: &Path, args: &[&str]) -> Result<Option<Vec<u8>>, DataError> {
    let _slot = Slot::acquire();
    let output = command(repo_root, args).output()?;
    if output.status.success() {
        return Ok(Some(output.stdout));
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        return Ok(None);
    }
    Err(DataError::Git(stderr))
}

/// Resolve a revision expression to a full commit id, or `None` when it does not exist.
///
/// # Errors
///
/// * [`DataError::InvalidRequest`] when the revision looks like an option.
/// * [`DataError::Io`] / [`DataError::Git`] when git fails unexpectedly.
pub fn resolve_commit(repo_root: &Path, rev: &str) -> Result<Option<String>, DataError> {
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
/// * [`DataError::Io`] / [`DataError::Git`] when git fails unexpectedly.
pub fn merge_base(repo_root: &Path, left: &str, right: &str) -> Result<Option<String>, DataError> {
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
/// * [`DataError::Io`] / [`DataError::Git`] when git fails.
pub fn empty_tree_oid(repo_root: &Path) -> Result<String, DataError> {
    let mut child = command(repo_root, &["hash-object", "-t", "tree", "--stdin"])
        .stdin(Stdio::piped())
        .spawn()?;
    // Dropping stdin closes it, so git hashes empty input.
    drop(child.stdin.take());
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(DataError::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Reject revision expressions that git could parse as options.
///
/// # Errors
///
/// * [`DataError::InvalidRequest`] for empty revisions, revisions starting with `-`, and
///   revisions containing control characters.
pub fn validate_revision(rev: &str) -> Result<(), DataError> {
    if rev.is_empty() || rev.starts_with('-') || rev.chars().any(char::is_control) {
        return Err(DataError::InvalidRequest(format!(
            "invalid revision: {rev:?}"
        )));
    }
    Ok(())
}

/// Require a hexadecimal object id (abbreviated ids of at least 4 digits are accepted).
///
/// # Errors
///
/// * [`DataError::InvalidRequest`] when `oid` is not 4 to 64 hexadecimal digits.
pub fn validate_oid(oid: &str) -> Result<(), DataError> {
    if (4..=64).contains(&oid.len()) && oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(());
    }
    Err(DataError::InvalidRequest(format!(
        "invalid object id: {oid:?}"
    )))
}

/// Require a confined repository-relative path: non-empty, relative, without `.`/`..` or `.git`
/// components, and without NUL or newline characters.
///
/// # Errors
///
/// * [`DataError::PathRejected`] when the path violates any of these rules.
pub fn validate_relative_path(path: &str) -> Result<(), DataError> {
    let rejected = |reason: &str| Err(DataError::PathRejected(format!("{path:?}: {reason}")));
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
                    Err(DataError::PathRejected(_))
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
