//! What a new review reviews, as the assistant names it.
//!
//! `code_review.create_review` and the app's prefilled wizard take `sources` (and `directory`);
//! [`plan`] turns them into review source kinds and the repository the review belongs to.

use super::AssistantError;
use bcode_code_review_models::ReviewSourceKind;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Review-source provider of pull requests named by number or URL.
const GITHUB_PROVIDER: &str = "github";
/// The host of github.com repositories, written as `owner/name` without it.
const GITHUB_HOST: &str = "github.com";
/// Sources one review may have at most.
const MAX_SOURCES: usize = 50;

/// The schema of one source of `create_review`.
#[must_use]
pub fn source_schema() -> Value {
    json!({
        "type": "object",
        "description": "One thing to review. `kind` decides which other fields apply.",
        "properties": {
            "kind": {
                "type": "string",
                "enum": [
                    "pull_request", "stack", "stack_layers", "branch", "current_branch", "range",
                    "commits", "commit", "last_commit", "working_tree", "staged", "unstaged"
                ],
                "description": "pull_request: one pull request (number or url). stack: a whole stack as one diff (top = its top pull request). stack_layers: a stack stepped through one pull request at a time (layers, bottom first; prefer this for a stack). branch: head against base. current_branch: the checked-out branch against its base. range: base..head commits. commits: several commits (revs). commit / last_commit: one commit. working_tree / staged / unstaged: uncommitted changes (all, staged only, unstaged only), optionally limited to `paths`."
            },
            "number": {"type": "integer", "minimum": 1, "description": "pull_request: its number."},
            "url": {"type": "string", "description": "pull_request: its web address, instead of number."},
            "repository": {"type": "string", "description": "pull_request, stack, stack_layers: owner/name on the forge; the origin remote's when omitted."},
            "top": {"type": "integer", "minimum": 1, "description": "stack: number of its top pull request."},
            "layers": {"type": "array", "items": {"type": "integer", "minimum": 1}, "minItems": 1, "description": "stack_layers: pull request numbers from the bottom layer up."},
            "titles": {"type": "array", "items": {"type": "string"}, "description": "stack_layers: titles of the layers' pull requests, in the same order (labels only)."},
            "base": {"type": "string", "description": "branch, range, current_branch: base branch or revision."},
            "head": {"type": "string", "description": "branch, range: head branch or revision."},
            "merge_base": {"type": "boolean", "description": "branch, range: compare from their merge base (default: true for branch, false for range)."},
            "rev": {"type": "string", "description": "commit: the revision."},
            "revs": {"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "commits: revisions."},
            "paths": {"type": "array", "items": {"type": "string"}, "description": "working_tree, staged, unstaged: repository-relative directories or files to limit it to."},
            "label": {"type": "string", "description": "How the source is labelled (defaults from its kind)."}
        },
        "required": ["kind"],
        "additionalProperties": false
    })
}

/// One source as the tool receives it.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourceArgument {
    kind: String,
    number: Option<u64>,
    url: Option<String>,
    repository: Option<String>,
    top: Option<u64>,
    layers: Vec<u64>,
    titles: Vec<String>,
    base: Option<String>,
    head: Option<String>,
    merge_base: Option<bool>,
    rev: Option<String>,
    revs: Vec<String>,
    paths: Vec<String>,
    label: Option<String>,
}

/// The repository a review is created in and its sources with their labels.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// Repository root.
    pub repo_root: PathBuf,
    /// Sources and labels.
    pub sources: Vec<(ReviewSourceKind, Option<String>)>,
}

/// Turn `sources` into a plan.
///
/// The plan is in the repository of `directory` (relative to `working_directory` when
/// relative) or of the working directory. With `directory` below the repository root,
/// uncommitted-change sources are limited to it unless they name `paths`.
///
/// # Errors
///
/// * [`AssistantError::Invalid`] for an unknown kind, a missing field, an unparsable pull
///   request address, or a directory outside every repository.
pub fn plan(
    working_directory: &Path,
    directory: Option<&str>,
    sources: Vec<SourceArgument>,
) -> Result<Plan, AssistantError> {
    if sources.is_empty() {
        return Err(AssistantError::Invalid(
            "pass `sources` with at least one source".to_owned(),
        ));
    }
    if sources.len() > MAX_SOURCES {
        return Err(AssistantError::Invalid(format!(
            "a review has at most {MAX_SOURCES} sources"
        )));
    }
    let (repo_root, scope) = match directory.map(str::trim).filter(|dir| !dir.is_empty()) {
        Some(directory) => {
            let path = working_directory.join(directory);
            let path = path.canonicalize().map_err(|error| {
                AssistantError::Invalid(format!("directory {directory}: {error}"))
            })?;
            if !path.is_dir() {
                return Err(AssistantError::Invalid(format!(
                    "{directory} is not a directory"
                )));
            }
            let root = git(&path, &["rev-parse", "--show-toplevel"])
                .map(PathBuf::from)
                .ok_or_else(|| {
                    AssistantError::Invalid(format!("{directory} is not inside a git repository"))
                })?;
            let root = root.canonicalize().unwrap_or(root);
            let scope = path
                .strip_prefix(&root)
                .ok()
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                .filter(|relative| !relative.is_empty());
            (root, scope)
        }
        None => (
            git(working_directory, &["rev-parse", "--show-toplevel"])
                .map_or_else(|| working_directory.to_path_buf(), PathBuf::from),
            None,
        ),
    };
    let mut planned = Vec::new();
    for source in sources {
        planned.extend(kinds(&repo_root, scope.as_deref(), source)?);
    }
    Ok(Plan {
        repo_root,
        sources: planned,
    })
}

/// Run git in `repo`; its trimmed output when it succeeded with some.
fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|text| !text.is_empty())
}

fn required<T>(value: Option<T>, kind: &str, field: &str) -> Result<T, AssistantError> {
    value.ok_or_else(|| AssistantError::Invalid(format!("a {kind} source needs `{field}`")))
}

fn kinds(
    repo: &Path,
    scope: Option<&str>,
    source: SourceArgument,
) -> Result<Vec<(ReviewSourceKind, Option<String>)>, AssistantError> {
    let label = source.label.clone();
    let one = |kind: ReviewSourceKind| Ok(vec![(kind, label.clone())]);
    let kind = source.kind.as_str();
    match kind {
        "pull_request" => {
            let (repository, number) = match (&source.url, source.number) {
                (Some(url), _) => parse_pull_url(url)?,
                (None, Some(number)) => (repository(repo, source.repository.as_deref())?, number),
                (None, None) => {
                    return Err(AssistantError::Invalid(
                        "a pull_request source needs `number` or `url`".to_owned(),
                    ));
                }
            };
            one(ReviewSourceKind::PullRequest {
                provider: GITHUB_PROVIDER.to_owned(),
                repository,
                number,
            })
        }
        "stack" => one(ReviewSourceKind::Stack {
            provider: GITHUB_PROVIDER.to_owned(),
            repository: repository(repo, source.repository.as_deref())?,
            top: required(source.top.or(source.number), kind, "top")?,
        }),
        "stack_layers" => {
            if source.layers.is_empty() {
                return Err(AssistantError::Invalid(
                    "a stack_layers source needs `layers` (pull request numbers, bottom first)"
                        .to_owned(),
                ));
            }
            let repository = repository(repo, source.repository.as_deref())?;
            Ok(source
                .layers
                .iter()
                .enumerate()
                .map(|(index, number)| {
                    let title = source
                        .titles
                        .get(index)
                        .map(|title| format!("#{number} {}", title.trim()));
                    (
                        ReviewSourceKind::StackLayer {
                            provider: GITHUB_PROVIDER.to_owned(),
                            repository: repository.clone(),
                            number: *number,
                        },
                        title,
                    )
                })
                .collect())
        }
        "branch" => one(ReviewSourceKind::BranchCompare {
            base_branch: required(source.base, kind, "base")?,
            head_branch: required(source.head, kind, "head")?,
            merge_base: source.merge_base.unwrap_or(true),
        }),
        "current_branch" => one(ReviewSourceKind::CurrentBranch { base: source.base }),
        "range" => one(ReviewSourceKind::CommitRange {
            base: required(source.base, kind, "base")?,
            head: required(source.head, kind, "head")?,
            merge_base: source.merge_base.unwrap_or(false),
        }),
        "commits" => {
            if source.revs.is_empty() {
                return Err(AssistantError::Invalid(
                    "a commits source needs `revs`".to_owned(),
                ));
            }
            one(ReviewSourceKind::Commits { revs: source.revs })
        }
        "commit" => one(ReviewSourceKind::Commit {
            rev: required(source.rev, kind, "rev")?,
        }),
        "last_commit" => one(ReviewSourceKind::LastCommit),
        "working_tree" | "staged" | "unstaged" => {
            Ok(vec![(uncommitted(kind, scope, &source.paths), label)])
        }
        other => Err(AssistantError::Invalid(format!(
            "unknown source kind {other:?}"
        ))),
    }
}

/// An uncommitted-changes source of `kind` (`working_tree`, `staged`, `unstaged`) limited to
/// `paths`, else to `scope`, else the whole repository.
fn uncommitted(kind: &str, scope: Option<&str>, paths: &[String]) -> ReviewSourceKind {
    let mut paths: Vec<String> = paths
        .iter()
        .map(|path| path.trim().trim_start_matches("./").trim_end_matches('/'))
        .filter(|path| !path.is_empty() && *path != ".")
        .map(str::to_owned)
        .collect();
    if paths.is_empty()
        && let Some(scope) = scope
    {
        paths.push(scope.to_owned());
    }
    let (staged, unstaged) = match kind {
        "staged" => (true, false),
        "unstaged" => (false, true),
        _ => (true, true),
    };
    if paths.is_empty() {
        return match kind {
            "staged" => ReviewSourceKind::IndexStaged,
            "unstaged" => ReviewSourceKind::WorkingTreeUnstaged,
            _ => ReviewSourceKind::WorkingTreeAndIndex,
        };
    }
    ReviewSourceKind::WorkingTreePaths {
        paths,
        staged,
        unstaged,
    }
}

/// The forge repository of `repo`: `explicit`, else its `origin` remote's (`owner/name` on
/// github.com, `host/owner/name` elsewhere).
fn repository(repo: &Path, explicit: Option<&str>) -> Result<String, AssistantError> {
    if let Some(explicit) = explicit.map(str::trim).filter(|name| !name.is_empty()) {
        return Ok(explicit.to_owned());
    }
    git(repo, &["remote", "get-url", "origin"])
        .and_then(|url| bcode_forge_models::RepositoryRef::from_remote_url(&url))
        .map(|repository| {
            if repository.host == GITHUB_HOST {
                repository.full_name()
            } else {
                format!("{}/{}", repository.host, repository.full_name())
            }
        })
        .ok_or_else(|| {
            AssistantError::Invalid(
                "the repository's origin remote names no forge repository; pass `repository`"
                    .to_owned(),
            )
        })
}

/// `owner/name` (or `host/owner/name`) and the number of a pull request address such as
/// `https://github.com/owner/name/pull/12`.
fn parse_pull_url(url: &str) -> Result<(String, u64), AssistantError> {
    let invalid = || {
        AssistantError::Invalid(format!(
            "{url} is not a pull request address like https://github.com/owner/name/pull/12"
        ))
    };
    let rest = url
        .trim()
        .strip_prefix("https://")
        .or_else(|| url.trim().strip_prefix("http://"))
        .ok_or_else(invalid)?;
    let parts: Vec<&str> = rest.split('/').filter(|part| !part.is_empty()).collect();
    let [host, owner, name, marker, number, ..] = parts.as_slice() else {
        return Err(invalid());
    };
    if *marker != "pull" && *marker != "pulls" {
        return Err(invalid());
    }
    let number = number
        .split(['#', '?'])
        .next()
        .and_then(|number| number.parse::<u64>().ok())
        .ok_or_else(invalid)?;
    let repository = if *host == GITHUB_HOST {
        format!("{owner}/{name}")
    } else {
        format!("{host}/{owner}/{name}")
    };
    Ok((repository, number))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_request_addresses_parse_on_github_and_enterprise_hosts() {
        assert_eq!(
            parse_pull_url("https://github.com/o/r/pull/12").expect("url"),
            ("o/r".to_owned(), 12)
        );
        assert_eq!(
            parse_pull_url("https://git.corp/o/r/pull/7/files").expect("url"),
            ("git.corp/o/r".to_owned(), 7)
        );
        assert!(parse_pull_url("https://github.com/o/r/issues/3").is_err());
        assert!(parse_pull_url("o/r#3").is_err());
    }
}
