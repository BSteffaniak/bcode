//! What a new review reviews, as the assistant names it.
//!
//! `code_review.create_review` and the app's prefilled wizard take `sources` (and `directory`);
//! [`plan`] turns them into review source kinds and the repository the review belongs to.

use super::AssistantError;
use crate::selectors;
use bcode_code_review_models::{PathSelector, ReviewSourceKind};
use serde::{Deserialize, Serialize};
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
                    "commits", "commit", "last_commit", "working_tree", "staged", "unstaged",
                    "files", "repository"
                ],
                "description": "pull_request: one pull request (number or url). stack: a whole stack as one diff (top = its top pull request). stack_layers: a stack stepped through one pull request at a time (layers, bottom first; prefer this for a stack). branch: head against base. current_branch: the checked-out branch against its base. range: base..head commits. commits: several commits (revs). commit / last_commit: one commit. working_tree / staged / unstaged: uncommitted changes (all, staged only, unstaged only), optionally limited to `paths`. files: whole files, folders, globs or line ranges as they are now, not a change (`paths`, optional `exclude`); a folder is listed again whenever the review opens. repository: the whole repository, or some folders (`paths`), as it is now."
            },
            "number": {"type": "integer", "minimum": 1, "description": "pull_request: its number."},
            "url": {"type": "string", "description": "pull_request: its web address, instead of number."},
            "repository": {"type": "string", "description": "pull_request, stack, stack_layers: owner/name on the forge; the origin remote's when omitted."},
            "provider": {"type": "string", "description": "pull_request, stack, stack_layers: the review-source provider id, as find_pull_requests reports it (default github)."},
            "top": {"type": "integer", "minimum": 1, "description": "stack: number of its top pull request."},
            "layers": {"type": "array", "items": {"type": "integer", "minimum": 1}, "minItems": 1, "description": "stack_layers: pull request numbers from the bottom layer up."},
            "titles": {"type": "array", "items": {"type": "string"}, "description": "stack_layers: titles of the layers' pull requests, in the same order (labels only)."},
            "base": {"type": "string", "description": "branch, range, current_branch: base branch or revision."},
            "head": {"type": "string", "description": "branch, range: head branch or revision."},
            "merge_base": {"type": "boolean", "description": "branch, range: compare from their merge base (default: true for branch, false for range)."},
            "rev": {"type": "string", "description": "commit: the revision."},
            "revs": {"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "commits: revisions."},
            "paths": {"type": "array", "items": {"type": "string"}, "description": "working_tree, staged, unstaged: repository-relative directories or files to limit it to. files: what to review, repository-relative: `path` (a file or a folder), `folder/`, a glob such as `src/**/*.rs`, or `path:START-END` for lines of a file. repository: folders to limit it to."},
            "exclude": {"type": "array", "items": {"type": "string"}, "description": "files: files and folders to leave out of its folders and globs."},
            "label": {"type": "string", "description": "How the source is labelled (defaults from its kind)."}
        },
        "required": ["kind"],
        "additionalProperties": false
    })
}

/// One source as the tool receives it.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourceArgument {
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    layers: Vec<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    titles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_base: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rev: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    revs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    paths: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    exclude: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
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
    let (repo_root, scope) = repository_of(working_directory, directory)?;
    let mut planned = Vec::new();
    for source in sources {
        planned.extend(kinds(&repo_root, scope.as_deref(), source)?);
    }
    Ok(Plan {
        repo_root,
        sources: planned,
    })
}

/// The repository a tool call acts on, and the folder below its root that `directory` names.
///
/// The repository is that of `directory` (relative to `working_directory` when relative), else
/// of the working directory.
///
/// # Errors
///
/// * [`AssistantError::Invalid`] for a directory that does not exist or is outside every
///   repository.
pub fn repository_of(
    working_directory: &Path,
    directory: Option<&str>,
) -> Result<(PathBuf, Option<String>), AssistantError> {
    Ok(
        match directory.map(str::trim).filter(|dir| !dir.is_empty()) {
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
                        AssistantError::Invalid(format!(
                            "{directory} is not inside a git repository"
                        ))
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
        },
    )
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
    let provider = source
        .provider
        .clone()
        .filter(|provider| !provider.trim().is_empty())
        .unwrap_or_else(|| GITHUB_PROVIDER.to_owned());
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
                provider,
                repository,
                number,
            })
        }
        "stack" => one(ReviewSourceKind::Stack {
            provider,
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
                    // One layer without a title keeps the source's label.
                    let title = source
                        .titles
                        .get(index)
                        .map(|title| format!("#{number} {}", title.trim()))
                        .or_else(|| label.clone().filter(|_| source.layers.len() == 1));
                    (
                        ReviewSourceKind::StackLayer {
                            provider: provider.clone(),
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
        "files" => one(files_kind(repo, scope, &source)?),
        "repository" => one(repository_kind(repo, scope, &source)?),
        other => Err(AssistantError::Invalid(format!(
            "unknown source kind {other:?}"
        ))),
    }
}

/// A `files` source: its `paths` (else the folder `scope`) minus its `exclude`.
fn files_kind(
    repo: &Path,
    scope: Option<&str>,
    source: &SourceArgument,
) -> Result<ReviewSourceKind, AssistantError> {
    let mut include = selectors(repo, &source.paths)?;
    if include.is_empty()
        && let Some(scope) = scope
    {
        include.push(PathSelector::Folder {
            path: scope.to_owned(),
        });
    }
    if include.is_empty() {
        return Err(AssistantError::Invalid(
            "a files source needs `paths` (files, folders, globs or ranges)".to_owned(),
        ));
    }
    let exclude = selectors(repo, &source.exclude)?;
    crate::data::validate_selectors(&include, &exclude)
        .map_err(|error| AssistantError::Invalid(error.to_string()))?;
    Ok(selectors::files_source(include, exclude))
}

/// A `repository` source: the whole repository, else its `paths` folders (else `scope`).
fn repository_kind(
    repo: &Path,
    scope: Option<&str>,
    source: &SourceArgument,
) -> Result<ReviewSourceKind, AssistantError> {
    let mut folders = selectors(repo, &source.paths)?;
    if folders.is_empty()
        && let Some(scope) = scope
    {
        folders.push(PathSelector::Folder {
            path: scope.to_owned(),
        });
    }
    selectors::repository_source(folders).map_err(AssistantError::Invalid)
}

/// `texts` in the selector syntax, relative to the repository root `repo` (an absolute path
/// inside it is taken as it is).
fn selectors(repo: &Path, texts: &[String]) -> Result<Vec<PathSelector>, AssistantError> {
    let root = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    texts
        .iter()
        .filter(|text| !text.trim().is_empty())
        .map(|text| selectors::parse_selector(&root, &root, text).map_err(AssistantError::Invalid))
        .collect()
}

/// The `create_review` source that creates `kind`, labelled `label`.
///
/// Every kind a review can have can be created by the assistant. `None` for a kind no source
/// can name: an unsupported kind, or uncommitted changes without staged or unstaged ones.
#[must_use]
// One arm per source kind: a conversion table.
#[allow(clippy::too_many_lines)]
pub fn source_argument(kind: &ReviewSourceKind, label: Option<&str>) -> Option<SourceArgument> {
    let of = |kind: &str| SourceArgument {
        kind: kind.to_owned(),
        label: label.map(str::to_owned),
        ..SourceArgument::default()
    };
    let provider = |provider: &str| (provider != GITHUB_PROVIDER).then(|| provider.to_owned());
    // Exhaustive on purpose: a new kind does not compile until the tool can create it.
    Some(match kind {
        ReviewSourceKind::WorkingTreeUnstaged => of("unstaged"),
        ReviewSourceKind::IndexStaged => of("staged"),
        ReviewSourceKind::WorkingTreeAndIndex => of("working_tree"),
        ReviewSourceKind::WorkingTreePaths {
            paths,
            staged,
            unstaged,
        } => SourceArgument {
            paths: paths.clone(),
            ..of(match (staged, unstaged) {
                (true, true) => "working_tree",
                (true, false) => "staged",
                (false, true) => "unstaged",
                (false, false) => return None,
            })
        },
        ReviewSourceKind::LastCommit => of("last_commit"),
        ReviewSourceKind::Commit { rev } => SourceArgument {
            rev: Some(rev.clone()),
            ..of("commit")
        },
        ReviewSourceKind::CommitRange {
            base,
            head,
            merge_base,
        } => SourceArgument {
            base: Some(base.clone()),
            head: Some(head.clone()),
            merge_base: Some(*merge_base),
            ..of("range")
        },
        ReviewSourceKind::BranchCompare {
            base_branch,
            head_branch,
            merge_base,
        } => SourceArgument {
            base: Some(base_branch.clone()),
            head: Some(head_branch.clone()),
            merge_base: Some(*merge_base),
            ..of("branch")
        },
        ReviewSourceKind::File { path } => SourceArgument {
            paths: vec![PathSelector::File { path: path.clone() }.to_string()],
            ..of("files")
        },
        ReviewSourceKind::FileRange { path, start, end } => SourceArgument {
            paths: vec![
                PathSelector::FileRange {
                    path: path.clone(),
                    start: *start,
                    end: *end,
                }
                .to_string(),
            ],
            ..of("files")
        },
        ReviewSourceKind::FileSet { include, exclude } => SourceArgument {
            paths: include.iter().map(ToString::to_string).collect(),
            exclude: exclude.iter().map(ToString::to_string).collect(),
            ..of("files")
        },
        ReviewSourceKind::Repository => of("repository"),
        ReviewSourceKind::Commits { revs } => SourceArgument {
            revs: revs.clone(),
            ..of("commits")
        },
        ReviewSourceKind::PullRequest {
            provider: id,
            repository,
            number,
        } => SourceArgument {
            number: Some(*number),
            repository: Some(repository.clone()),
            provider: provider(id),
            ..of("pull_request")
        },
        ReviewSourceKind::StackLayer {
            provider: id,
            repository,
            number,
        } => SourceArgument {
            layers: vec![*number],
            repository: Some(repository.clone()),
            provider: provider(id),
            ..of("stack_layers")
        },
        ReviewSourceKind::Stack {
            provider: id,
            repository,
            top,
        } => SourceArgument {
            top: Some(*top),
            repository: Some(repository.clone()),
            provider: provider(id),
            ..of("stack")
        },
        ReviewSourceKind::CurrentBranch { base } => SourceArgument {
            base: base.clone(),
            ..of("current_branch")
        },
        ReviewSourceKind::Unsupported { .. } => return None,
    })
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
    use crate::testing::fixture::GitFixture;

    fn repository() -> GitFixture {
        let fixture = GitFixture::empty();
        for path in [
            "examples/hello-plugin/src/lib.rs",
            "examples/hello-plugin/README.md",
            "docs/guide.md",
        ] {
            let full = fixture.root().join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(full, "x\n").expect("write");
        }
        fixture
    }

    fn argument(value: serde_json::Value) -> SourceArgument {
        serde_json::from_value(value).expect("argument")
    }

    fn kinds_of(
        root: &Path,
        directory: Option<&str>,
        value: serde_json::Value,
    ) -> Result<Vec<ReviewSourceKind>, AssistantError> {
        plan(root, directory, vec![argument(value)])
            .map(|plan| plan.sources.into_iter().map(|(kind, _)| kind).collect())
    }

    fn folder(path: &str) -> PathSelector {
        PathSelector::Folder { path: path.into() }
    }

    #[test]
    fn files_and_repository_sources_take_the_selector_syntax() {
        let fixture = repository();
        let root = fixture.root();
        let kinds = |value| kinds_of(root, None, value).expect("plan");
        assert_eq!(
            kinds(json!({"kind": "files", "paths": ["examples/hello-plugin"]})),
            [ReviewSourceKind::FileSet {
                include: vec![folder("examples/hello-plugin")],
                exclude: Vec::new(),
            }]
        );
        assert_eq!(
            kinds(json!({"kind": "files", "paths": ["examples/hello-plugin/src/lib.rs:3-1"]})),
            [ReviewSourceKind::FileRange {
                path: "examples/hello-plugin/src/lib.rs".into(),
                start: 1,
                end: 3,
            }]
        );
        assert_eq!(
            kinds(json!({
                "kind": "files",
                "paths": ["examples/", "docs/**/*.md", "examples/hello-plugin/README.md"],
                "exclude": ["examples/hello-plugin/src"],
            })),
            [ReviewSourceKind::FileSet {
                include: vec![
                    folder("examples"),
                    PathSelector::Glob {
                        pattern: "docs/**/*.md".into()
                    },
                    PathSelector::File {
                        path: "examples/hello-plugin/README.md".into()
                    },
                ],
                exclude: vec![folder("examples/hello-plugin/src")],
            }]
        );
        assert_eq!(
            kinds(json!({"kind": "repository"})),
            [ReviewSourceKind::Repository]
        );
        assert_eq!(
            kinds(json!({"kind": "repository", "paths": ["examples", "docs/"]})),
            [ReviewSourceKind::FileSet {
                include: vec![folder("examples"), folder("docs")],
                exclude: Vec::new(),
            }]
        );
        // A directory below the root is the folder when no paths are named.
        for kind in ["files", "repository"] {
            assert_eq!(
                kinds_of(root, Some("examples/hello-plugin"), json!({"kind": kind})).expect("plan"),
                [ReviewSourceKind::FileSet {
                    include: vec![folder("examples/hello-plugin")],
                    exclude: Vec::new(),
                }]
            );
        }
    }

    #[test]
    fn files_and_repository_sources_refuse_what_names_nothing_or_leaves_the_repository() {
        let fixture = repository();
        let root = fixture.root();
        let error = |value| match kinds_of(root, None, value) {
            Err(AssistantError::Invalid(message)) => message,
            other => panic!("expected an invalid argument, got {other:?}"),
        };
        assert_eq!(
            error(json!({"kind": "files", "paths": ["examples/helo-plugin"]})),
            "examples/helo-plugin: no such file or folder"
        );
        assert!(error(json!({"kind": "files"})).contains("needs `paths`"));
        assert!(error(json!({"kind": "files", "paths": ["../outside"]})).contains("outside"));
        assert!(error(json!({"kind": "files", "paths": ["/etc/hosts"]})).contains("outside"));
        assert!(
            error(json!({"kind": "files", "paths": ["examples/"], "exclude": ["*.md"]}))
                .contains("only files and folders can be excluded")
        );
        assert!(
            error(json!({"kind": "repository", "paths": ["docs/guide.md"]}))
                .contains("limited to folders")
        );
    }

    #[test]
    fn every_source_kind_but_unsupported_ones_has_a_create_review_source() {
        let fixture = repository();
        let root = fixture.root();
        let pull = |number| ReviewSourceKind::PullRequest {
            provider: GITHUB_PROVIDER.into(),
            repository: "o/r".into(),
            number,
        };
        let kinds = vec![
            ReviewSourceKind::WorkingTreeUnstaged,
            ReviewSourceKind::IndexStaged,
            ReviewSourceKind::WorkingTreeAndIndex,
            ReviewSourceKind::WorkingTreePaths {
                paths: vec!["docs".into()],
                staged: true,
                unstaged: false,
            },
            ReviewSourceKind::LastCommit,
            ReviewSourceKind::Commit { rev: "abc".into() },
            ReviewSourceKind::CommitRange {
                base: "a".into(),
                head: "b".into(),
                merge_base: true,
            },
            ReviewSourceKind::BranchCompare {
                base_branch: "main".into(),
                head_branch: "topic".into(),
                merge_base: false,
            },
            ReviewSourceKind::File {
                path: "docs/guide.md".into(),
            },
            ReviewSourceKind::FileRange {
                path: "docs/guide.md".into(),
                start: 2,
                end: 4,
            },
            ReviewSourceKind::FileSet {
                include: vec![folder("examples"), folder("docs")],
                exclude: vec![PathSelector::File {
                    path: "examples/hello-plugin/README.md".into(),
                }],
            },
            ReviewSourceKind::Repository,
            ReviewSourceKind::Commits {
                revs: vec!["a".into(), "b".into()],
            },
            pull(7),
            ReviewSourceKind::StackLayer {
                provider: GITHUB_PROVIDER.into(),
                repository: "o/r".into(),
                number: 8,
            },
            ReviewSourceKind::Stack {
                provider: GITHUB_PROVIDER.into(),
                repository: "o/r".into(),
                top: 9,
            },
            ReviewSourceKind::CurrentBranch {
                base: Some("main".into()),
            },
            ReviewSourceKind::Stack {
                provider: "graphite".into(),
                repository: "o/r".into(),
                top: 3,
            },
        ];
        for kind in kinds {
            let argument = source_argument(&kind, Some("label"))
                .unwrap_or_else(|| panic!("{kind:?} has no create_review source"));
            let planned = plan(root, None, vec![argument]).expect("plan");
            assert_eq!(
                planned.sources,
                [(kind.clone(), Some("label".into()))],
                "{kind:?}"
            );
        }
        let unsupported: ReviewSourceKind =
            serde_json::from_str(r#"{"kind":"future_kind"}"#).expect("kind");
        assert!(source_argument(&unsupported, None).is_none());
    }

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
