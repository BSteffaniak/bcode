//! The app's action tools for the review assistant: what each is called, what it takes, and
//! the parts that need no screen (finding pull requests and stacks).
//!
//! The app answers them in [`crate::app::ReviewApp`]: moving within the review on screen and
//! changing its comparison or view are read-only (they change what is shown, not what is
//! stored) and run without asking; opening another screen, completing a pass, publishing and
//! pull request verdicts are mutating, so Bcode's permission rules ask first. Publishing and
//! verdicts only open the app's own dialog, which the reviewer confirms.

use bcode_code_review_models::PullRequestSummary;
use bcode_tool_models::client_tool::{ClientToolDescriptor, ClientToolPolicy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Finds the user's pull requests and stacks.
pub const FIND_PULL_REQUESTS: &str = "bcode.code-review.find_pull_requests";
/// Moves within the review on screen.
pub const GO_TO: &str = "bcode.code-review.go_to";
/// Changes the comparison of the review on screen.
pub const SET_COMPARISON: &str = "bcode.code-review.set_comparison";
/// Changes how the review on screen is shown.
pub const SET_VIEW: &str = "bcode.code-review.set_view";
/// Opens a review, home, or the new-review wizard.
pub const OPEN_REVIEW: &str = "bcode.code-review.open_review";
/// Reloads home or the review on screen.
pub const REFRESH: &str = "bcode.code-review.refresh";
/// Marks the review pass complete.
pub const COMPLETE_PASS: &str = "bcode.code-review.complete_pass";
/// Opens a pull request verdict dialog.
pub const PULL_REQUEST_REVIEW: &str = "bcode.code-review.pull_request_review";
/// Opens the publish dialog.
pub const PUBLISH: &str = "bcode.code-review.publish";
/// Shows the review's guide and steps through it.
pub const GUIDE_SHOW: &str = "bcode.code-review.guide_show";

/// Pull requests and stacks listed at most.
pub const MAX_FOUND: usize = 50;
/// How long a client tool call may run, in milliseconds.
const DEADLINE_MS: u32 = 60_000;

fn descriptor(
    name: &str,
    policy: ClientToolPolicy,
    description: &str,
    properties: &Value,
) -> ClientToolDescriptor {
    ClientToolDescriptor {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "additionalProperties": false
        }),
        policy,
        max_duration_ms: DEADLINE_MS,
    }
}

fn location_properties() -> serde_json::Map<String, Value> {
    let Value::Object(map) = json!({
        "layer": {"type": ["integer", "string"], "description": "A stack layer: its pull request number, or a source id."},
        "path": {"type": "string", "description": "A file path of the review."},
        "line": {"type": "integer", "minimum": 1, "description": "A line of the file."},
        "side": {"type": "string", "enum": ["new", "old"], "description": "Side the line is on (new by default)."},
        "source": {"type": "string", "description": "Source id, when several sources have the file."},
        "thread": {"type": "string", "description": "A thread id exactly as code_review.list_threads, current_view or the question gives it (a pull request thread's is `<repository>#<number>:<thread id>`). The thread is the place: it is reached wherever the review shows it (an outdated one near its code, on its file or on its pull request), entering its layer when needed; path, line and version are then not used."},
        "version": {"type": "integer", "minimum": 1, "description": "Show this version (v1 is the first) of the layer (`layer`, else the current one): its whole change as it was then, read-only where it differs from now."}
    }) else {
        return serde_json::Map::new();
    };
    map
}

/// The action tools' descriptors.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn descriptors() -> Vec<ClientToolDescriptor> {
    let mut go_to = location_properties();
    go_to.insert(
        "review".to_owned(),
        json!({"type": "string", "description": "The review the place is in; it must be the one on screen (else use open_review)."}),
    );
    let mut open = location_properties();
    open.insert(
        "review".to_owned(),
        json!({"type": "string", "description": "Id of the saved review to open (code_review.list_reviews)."}),
    );
    open.insert(
        "title".to_owned(),
        json!({"type": "string", "description": "new_review: the title to fill in."}),
    );
    open.insert(
        "sources".to_owned(),
        json!({
            "type": "array",
            "items": bcode_code_review::assistant::create::source_schema(),
            "description": "new_review: the sources to fill in (as code_review.create_review takes them); the user confirms or changes them in the wizard."
        }),
    );
    open.insert(
        "screen".to_owned(),
        json!({"type": "string", "enum": ["review", "home", "new_review"], "description": "review (default, needs `review`), home (the reviews list) or new_review (the wizard)."}),
    );
    vec![
        descriptor(
            FIND_PULL_REQUESTS,
            ClientToolPolicy::ReadOnly,
            "Find open pull requests and stacks in this repository with the app's forge provider, newest first. Each stack lists its layers bottom first and a ready `create_review_source` to pass to code_review.create_review (stack_layers steps through it one pull request at a time). filter: mine (stacks with a layer whose branch is a local branch of this repository: the user's own work), review_requested (review requested from the user), current_branch (the stack of the checked-out branch), all. Use it for \"my latest stack\" or \"pull request #N\".",
            &json!({
                "filter": {"type": "string", "enum": ["mine", "review_requested", "current_branch", "all"], "description": "Which pull requests (default all)."},
                "query": {"type": "string", "description": "Only pull requests whose number, title or branch contains this."},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_FOUND, "description": "Stacks to return at most (default 10)."}
            }),
        ),
        descriptor(
            GO_TO,
            ClientToolPolicy::ReadOnly,
            "Move the user's view within the review on screen to a stack layer, a file and line, a thread, or an earlier version of a layer, as the keys do. Fails when the place is in another review: use open_review then. Returns what is on screen now.",
            &Value::Object(go_to),
        ),
        descriptor(
            SET_COMPARISON,
            ClientToolPolicy::ReadOnly,
            "Change the comparison of the review on screen, as the Compare menu (b) does: what changes are shown since (baseline: off = the full change, last_looked, previous_push, ago with ago_minutes, between with from_ms/to_ms, pushes with from_push/to_push = the stack just before push from_push compared with the stack after push to_push (pushes are numbered from 1, oldest first, as current_view lists them), stack_at with at_ms = every layer as it was then), the view (full = whole change with what is new marked, changes = only what changed), only pull requests with real changes, showing the files the comparison hides, one pull request, all of them, or the whole stack as one diff, versions vN..vM of one pull request, and how new lines are marked. Returns the comparison as it is now (it may still be computing).",
            &json!({
                "baseline": {"type": "string", "enum": ["off", "last_looked", "previous_push", "ago", "between", "pushes", "stack_at"]},
                "ago_minutes": {"type": "integer", "minimum": 1},
                "from_ms": {"type": "integer", "minimum": 0},
                "to_ms": {"type": "integer", "minimum": 0},
                "from_push": {"type": "integer", "minimum": 1, "description": "For pushes: the first push compared."},
                "to_push": {"type": "integer", "minimum": 1, "description": "For pushes: the last push compared (the latest when omitted)."},
                "at_ms": {"type": "integer", "minimum": 0, "description": "For stack_at: the time to show the stack at."},
                "view": {"type": "string", "enum": ["full", "changes"]},
                "only_changed": {"type": "boolean"},
                "show_hidden_files": {"type": "boolean"},
                "all_layers": {"type": "boolean", "description": "Every pull request of the stack at once (false: one at a time)."},
                "stack_view": {"type": "string", "enum": ["one_pr", "all_prs", "whole"], "description": "How a stack shows: one pull request at a time, every pull request, or the whole stack as one diff."},
                "versions": {
                    "type": "object",
                    "properties": {
                        "number": {"type": "integer", "minimum": 1},
                        "from": {"type": "integer", "minimum": 1, "description": "From version (v1 is the first)."},
                        "to": {"type": "integer", "minimum": 1, "description": "To version (the latest when omitted)."}
                    },
                    "required": ["number", "from"],
                    "additionalProperties": false
                },
                "mark_style": {"type": "string", "enum": ["both", "bar", "tint", "strong"]},
                "word_marks": {"type": "boolean"},
                "seen_hunks": {"type": "string", "enum": ["show", "dim", "fold"]},
                "removed_inline": {"type": "boolean"}
            }),
        ),
        descriptor(
            SET_VIEW,
            ClientToolPolicy::ReadOnly,
            "Change how the review on screen is shown: layout, line wrapping, hiding resolved threads or viewed files, the file list as a tree or flat, the file and threads panels, and folding every file or thread.",
            &json!({
                "layout": {"type": "string", "enum": ["unified", "side_by_side"]},
                "wrap": {"type": "boolean"},
                "hide_resolved_threads": {"type": "boolean"},
                "hide_viewed_files": {"type": "boolean"},
                "file_list": {"type": "string", "enum": ["tree", "flat"]},
                "files_panel": {"type": "boolean"},
                "threads_panel": {"type": "boolean"},
                "fold_files": {"type": "boolean"},
                "fold_threads": {"type": "boolean"}
            }),
        ),
        descriptor(
            OPEN_REVIEW,
            ClientToolPolicy::Mutating,
            "Take the user to another screen: open a saved review of this repository (optionally at a layer, file, line or thread), the reviews home, or the new-review wizard (filled in with `title` and `sources` for the user to confirm). The user may be asked first. Returns what is on screen once it opened.",
            &Value::Object(open),
        ),
        descriptor(
            REFRESH,
            ClientToolPolicy::ReadOnly,
            "Reload the reviews home, or check the review on screen for new changes (as r does).",
            &json!({}),
        ),
        descriptor(
            COMPLETE_PASS,
            ClientToolPolicy::Mutating,
            "Mark the review pass of the review on screen complete (as shift+m does), so later comparisons start from what is shown now.",
            &json!({}),
        ),
        descriptor(
            PULL_REQUEST_REVIEW,
            ClientToolPolicy::Mutating,
            "Open the app's dialog to approve, request changes on, or comment on the pull request of the review on screen, or to approve every fully viewed stack layer. The user confirms in the dialog; nothing is sent until they do.",
            &json!({
                "action": {"type": "string", "enum": ["approve", "request_changes", "comment", "approve_viewed_layers"]}
            }),
        ),
        descriptor(
            PUBLISH,
            ClientToolPolicy::Mutating,
            "Open the app's publish dialog for the review on screen (as shift+p does), to publish its draft comments with a publisher such as GitHub. The user confirms in the dialog; nothing is sent until they do.",
            &json!({}),
        ),
        descriptor(
            GUIDE_SHOW,
            ClientToolPolicy::ReadOnly,
            "Show the review's guide (written with code_review.guide.start and code_review.guide.write) and move the user through it exactly as the guide keys do: open the guide panel (or the assistant's Guide tab when the screen has no room for it), select a section, show one of its citations in the diff, or step to the next, previous or next unreviewed section. Without arguments it opens the guide where the user left it. Returns what is on screen now, with the guide's current section.",
            &json!({
                "guide_id": {"type": "string", "description": "The guide to show (default: the one on screen, else the review's newest)."},
                "version": {"type": "integer", "minimum": 1, "description": "Must be the version on screen (the guide's current one)."},
                "section": {"type": "string", "description": "Section id (s3, s3.1) or outline number (3.1) to show, with its first citation."},
                "citation": {"type": "string", "description": "Citation id (c7) to show in the diff, in the section citing it."},
                "step": {"type": "string", "enum": ["next", "previous", "next_unreviewed"], "description": "Step from the current place, as alt+down, alt+up and alt+u do."},
                "focus": {"type": "boolean", "description": "Give the guide panel the keyboard (default false)."}
            }),
        ),
    ]
}

/// What `guide_show` takes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GuideShowArguments {
    /// The guide to show.
    pub guide_id: Option<String>,
    /// The version, which must be the one on screen.
    pub version: Option<u32>,
    /// Section id or outline number.
    pub section: Option<String>,
    /// Citation id.
    pub citation: Option<String>,
    /// `next`, `previous` or `next_unreviewed`.
    pub step: Option<String>,
    /// Whether the guide panel takes the keyboard.
    pub focus: bool,
}

/// What `find_pull_requests` takes.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FindArguments {
    /// `mine`, `review_requested`, `current_branch` or `all`.
    pub filter: Option<String>,
    /// Only pull requests whose number, title or branch contains this.
    pub query: Option<String>,
    /// Stacks returned at most.
    pub limit: Option<usize>,
}

/// One layer of a found stack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FoundLayer {
    /// Pull request number.
    pub number: u64,
    /// Title.
    pub title: String,
    /// Branch it comes from.
    pub head_branch: String,
    /// Branch it goes into.
    pub base_branch: String,
    /// Author login.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Last update (RFC 3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
    /// Whether it is a draft.
    pub draft: bool,
    /// Whether review is requested from the user.
    pub review_requested_from_me: bool,
    /// Whether its branch is a local branch of the repository.
    pub local_branch: bool,
}

/// A found stack (a lone pull request is a stack of one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FoundStack {
    /// Layers, bottom first.
    pub layers: Vec<FoundLayer>,
    /// The latest update of any layer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
    /// Whether it contains the checked-out branch.
    pub current_branch: bool,
    /// The source to pass to `code_review.create_review`.
    pub create_review_source: Value,
}

/// What `find_pull_requests` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Found {
    /// Forge repository.
    pub repository: String,
    /// Stacks, newest first.
    pub stacks: Vec<FoundStack>,
    /// Matching stacks left out by the limit.
    pub omitted: usize,
}

/// The stacks of `pulls`: each chain of pull requests whose base is another's head, from a
/// bottom (based on a branch no listed pull request has as head) up to each top, filtered and
/// ordered newest first.
#[must_use]
pub fn stacks(
    repository: &str,
    pulls: &[PullRequestSummary],
    local: &BTreeSet<String>,
    current: Option<&str>,
    arguments: &FindArguments,
) -> Found {
    let by_head: BTreeMap<&str, &PullRequestSummary> = pulls
        .iter()
        .map(|pull| (pull.head_branch.as_str(), pull))
        .collect();
    let has_child: BTreeSet<&str> = pulls
        .iter()
        .filter(|pull| by_head.contains_key(pull.base_branch.as_str()))
        .map(|pull| pull.base_branch.as_str())
        .collect();
    let mut found: Vec<FoundStack> = pulls
        .iter()
        .filter(|pull| !has_child.contains(pull.head_branch.as_str()))
        .map(|top| {
            let mut chain = vec![top];
            let mut seen = BTreeSet::from([top.number]);
            while let Some(below) = by_head.get(chain[chain.len() - 1].base_branch.as_str()) {
                if !seen.insert(below.number) {
                    break;
                }
                chain.push(below);
            }
            chain.reverse();
            let layers: Vec<FoundLayer> = chain
                .iter()
                .map(|pull| FoundLayer {
                    number: pull.number,
                    title: pull.title.clone(),
                    head_branch: pull.head_branch.clone(),
                    base_branch: pull.base_branch.clone(),
                    author: pull.author.clone(),
                    updated: pull.updated.clone(),
                    draft: pull.draft,
                    review_requested_from_me: pull.review_requested_from_me,
                    local_branch: local.contains(&pull.head_branch),
                })
                .collect();
            let updated = layers.iter().filter_map(|layer| layer.updated.clone()).max();
            let source = if layers.len() == 1 {
                json!({"kind": "pull_request", "number": layers[0].number, "repository": repository, "label": format!("#{} {}", layers[0].number, layers[0].title)})
            } else {
                json!({
                    "kind": "stack_layers",
                    "repository": repository,
                    "layers": layers.iter().map(|layer| layer.number).collect::<Vec<_>>(),
                    "titles": layers.iter().map(|layer| layer.title.clone()).collect::<Vec<_>>(),
                })
            };
            FoundStack {
                current_branch: current
                    .is_some_and(|branch| layers.iter().any(|layer| layer.head_branch == branch)),
                layers,
                updated,
                create_review_source: source,
            }
        })
        .collect();
    let query = arguments
        .query
        .as_deref()
        .map(|query| query.trim().trim_start_matches('#').to_lowercase())
        .filter(|query| !query.is_empty());
    found.retain(|stack| {
        let filter = match arguments.filter.as_deref().unwrap_or("all") {
            "mine" => stack.layers.iter().any(|layer| layer.local_branch),
            "review_requested" => stack
                .layers
                .iter()
                .any(|layer| layer.review_requested_from_me),
            "current_branch" => stack.current_branch,
            _ => true,
        };
        filter
            && query.as_ref().is_none_or(|query| {
                stack.layers.iter().any(|layer| {
                    layer.number.to_string() == *query
                        || layer.title.to_lowercase().contains(query)
                        || layer.head_branch.to_lowercase().contains(query)
                })
            })
    });
    found.sort_by(|a, b| {
        b.updated
            .cmp(&a.updated)
            .then_with(|| b.layers.len().cmp(&a.layers.len()))
    });
    let limit = arguments.limit.unwrap_or(10).clamp(1, MAX_FOUND);
    let omitted = found.len().saturating_sub(limit);
    found.truncate(limit);
    Found {
        repository: repository.to_owned(),
        stacks: found,
        omitted,
    }
}

/// Find pull requests and stacks for `arguments` in the repository at `repo`.
///
/// # Errors
///
/// * No provider lists pull requests, the provider failed, or the arguments are invalid.
pub async fn find(
    services: crate::services::ReviewServices,
    repo: std::path::PathBuf,
    arguments: &str,
) -> Result<String, String> {
    let arguments: FindArguments = if arguments.trim().is_empty() {
        FindArguments::default()
    } else {
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?
    };
    let answer = crate::home::load::pulls(services, repo.clone())
        .await?
        .ok_or("no forge provider lists pull requests for this repository")?;
    // Git runs off the async workers.
    let (local, current) = tokio::task::spawn_blocking(move || {
        let local: BTreeSet<String> = git(
            &repo,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
        )
        .lines()
        .map(str::to_owned)
        .collect();
        (local, git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]))
    })
    .await
    .map_err(|error| format!("reading the local branches failed: {error}"))?;
    let found = stacks(
        &answer.repository,
        &answer.pulls,
        &local,
        Some(current.trim()).filter(|branch| !branch.is_empty() && *branch != "HEAD"),
        &arguments,
    );
    serde_json::to_string_pretty(&found).map_err(|error| error.to_string())
}

fn git(repo: &std::path::Path, args: &[&str]) -> String {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pull(number: u64, head: &str, base: &str, updated: &str) -> PullRequestSummary {
        PullRequestSummary {
            number,
            title: format!("Title {number}"),
            author: Some("ana".to_owned()),
            base_branch: base.to_owned(),
            head_branch: head.to_owned(),
            head_oid: "a".repeat(40),
            base_oid: None,
            review_requested_from_me: number == 9,
            draft: false,
            updated: Some(updated.to_owned()),
            url: None,
        }
    }

    #[test]
    fn stacks_chain_from_the_bottom_newest_first_and_filter() {
        let pulls = vec![
            pull(3, "feat-3", "feat-2", "2026-11-03"),
            pull(1, "feat-1", "main", "2026-11-01"),
            pull(2, "feat-2", "feat-1", "2026-11-02"),
            pull(9, "theirs", "main", "2026-11-05"),
        ];
        let local = BTreeSet::from(["feat-1".to_owned(), "feat-2".to_owned()]);
        let all = stacks(
            "acme/w",
            &pulls,
            &local,
            Some("feat-2"),
            &FindArguments::default(),
        );
        assert_eq!(all.stacks.len(), 2);
        assert_eq!(all.stacks[0].layers[0].number, 9, "newest first");
        let mine = stacks(
            "acme/w",
            &pulls,
            &local,
            Some("feat-2"),
            &FindArguments {
                filter: Some("mine".to_owned()),
                ..FindArguments::default()
            },
        );
        assert_eq!(mine.stacks.len(), 1);
        let numbers: Vec<u64> = mine.stacks[0]
            .layers
            .iter()
            .map(|layer| layer.number)
            .collect();
        assert_eq!(numbers, vec![1, 2, 3], "bottom first");
        assert!(mine.stacks[0].current_branch);
        assert_eq!(mine.stacks[0].create_review_source["kind"], "stack_layers");
        assert_eq!(
            mine.stacks[0].create_review_source["layers"],
            json!([1, 2, 3])
        );
        let requested = stacks(
            "acme/w",
            &pulls,
            &local,
            None,
            &FindArguments {
                filter: Some("review_requested".to_owned()),
                ..FindArguments::default()
            },
        );
        assert_eq!(
            requested.stacks[0].create_review_source["kind"],
            "pull_request"
        );
        let by_number = stacks(
            "acme/w",
            &pulls,
            &local,
            None,
            &FindArguments {
                query: Some("#2".to_owned()),
                ..FindArguments::default()
            },
        );
        assert_eq!(by_number.stacks.len(), 1);
    }
}
