//! Code search and path listings at a pinned revision: `git grep` and `git ls-tree` (or the
//! working tree and index), bounded so a search with many matches or a huge tree costs no more
//! than what is kept.

use super::DataError;
use super::git;
use bcode_code_review_models::RevisionRef;
use std::path::Path;

/// Characters of a matching line kept.
pub const MAX_HIT_TEXT_CHARS: usize = 300;
/// Bytes of a matched record kept: its path and line number, and room for
/// [`MAX_HIT_TEXT_CHARS`] characters of text; the rest of a long line is never held.
const MAX_HIT_RECORD_BYTES: usize = 8 * 1024 + MAX_HIT_TEXT_CHARS * 4;
/// Path patterns one search takes at most.
pub const MAX_PATH_PATTERNS: usize = 20;

/// How a search pattern matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrepMode {
    /// The text as it is.
    Fixed,
    /// The text as a whole word.
    Word,
    /// A POSIX extended regular expression.
    Regex,
}

/// One search.
#[derive(Debug, Clone, Copy)]
pub struct GrepQuery<'a> {
    /// The pattern.
    pub pattern: &'a str,
    /// How it matches.
    pub mode: GrepMode,
    /// Whether case is ignored.
    pub ignore_case: bool,
    /// Path globs the search is limited to (`*` within a directory, `**` across); empty for
    /// the whole tree.
    pub paths: &'a [String],
}

/// One matching line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepHit {
    /// Repository-relative path.
    pub path: String,
    /// Line (1-based).
    pub line: u32,
    /// The line, cut to [`MAX_HIT_TEXT_CHARS`].
    pub text: String,
}

/// The revision arguments of a search or listing of `revision`; `None` for the empty tree,
/// which has nothing in it.
fn revision_args(revision: &RevisionRef) -> Result<Option<Vec<String>>, DataError> {
    Ok(match revision {
        RevisionRef::Commit { oid } => {
            git::validate_oid(oid)?;
            Some(vec![oid.clone()])
        }
        RevisionRef::Index => Some(vec!["--cached".to_owned()]),
        RevisionRef::WorkingTree => Some(vec!["--untracked".to_owned()]),
        RevisionRef::EmptyTree => None,
    })
}

/// `paths` as glob pathspecs, each checked to stay inside the repository.
fn pathspecs(paths: &[String]) -> Result<Vec<String>, DataError> {
    if paths.len() > MAX_PATH_PATTERNS {
        return Err(DataError::InvalidRequest(format!(
            "at most {MAX_PATH_PATTERNS} path patterns per search"
        )));
    }
    paths
        .iter()
        .map(|path| {
            let path = path.trim().trim_start_matches("./").trim_end_matches('/');
            git::validate_glob(path)?;
            Ok(format!(":(glob){path}"))
        })
        .collect()
}

fn grep_command(
    repo_root: &Path,
    revision: &[String],
    query: &GrepQuery<'_>,
    files_only: bool,
) -> Result<std::process::Command, DataError> {
    if query.pattern.is_empty() || query.pattern.contains(['\n', '\0']) {
        return Err(DataError::InvalidRequest(
            "the search pattern must be one non-empty line".to_owned(),
        ));
    }
    let specs = pathspecs(query.paths)?;
    let mut args: Vec<&str> = vec!["grep", "-I", "--no-color", "-z"];
    args.push(if files_only { "-l" } else { "-n" });
    match query.mode {
        GrepMode::Fixed => args.push("-F"),
        GrepMode::Word => args.extend(["-F", "-w"]),
        GrepMode::Regex => args.push("-E"),
    }
    if query.ignore_case {
        args.push("-i");
    }
    args.extend(["-e", query.pattern]);
    args.extend(revision.iter().map(String::as_str));
    args.push("--");
    args.extend(specs.iter().map(String::as_str));
    Ok(git::magic_command(repo_root, &args))
}

/// The path of a `git grep` record: without the `<oid>:` a commit search prefixes.
fn record_path<'a>(record: &'a str, revision: &RevisionRef) -> &'a str {
    match revision {
        RevisionRef::Commit { oid } => record
            .strip_prefix(oid.as_str())
            .and_then(|rest| rest.strip_prefix(':'))
            .unwrap_or(record),
        _ => record,
    }
}

/// Lines matching `query` at `revision`, in path and line order, at most `max`; and whether
/// there were more.
///
/// # Errors
///
/// * [`DataError::InvalidRequest`] for an empty or multi-line pattern, or too many path
///   patterns; [`DataError::PathRejected`] for a path pattern leaving the repository.
/// * [`DataError::Git`] / [`DataError::Io`] when git fails (an invalid regular expression
///   says so here).
pub fn grep(
    repo_root: &Path,
    revision: &RevisionRef,
    query: &GrepQuery<'_>,
    max: usize,
) -> Result<(Vec<GrepHit>, bool), DataError> {
    let Some(revision_args) = revision_args(revision)? else {
        return Ok((Vec::new(), false));
    };
    let command = grep_command(repo_root, &revision_args, query, false)?;
    let (records, more) = git::first_records(command, max, b'\n', MAX_HIT_RECORD_BYTES)?;
    let hits = records
        .iter()
        .filter_map(|record| {
            let mut fields = record.splitn(3, '\0');
            let path = record_path(fields.next()?, revision).to_owned();
            let line = fields.next()?.parse().ok()?;
            let text: String = fields.next()?.chars().take(MAX_HIT_TEXT_CHARS).collect();
            Some(GrepHit { path, line, text })
        })
        .collect();
    Ok((hits, more))
}

/// Paths of files matching `query` at `revision`, at most `max`; and whether there were more.
///
/// # Errors
///
/// As [`grep`].
pub fn grep_files(
    repo_root: &Path,
    revision: &RevisionRef,
    query: &GrepQuery<'_>,
    max: usize,
) -> Result<(Vec<String>, bool), DataError> {
    let Some(revision_args) = revision_args(revision)? else {
        return Ok((Vec::new(), false));
    };
    let command = grep_command(repo_root, &revision_args, query, true)?;
    let (records, more) = git::first_records(command, max, b'\0', usize::MAX)?;
    Ok((
        records
            .iter()
            .filter(|record| !record.is_empty())
            .map(|record| record_path(record, revision).to_owned())
            .collect(),
        more,
    ))
}

/// Paths of every file at `revision` that `accept` takes, at most `keep` (the listing is read
/// in a stream, so a huge tree costs no more than what is kept); and how many it took in all.
///
/// # Errors
///
/// * [`DataError::Git`] / [`DataError::Io`] when git fails.
pub fn list_paths(
    repo_root: &Path,
    revision: &RevisionRef,
    keep: usize,
    accept: impl Fn(&str) -> bool,
) -> Result<(Vec<String>, u64), DataError> {
    let args: Vec<String> = match revision {
        RevisionRef::Commit { oid } => {
            git::validate_oid(oid)?;
            ["ls-tree", "-r", "-z", "--name-only", oid.as_str()]
                .map(str::to_owned)
                .to_vec()
        }
        RevisionRef::Index => ["ls-files", "-z", "--cached"].map(str::to_owned).to_vec(),
        RevisionRef::WorkingTree => [
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--deduplicate",
        ]
        .map(str::to_owned)
        .to_vec(),
        RevisionRef::EmptyTree => return Ok((Vec::new(), 0)),
    };
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let listed = git::limited_records_matching(git::command(repo_root, &args), keep, |record| {
        std::str::from_utf8(record).is_ok_and(&accept)
    })?;
    Ok((listed.records, listed.total))
}

/// Characters a regular expression gives meaning to, escaped so `text` matches as it is.
#[must_use]
pub fn escape_regex(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if "\\^$.|?*+()[]{}".contains(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// Whether `symbol` is a plain identifier (letters, digits, `_`, not starting with a digit),
/// which symbol searches require.
#[must_use]
pub fn is_identifier(symbol: &str) -> bool {
    let mut characters = symbol.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// A POSIX extended regular expression matching common definitions of `symbol` (an
/// identifier): Rust items and impls, Python functions and classes, JavaScript and TypeScript
/// declarations, Go functions and types.
#[must_use]
pub fn definition_pattern(symbol: &str) -> String {
    let name = escape_regex(symbol);
    let before = "(^|[^A-Za-z0-9_])";
    let after = "([^A-Za-z0-9_]|$)";
    let keywords = "(fn|struct|enum|trait|type|union|const|static|mod|macro_rules!|def|class|function|interface|let|var|func)";
    format!(
        "{before}{keywords}[[:space:]]+{name}{after}|{before}impl(<[^>]*>)?[[:space:]]+([A-Za-z0-9_:<>, ]+[[:space:]]+for[[:space:]]+)?{name}{after}|{before}func[[:space:]]+\\([^)]*\\)[[:space:]]*{name}{after}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::fixture::GitFixture;

    fn repo() -> (GitFixture, String) {
        let fixture = GitFixture::empty();
        let write = |path: &str, text: &str| {
            let full = fixture.root().join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("dir");
            std::fs::write(full, text).expect("write");
        };
        write(
            "src/budget.rs",
            "pub struct RetryBudget {\n    tokens: u32,\n}\n\nimpl RetryBudget {\n    pub fn new() -> Self { Self { tokens: 1 } }\n}\n",
        );
        write(
            "src/client.rs",
            "use crate::budget::RetryBudget;\nfn send() { let budget = RetryBudget::new(); }\n",
        );
        write("tools/retry.py", "def retry_budget():\n    pass\n");
        fixture.run(&["add", "."]);
        fixture.run(&["commit", "-q", "-m", "files"]);
        let head = fixture.git(&["rev-parse", "HEAD"]).trim().to_owned();
        (fixture, head)
    }

    #[test]
    fn searches_read_the_pinned_commit_not_the_working_tree() {
        let (fixture, head) = repo();
        std::fs::write(fixture.root().join("src/client.rs"), "nothing here\n").expect("edit");
        let revision = RevisionRef::commit(&head);
        let query = GrepQuery {
            pattern: "RetryBudget",
            mode: GrepMode::Word,
            ignore_case: false,
            paths: &[],
        };
        let (hits, more) = grep(fixture.root(), &revision, &query, 100).expect("grep");
        assert!(!more);
        assert!(
            hits.iter()
                .any(|hit| hit.path == "src/client.rs" && hit.line == 1)
        );
        let (live, _) = grep(fixture.root(), &RevisionRef::WorkingTree, &query, 100).expect("grep");
        assert!(!live.iter().any(|hit| hit.path == "src/client.rs"));
        let (none, more) = grep(
            fixture.root(),
            &revision,
            &GrepQuery {
                pattern: "absent_everywhere",
                ..query
            },
            100,
        )
        .expect("no matches is not an error");
        assert!(none.is_empty() && !more);
    }

    #[test]
    fn a_match_on_a_huge_line_keeps_only_its_start_and_the_lines_after() {
        let (fixture, _) = repo();
        let huge = format!(
            "RetryBudget{}\nRetryBudget after\n",
            "x".repeat(4 * 1024 * 1024)
        );
        std::fs::write(fixture.root().join("src/minified.js"), huge).expect("write");
        let query = GrepQuery {
            pattern: "RetryBudget",
            mode: GrepMode::Fixed,
            ignore_case: false,
            paths: &["src/minified.js".to_owned()],
        };
        let (hits, more) =
            grep(fixture.root(), &RevisionRef::WorkingTree, &query, 10).expect("grep");
        assert!(!more);
        assert_eq!(
            hits.iter()
                .map(|hit| (hit.line, hit.text.chars().count()))
                .collect::<Vec<_>>(),
            [(1, MAX_HIT_TEXT_CHARS), (2, "RetryBudget after".len())]
        );
    }

    #[test]
    fn searches_stop_at_their_bound_and_say_so() {
        let (fixture, head) = repo();
        let query = GrepQuery {
            pattern: "RetryBudget",
            mode: GrepMode::Fixed,
            ignore_case: false,
            paths: &[],
        };
        let (hits, more) =
            grep(fixture.root(), &RevisionRef::commit(&head), &query, 1).expect("grep");
        assert_eq!(hits.len(), 1);
        assert!(more);
    }

    #[test]
    fn path_globs_limit_a_search_and_cannot_leave_the_repository() {
        let (fixture, head) = repo();
        let revision = RevisionRef::commit(&head);
        let paths = vec!["tools/**".to_owned()];
        let query = GrepQuery {
            pattern: "retry",
            mode: GrepMode::Fixed,
            ignore_case: true,
            paths: &paths,
        };
        let (files, _) = grep_files(fixture.root(), &revision, &query, 10).expect("grep");
        assert_eq!(files, vec!["tools/retry.py".to_owned()]);
        let escaping = vec!["../x".to_owned()];
        assert!(
            grep(
                fixture.root(),
                &revision,
                &GrepQuery {
                    paths: &escaping,
                    ..query
                },
                10
            )
            .is_err()
        );
    }

    #[test]
    fn definitions_are_found_across_languages() {
        let (fixture, head) = repo();
        let revision = RevisionRef::commit(&head);
        let pattern = definition_pattern("RetryBudget");
        let query = GrepQuery {
            pattern: &pattern,
            mode: GrepMode::Regex,
            ignore_case: false,
            paths: &[],
        };
        let (hits, _) = grep(fixture.root(), &revision, &query, 10).expect("grep");
        let lines: Vec<(&str, u32)> = hits
            .iter()
            .map(|hit| (hit.path.as_str(), hit.line))
            .collect();
        assert_eq!(lines, vec![("src/budget.rs", 1), ("src/budget.rs", 5)]);
        let pattern = definition_pattern("retry_budget");
        let (hits, _) = grep(
            fixture.root(),
            &revision,
            &GrepQuery {
                pattern: &pattern,
                ..query
            },
            10,
        )
        .expect("grep");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "tools/retry.py");
        assert!(is_identifier("retry_budget") && !is_identifier("a b") && !is_identifier("9a"));
    }

    #[test]
    fn listings_keep_only_what_is_accepted() {
        let (fixture, head) = repo();
        let (paths, total) = list_paths(fixture.root(), &RevisionRef::commit(&head), 10, |path| {
            std::path::Path::new(path)
                .extension()
                .is_some_and(|extension| extension == "rs")
        })
        .expect("list");
        assert_eq!(total, 2);
        assert_eq!(
            paths,
            vec!["src/budget.rs".to_owned(), "src/client.rs".to_owned()]
        );
    }
}
