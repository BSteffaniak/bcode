//! Review tools: the code review exposed to agents as `bcode.tool/v1` tools.
//!
//! Any Bcode session can call them. A session the review app started for its assistant is bound
//! to that review (see [`bcode_code_review::assistant`]), so its tools act on the review on
//! screen; any session can instead name a saved review with `review` (from
//! `code_review.list_reviews`).
//!
//! Each tool declares its policy operation, so Bcode's permission rules govern it like any other
//! tool:
//!
//! * Reads (`summary`, `list_files`, `file_diff`, `file_content`, `list_threads`, `get_thread`,
//!   `list_reviews`) and `navigate` are read-only: allowed unless a rule says otherwise.
//!   `list_threads` and `get_thread` read pull request threads from the snapshot the review app
//!   keeps in the review store, so they never reach the forge.
//!   `file_diff` and `file_content` also declare the file they read, so read path rules (such
//!   as a denied `**/.env`) apply to them.
//! * `pull_request` asks GitHub with the user's credentials, so it also declares the
//!   repository's web address and web rules decide it; without a rule it asks.
//! * Writes (`add_comment`, `reply`, `resolve_thread`, `mark_viewed`, `suggest_change`) are
//!   mutating and ask by default. `reply` and `resolve_thread` change local threads only and
//!   refuse a pull request thread.
//! * `publish` sends the review out of the repository and asks by default. An `output_path`
//!   must be relative to the session's working directory and stay inside it; the publish also
//!   declares that file as a write, so write path rules apply and the prompt names it.
//!
//! The session's id and working directory come from the host's workspace context during
//! preparation and travel to the invocation in the preparation descriptor.

use bcode_code_review::assistant::{
    AssistantError, Location, PullThreadQuery, ReviewRef, ReviewTools, Side,
};
use bcode_code_review::viewed::ViewedReset;
use bcode_code_review::{CodeReviewConfig, ReviewData, ReviewStore};
use bcode_code_review_models::{
    ReviewPathMatchKind, ReviewPathsFindRequest, ReviewSourceKind, ReviewThreadKind,
    ReviewThreadSeverity,
};
use bcode_plugin_sdk::prelude::*;
use bcode_plugin_sdk::{ToolPolicyIdentity, ToolPolicyOperation, ToolPolicyPreparation};
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, OP_PREPARE_TOOL, ToolDefinition,
    ToolDiscoveryPolicy, ToolInvocationRequest, ToolInvocationResponse, ToolList,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;

/// Prefix of every review tool name.
pub const TOOL_PREFIX: &str = "code_review.";
/// Permission category of the read tools.
pub const CATEGORY_READ: &str = "code_review.read";
/// Permission category of the tools that change the review.
pub const CATEGORY_WRITE: &str = "code_review.write";
/// Permission category of the tools that send the review out.
pub const CATEGORY_PUBLISH: &str = "code_review.publish";
/// Bytes of tool output returned at most.
const MAX_OUTPUT_BYTES: usize = 128 * 1024;
/// Items one batch tool call takes at most.
const MAX_BATCH: usize = 100;

/// What a tool does to the review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Reads it (or moves the reviewer's view).
    Read,
    /// Changes its stored state.
    Write,
    /// Sends it out of the repository.
    Publish,
}

/// One review tool.
struct Spec {
    name: &'static str,
    access: Access,
    description: &'static str,
    schema: fn() -> Value,
}

/// The `review` argument every tool that acts on one review takes.
fn review_property() -> Value {
    json!({
        "type": "string",
        "description": "Id of a saved review (from code_review.list_reviews). Omit it to use the review this session is attached to."
    })
}

fn location_properties() -> serde_json::Map<String, Value> {
    let Value::Object(map) = json!({
        "path": {"type": "string", "description": "File path as listed by code_review.list_files. Omit for the whole review."},
        "line": {"type": "integer", "minimum": 1, "description": "First line; omit for the whole file."},
        "end_line": {"type": "integer", "minimum": 1, "description": "Last line of a range."},
        "side": {"type": "string", "enum": ["new", "old"], "description": "Side the line numbers are on: new (added and unchanged lines, the default) or old (removed lines)."},
        "source": {"type": "string", "description": "Source id, when the file is in several sources."},
        "review": review_property(),
    }) else {
        return serde_json::Map::new();
    };
    map
}

/// A `review` argument naming a saved review to act on.
fn review_property_required() -> Value {
    json!({
        "type": "string",
        "description": "Id of a saved review (from code_review.list_reviews)."
    })
}

fn json_map(value: Value) -> serde_json::Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    }
}

fn object(properties: serde_json::Map<String, Value>, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": required,
        "additionalProperties": false,
    })
}

fn with_review(properties: Value, required: &[&str]) -> Value {
    let mut map = match properties {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    map.insert("review".to_owned(), review_property());
    object(map, required)
}

const SPECS: &[Spec] = &[
    Spec {
        name: "code_review.list_reviews",
        access: Access::Read,
        description: "List the saved code reviews of this repository, most recently updated first: id, title, kind (stack_step_through, whole_stack, pull_request, branch, current_branch, range, commits, working_tree, ...), source labels, pull request numbers, viewed files, archived, created and updated times. Pass `query` to find one by title, label or pull request number.",
        schema: || {
            object(
                json_map(json!({
                    "include_archived": {"type": "boolean", "description": "Include archived reviews (default false)."},
                    "query": {"type": "string", "description": "Only reviews whose title, kind, source labels or pull request numbers contain this (case-insensitive)."},
                })),
                &[],
            )
        },
    },
    Spec {
        name: "code_review.summary",
        access: Access::Read,
        description: "Summarize the code review: title, sources and the revisions they resolve to, stack layers, changed files and lines, viewed files, threads and suggestions.",
        schema: || with_review(json!({}), &[]),
    },
    Spec {
        name: "code_review.list_files",
        access: Access::Read,
        description: "List the review's changed files with status, added and removed lines, and whether the reviewer marked each viewed. `truncated` says which folder, glob or repository sources were cut to their file limit (files.max_files) and how many files they select in all.",
        schema: || with_review(json!({}), &[]),
    },
    Spec {
        name: "code_review.file_diff",
        access: Access::Read,
        description: "Show one file's diff in the review as numbered lines (old line, new line, +/- marker, text) with hunk headers.",
        schema: || {
            with_review(
                json!({
                    "path": {"type": "string", "description": "File path."},
                    "source": {"type": "string", "description": "Source id, when the file is in several sources."},
                    "context": {"type": "integer", "minimum": 0, "maximum": 20, "description": "Context lines around changes (default 3)."},
                    "max_lines": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "Diff lines to return at most (default 600)."},
                }),
                &["path"],
            )
        },
    },
    Spec {
        name: "code_review.file_content",
        access: Access::Read,
        description: "Read a file of the review at its head (new side) or base (old side) revision, as numbered lines.",
        schema: || {
            with_review(
                json!({
                    "path": {"type": "string", "description": "File path."},
                    "side": {"type": "string", "enum": ["new", "old"], "description": "new (the reviewed revision, default) or old (the base)."},
                    "start_line": {"type": "integer", "minimum": 1, "description": "First line (default 1)."},
                    "count": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "Lines to return (default 400)."},
                    "source": {"type": "string", "description": "Source id, when the file is in several sources."},
                }),
                &["path"],
            )
        },
    },
    Spec {
        name: "code_review.list_threads",
        access: Access::Read,
        description: "List the review's comment threads: its own local threads (location, kind, severity, resolved, comments), pending suggested changes, and a page of its pull requests' threads on the forge (origin pull_request: author, comments, resolved, outdated, where each was written and where the review shows it now), as the review app last read them. Pass a thread_id to code_review.get_thread to read one in full.",
        schema: || {
            with_review(
                json!({
                    "include_resolved": {"type": "boolean", "description": "Include resolved threads (default false)."},
                    "path": {"type": "string", "description": "Only threads on this file."},
                    "offset": {"type": "integer", "minimum": 0, "description": "First pull request thread of the page (next_offset of the previous page)."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Pull request threads per page (default 50)."},
                }),
                &[],
            )
        },
    },
    Spec {
        name: "code_review.get_thread",
        access: Access::Read,
        description: "Read one thread in full: a local thread, or a pull request thread with every comment and reply in order, its authors, resolved and outdated state, where it was written (path, original lines, commit), where the review shows it now, and the diff hunk it was written on.",
        schema: || {
            with_review(
                json!({"thread_id": {"type": "string", "description": "Thread id from code_review.list_threads (or a prompt)."}}),
                &["thread_id"],
            )
        },
    },
    Spec {
        name: "code_review.pull_request",
        access: Access::Read,
        description: "Show the pull request behind the review (title, state, branches, review decision, CI checks), found by the review's pull request sources or its branch. Uses the GitHub CLI.",
        schema: || with_review(json!({}), &[]),
    },
    Spec {
        name: "code_review.navigate",
        access: Access::Read,
        description: "Move the reviewer's view in the review app to a file or line, to show them what you are talking about.",
        schema: || object(location_properties(), &[]),
    },
    Spec {
        name: "code_review.add_comment",
        access: Access::Write,
        description: "Draft a review comment on lines, a file, or the whole review. Drafts stay local until the reviewer publishes.",
        schema: || {
            let mut properties = location_properties();
            properties.insert(
                "body".to_owned(),
                json!({"type": "string", "description": "Comment text (markdown)."}),
            );
            properties.insert("kind".to_owned(), json!({"type": "string", "enum": ["note", "question", "todo", "finding", "summary"], "description": "Thread kind."}));
            properties.insert("severity".to_owned(), json!({"type": "string", "enum": ["info", "nit", "warning", "blocker"], "description": "Thread severity."}));
            object(properties, &["body"])
        },
    },
    Spec {
        name: "code_review.reply",
        access: Access::Write,
        description: "Reply in one of the review's local threads with a draft comment. Pull request threads cannot be replied to with this tool.",
        schema: || {
            with_review(
                json!({
                    "thread_id": {"type": "string", "description": "Thread id from code_review.list_threads."},
                    "body": {"type": "string", "description": "Reply text (markdown)."},
                }),
                &["thread_id", "body"],
            )
        },
    },
    Spec {
        name: "code_review.resolve_thread",
        access: Access::Write,
        description: "Resolve one of the review's local threads, or reopen it. Pull request threads cannot be resolved with this tool.",
        schema: || {
            with_review(
                json!({
                    "thread_id": {"type": "string", "description": "Thread id from code_review.list_threads."},
                    "resolved": {"type": "boolean", "description": "true resolves (default), false reopens."},
                }),
                &["thread_id"],
            )
        },
    },
    Spec {
        name: "code_review.mark_viewed",
        access: Access::Write,
        description: "Mark many files viewed (or not viewed) at their current content in one call, as the reviewer's `v` does. Select files with any of: `paths` (exact paths), `glob` (e.g. src/**/*.rs), `layers` (stack layers by pull request number, or source ids), `all` (every file of the selected layers, or of the review), `unchanged_since_look` (files whose change is the same as when the reviewer last looked at their layer). Selectors narrow each other. Returns the files changed, those already in that state, and named paths not in the review.",
        schema: || {
            with_review(
                json!({
                    "paths": {"type": "array", "items": {"type": "string"}, "description": "File paths as code_review.list_files lists them."},
                    "glob": {"type": "string", "description": "Glob over file paths; `*` stays within a directory, `**` crosses directories."},
                    "layers": {"type": "array", "items": {"type": "string"}, "description": "Stack layers (pull request numbers such as \"102\") or source ids to take files from."},
                    "all": {"type": "boolean", "description": "Every file of the selected layers (or of the whole review)."},
                    "unchanged_since_look": {"type": "boolean", "description": "Only files unchanged since the reviewer last looked at their layer (or completed a review pass)."},
                    "viewed": {"type": "boolean", "description": "true marks viewed (default), false unmarks."},
                }),
                &[],
            )
        },
    },
    Spec {
        name: "code_review.add_comments",
        access: Access::Write,
        description: "Draft several review comments in one call, each like code_review.add_comment. Drafts stay local until the reviewer publishes. Returns each comment's thread id or why it failed.",
        schema: || {
            let mut comment = location_properties();
            comment.remove("review");
            comment.insert(
                "body".to_owned(),
                json!({"type": "string", "description": "Comment text (markdown)."}),
            );
            comment.insert("kind".to_owned(), json!({"type": "string", "enum": ["note", "question", "todo", "finding", "summary"]}));
            comment.insert(
                "severity".to_owned(),
                json!({"type": "string", "enum": ["info", "nit", "warning", "blocker"]}),
            );
            with_review(
                json!({
                    "comments": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_BATCH,
                        "items": object(comment, &["body"]),
                        "description": "The comments."
                    }
                }),
                &["comments"],
            )
        },
    },
    Spec {
        name: "code_review.resolve_threads",
        access: Access::Write,
        description: "Resolve (or reopen) several of the review's local threads in one call. Pull request threads cannot be resolved with this tool. Returns each thread's result.",
        schema: || {
            with_review(
                json!({
                    "thread_ids": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": MAX_BATCH, "description": "Thread ids from code_review.list_threads."},
                    "resolved": {"type": "boolean", "description": "true resolves (default), false reopens."},
                }),
                &["thread_ids"],
            )
        },
    },
    Spec {
        name: "code_review.create_review",
        access: Access::Write,
        description: "Create a saved review and return its id, title, kind and sources. Kinds: a pull request (number or url), a stack stepped through one pull request at a time (stack_layers, bottom first) or as one diff (stack), a branch against a base, the current branch, a commit range, commits, one commit, uncommitted changes (working_tree, staged, unstaged) optionally limited to `paths` or to `directory`, code as it is now (files: files, folders, globs or line ranges; a folder is listed again whenever the review opens), and the whole repository or some of its folders as it is now (repository). With `directory`, the review is created in that directory's repository, and uncommitted changes, files and repository reviews without `paths` are limited to it. To find a folder, package or file the user names, call code_review.find_paths first; to find the user's pull requests and stacks, call bcode.code-review.find_pull_requests first; to show the new review, call bcode.code-review.open_review. Every kind of review the app's wizard creates can be created here; never run the bcode command line instead.",
        schema: || {
            object(
                json_map(json!({
                    "sources": {"type": "array", "minItems": 1, "items": create::source_schema(), "description": "What to review, in order."},
                    "title": {"type": "string", "description": "Title (defaults from the sources)."},
                    "directory": {"type": "string", "description": "A directory (absolute, or relative to the session's working directory): the review belongs to its repository, and uncommitted changes, files and repository sources without `paths` are limited to it."},
                })),
                &["sources"],
            )
        },
    },
    Spec {
        name: "code_review.find_paths",
        access: Access::Read,
        description: "Find repository paths by what the user calls them: a folder name, a package or crate name (Cargo.toml, package.json, pyproject.toml), a file name, or a path ending (\"the hello plugin\", \"bcode-git\", \"Cargo.toml\", \"plugin/src\"). Files git ignores are left out unless `include_ignored`. Returns matches best first with their kind, files under each folder now, and how they matched; folders and packages end with `/` and can be passed as they are in the `paths` of a code_review.create_review files or repository source. `ambiguous` is true when the two best matches are equally likely: ask the user which one only then, or when nothing matched. Read-only.",
        schema: || {
            object(
                json_map(json!({
                    "query": {"type": "string", "description": "What the user called it: \"the hello plugin\", \"bcode-git\", \"the review wizard\", \"Cargo.toml\"."},
                    "kinds": {"type": "array", "items": {"type": "string", "enum": ["folder", "package", "file"]}, "description": "What to look for (default all)."},
                    "directory": {"type": "string", "description": "Search the repository of this directory (default the session's)."},
                    "include_ignored": {"type": "boolean", "description": "Also search files git ignores (default false)."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "Matches returned (default 10)."},
                })),
                &["query"],
            )
        },
    },
    Spec {
        name: "code_review.update_review",
        access: Access::Write,
        description: "Rename, archive or restore saved reviews: `review` for one or `reviews` for many.",
        schema: || {
            object(
                json_map(json!({
                    "review": review_property_required(),
                    "reviews": {"type": "array", "items": {"type": "string"}, "maxItems": MAX_BATCH, "description": "Review ids, to archive or restore several."},
                    "title": {"type": "string", "description": "New title (one review only)."},
                    "archived": {"type": "boolean", "description": "true archives, false restores."},
                })),
                &[],
            )
        },
    },
    Spec {
        name: "code_review.delete_review",
        access: Access::Write,
        description: "Delete saved reviews for good (their comments are no longer reachable). Prefer archiving (code_review.update_review) unless the user asked to delete. `review` for one or `reviews` for many.",
        schema: || {
            object(
                json_map(json!({
                    "review": review_property_required(),
                    "reviews": {"type": "array", "items": {"type": "string"}, "maxItems": MAX_BATCH, "description": "Review ids."},
                })),
                &[],
            )
        },
    },
    Spec {
        name: "code_review.suggest_change",
        access: Access::Write,
        description: "Suggest a review comment the reviewer can accept as a draft, optionally replacing the lines with new code (shown as a suggestion block).",
        schema: || {
            let mut properties = location_properties();
            properties.insert(
                "body".to_owned(),
                json!({"type": "string", "description": "Comment text (markdown)."}),
            );
            properties.insert(
                "replacement".to_owned(),
                json!({"type": "string", "description": "New text for the lines line..end_line."}),
            );
            properties.insert(
                "rationale".to_owned(),
                json!({"type": "string", "description": "Why, in one sentence."}),
            );
            object(properties, &["path"])
        },
    },
    Spec {
        name: "code_review.publish",
        access: Access::Publish,
        description: "Preview or publish the review's comments with a built-in publisher: local_markdown (default), markdown_file, or json_file. Publishing to a forge such as GitHub is done from the review app.",
        schema: || {
            with_review(
                json!({
                    "publisher": {"type": "string", "description": "Publisher id (default local_markdown)."},
                    "submit": {"type": "boolean", "description": "true publishes; false (default) previews."},
                    "output_path": {"type": "string", "description": "Output file for the file publishers."},
                }),
                &[],
            )
        },
    },
];

/// Definitions of every review tool.
#[must_use]
pub fn definitions() -> Vec<ToolDefinition> {
    SPECS
        .iter()
        .map(|spec| ToolDefinition {
            name: spec.name.to_owned(),
            description: spec.description.to_owned(),
            input_schema: (spec.schema)(),
        })
        .collect()
}

/// What `name` does to the review, when it is a review tool.
#[must_use]
pub fn access(name: &str) -> Option<Access> {
    SPECS
        .iter()
        .find(|spec| spec.name == name)
        .map(|spec| spec.access)
}

/// Serve a `bcode.tool/v1` request.
pub fn invoke_service(data: &mut ReviewData, context: &NativeServiceContext) -> ServiceResponse {
    let request = &context.request;
    match request.operation.as_str() {
        OP_LIST_TOOLS => {
            if let Err(error) = request.payload_json::<ListToolsRequest>() {
                return ServiceResponse::error("invalid_request", error.to_string());
            }
            json_response(&ToolList::with_runtime_stable_discovery(
                definitions(),
                |tool| ToolDiscoveryPolicy::new(access(&tool.name) == Some(Access::Read)),
            ))
        }
        OP_PREPARE_TOOL => prepare_tool_service_response(request, definitions(), prepare),
        OP_INVOKE_TOOL => match request.payload_json::<ToolInvocationRequest>() {
            Ok(invocation) => {
                let config = context.config_or_default::<CodeReviewConfig>();
                let settings = context.config_or_default::<ToolSettings>();
                let response = match (config, settings) {
                    (Ok(config), Ok(settings)) => invoke(
                        data,
                        &ReviewStore::new(config),
                        settings.viewed_reset,
                        &invocation,
                    ),
                    (Err(error), _) | (_, Err(error)) => failure(&format!(
                        "the code review configuration is invalid: {error}"
                    )),
                };
                json_response(&response)
            }
            Err(error) => ServiceResponse::error("invalid_request", error.to_string()),
        },
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported code review tool operation",
        ),
    }
}

/// Review settings the tools read from the plugin configuration.
#[derive(Debug, Default, Deserialize)]
struct ToolSettings {
    #[serde(default)]
    viewed_reset: ViewedReset,
}

/// Where an invocation runs: carried from preparation to invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Descriptor {
    /// The calling session, when the host named it.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The session's working directory (the repository).
    pub working_directory: PathBuf,
    /// Where `publish` writes, resolved inside the working directory during preparation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceContext {
    working_directory: PathBuf,
    #[serde(default)]
    workspace_id: Option<String>,
}

/// The policy of one invocation: read-only reads, mutating writes and publishing.
fn prepare(
    request: &bcode_tool::ToolPreparationRequest,
    definition: &ToolDefinition,
) -> Result<ToolPolicyPreparation, String> {
    let access = access(&definition.name)
        .ok_or_else(|| format!("unknown review tool {}", definition.name))?;
    let workspace = request
        .host_context
        .iter()
        .find(|entry| {
            entry.schema == bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA
                && entry.schema_version == bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION
        })
        .ok_or_else(|| "review tools need the session's workspace".to_owned())
        .and_then(|entry| {
            serde_json::from_value::<WorkspaceContext>(entry.payload.clone())
                .map_err(|error| format!("invalid workspace context: {error}"))
        })?;
    if !workspace.working_directory.is_absolute() {
        return Err("the session's working directory must be absolute".to_owned());
    }
    // Arguments the model got wrong fail at invocation with a clear message; here they only
    // decide which resources the call declares.
    let arguments =
        serde_json::from_value::<Arguments>(without_blanks(request.invocation.arguments.clone()))
            .unwrap_or_default();
    let output_path = match (access, arguments.output_path.as_deref()) {
        (Access::Publish, Some(path)) if !path.is_empty() => {
            Some(inside(&workspace.working_directory, path, "output_path")?)
        }
        _ => None,
    };
    let (requires_permission, operation, category) = match access {
        Access::Read => (
            false,
            read_operation(&definition.name, &workspace.working_directory, &arguments)?,
            CATEGORY_READ,
        ),
        Access::Write => (true, ToolPolicyOperation::Mutating, CATEGORY_WRITE),
        Access::Publish => (
            true,
            output_path
                .as_ref()
                .map_or(ToolPolicyOperation::Mutating, |path| {
                    ToolPolicyOperation::Compound {
                        operations: vec![
                            ToolPolicyOperation::Mutating,
                            ToolPolicyOperation::Write {
                                paths: vec![path.display().to_string()],
                                category: "write".to_owned(),
                            },
                        ],
                    }
                }),
            CATEGORY_PUBLISH,
        ),
    };
    let descriptor = Descriptor {
        session_id: workspace.workspace_id,
        working_directory: workspace.working_directory,
        output_path,
    };
    Ok(ToolPolicyPreparation::new(requires_permission, operation)
        .with_identity(ToolPolicyIdentity {
            aliases: Vec::new(),
            compatibility_aliases: Vec::new(),
            capabilities: Vec::new(),
            permission_category: Some(category.to_owned()),
        })
        .with_descriptor(serde_json::to_value(descriptor).map_err(|error| error.to_string())?))
}

/// `path` resolved inside `root`: it must be relative and must not leave `root`.
fn inside(root: &std::path::Path, path: &str, name: &str) -> Result<PathBuf, String> {
    use std::path::Component;
    let relative = std::path::Path::new(path);
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(format!(
            "`{name}` must be a path relative to the session's working directory that stays inside it"
        ));
    }
    Ok(root.join(relative))
}

/// The policy operation of a read tool: read-only, plus the file it reads for `file_diff` and
/// `file_content`, plus the repository's web address for `pull_request`.
fn read_operation(
    name: &str,
    working_directory: &std::path::Path,
    arguments: &Arguments,
) -> Result<ToolPolicyOperation, String> {
    let resource = match name {
        "code_review.file_diff" | "code_review.file_content" => {
            match arguments.path.as_deref().filter(|path| !path.is_empty()) {
                // Review paths are relative to the repository root.
                Some(path) => {
                    let root = git(working_directory, &["rev-parse", "--show-toplevel"])
                        .map_or_else(|| working_directory.to_path_buf(), PathBuf::from);
                    Some(ToolPolicyOperation::Read {
                        paths: vec![inside(&root, path, "path")?.display().to_string()],
                    })
                }
                None => None,
            }
        }
        "code_review.pull_request" => Some(ToolPolicyOperation::Web {
            url: Some(repository_url(working_directory)),
        }),
        _ => None,
    };
    Ok(resource.map_or(ToolPolicyOperation::ReadOnly, |resource| {
        ToolPolicyOperation::Compound {
            operations: vec![ToolPolicyOperation::ReadOnly, resource],
        }
    }))
}

/// The web address of the repository's `origin`, or GitHub's when it has none.
fn repository_url(working_directory: &std::path::Path) -> String {
    git(working_directory, &["remote", "get-url", "origin"])
        .and_then(|url| bcode_forge_models::RepositoryRef::from_remote_url(&url))
        .map_or_else(
            || format!("https://{}/", bcode_forge::GITHUB_HOST),
            |repository| {
                format!(
                    "https://{}/{}/{}",
                    repository.host, repository.owner, repository.name
                )
            },
        )
}

/// Arguments of the tools; each tool reads the ones it takes.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Arguments {
    review: Option<String>,
    path: Option<String>,
    line: Option<u32>,
    end_line: Option<u32>,
    side: Option<Side>,
    source: Option<String>,
    context: Option<u32>,
    max_lines: Option<u32>,
    start_line: Option<u32>,
    count: Option<u32>,
    include_resolved: Option<bool>,
    offset: Option<usize>,
    limit: Option<usize>,
    body: Option<String>,
    kind: Option<ReviewThreadKind>,
    severity: Option<ReviewThreadSeverity>,
    thread_id: Option<String>,
    resolved: Option<bool>,
    paths: Option<Vec<String>>,
    viewed: Option<bool>,
    replacement: Option<String>,
    rationale: Option<String>,
    publisher: Option<String>,
    submit: Option<bool>,
    output_path: Option<String>,
    include_archived: Option<bool>,
    query: Option<String>,
    sources: Vec<create::SourceArgument>,
    title: Option<String>,
    directory: Option<String>,
    archived: Option<bool>,
    kinds: Vec<ReviewPathMatchKind>,
    include_ignored: Option<bool>,
    reviews: Vec<String>,
    comments: Vec<CommentArgument>,
    thread_ids: Vec<String>,
    glob: Option<String>,
    layers: Vec<String>,
    all: Option<bool>,
    unchanged_since_look: Option<bool>,
}

/// One comment of `add_comments`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CommentArgument {
    path: Option<String>,
    line: Option<u32>,
    end_line: Option<u32>,
    side: Option<Side>,
    source: Option<String>,
    body: Option<String>,
    kind: Option<ReviewThreadKind>,
    severity: Option<ReviewThreadSeverity>,
}

impl CommentArgument {
    fn location(&self) -> Location {
        Location {
            path: self.path.clone(),
            line: self.line,
            end_line: self.end_line,
            side: self.side.unwrap_or_default(),
            source: self.source.clone(),
        }
    }
}

/// One item of a batch call: what it did, or why not.
#[derive(Debug, Serialize)]
struct BatchItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl BatchItem {
    fn of<T: Serialize>(id: Option<String>, result: Result<T, AssistantError>) -> Self {
        match result {
            Ok(value) => Self {
                id,
                ok: true,
                result: serde_json::to_value(value).ok(),
                error: None,
            },
            Err(error) => Self {
                id,
                ok: false,
                result: None,
                error: Some(format!("{error} ({})", error.code())),
            },
        }
    }
}

/// Results of a batch call, with how many succeeded.
fn batch(items: &[BatchItem]) -> Result<String, AssistantError> {
    let succeeded = items.iter().filter(|item| item.ok).count();
    encode(&json!({
        "succeeded": succeeded,
        "failed": items.len() - succeeded,
        "items": items,
    }))
}

/// The review ids `review` and `reviews` name.
fn review_ids(arguments: &Arguments) -> Result<Vec<String>, AssistantError> {
    let mut ids: Vec<String> = arguments
        .review
        .iter()
        .chain(&arguments.reviews)
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
        .collect();
    ids.dedup();
    if ids.is_empty() {
        return Err(AssistantError::Invalid(
            "pass `review` or `reviews` with ids from code_review.list_reviews".to_owned(),
        ));
    }
    if ids.len() > MAX_BATCH {
        return Err(AssistantError::Invalid(format!(
            "at most {MAX_BATCH} reviews per call"
        )));
    }
    Ok(ids)
}

impl Arguments {
    fn location(&self) -> Location {
        Location {
            path: self.path.clone(),
            line: self.line,
            end_line: self.end_line,
            side: self.side.unwrap_or_default(),
            source: self.source.clone(),
        }
    }

    fn thread_query(&self) -> PullThreadQuery {
        PullThreadQuery {
            include_resolved: self.include_resolved.unwrap_or(false),
            path: self.path.clone().filter(|path| !path.is_empty()),
            offset: self.offset.unwrap_or(0),
            limit: self.limit,
        }
    }

    fn required<'a>(value: Option<&'a String>, name: &str) -> Result<&'a str, AssistantError> {
        value
            .map(String::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| AssistantError::Invalid(format!("`{name}` is required")))
    }
}

/// Fields whose empty string is a value of its own: an empty `replacement` suggests deleting
/// the lines.
const BLANK_IS_A_VALUE: &[&str] = &["replacement"];

/// Tool arguments with blank optional values taken as not given, except the fields in
/// [`BLANK_IS_A_VALUE`]. Required fields dropped this way are refused as missing.
fn without_blanks(value: Value) -> Value {
    bcode_tool::arguments::without_blank_values(value, BLANK_IS_A_VALUE)
}

/// Run one invocation.
fn invoke(
    data: &ReviewData,
    store: &ReviewStore,
    reset: ViewedReset,
    invocation: &ToolInvocationRequest,
) -> ToolInvocationResponse {
    let descriptor =
        match serde_json::from_value::<Descriptor>(invocation.preparation_descriptor.clone()) {
            Ok(descriptor) => descriptor,
            Err(error) => return failure(&format!("invalid review tool descriptor: {error}")),
        };
    let mut arguments =
        match serde_json::from_value::<Arguments>(without_blanks(invocation.arguments.clone())) {
            Ok(arguments) => arguments,
            Err(error) => return failure(&format!("invalid arguments: {error}")),
        };
    data.evict_idle();
    match run(
        data,
        store,
        reset,
        &invocation.name,
        &descriptor,
        &mut arguments,
    ) {
        Ok(output) => ToolInvocationResponse {
            output: bounded(output),
            is_error: false,
            content: Vec::new(),
            full_output: None,
            result: None,
        },
        Err(error) => failure(&format!("{} ({})", error, error.code())),
    }
}

fn run(
    data: &ReviewData,
    store: &ReviewStore,
    reset: ViewedReset,
    name: &str,
    descriptor: &Descriptor,
    arguments: &mut Arguments,
) -> Result<String, AssistantError> {
    let repo = descriptor.working_directory.as_path();
    let mut tools = ReviewTools::new(data, store).with_providers(remembered_providers());
    if let Some(result) = run_reviews(data, &tools, repo, name, arguments) {
        return result;
    }
    let reference = review_ref(arguments, descriptor)?;
    let review = tools.open(repo, &reference)?;
    match name {
        "code_review.summary" => encode(&tools.summary(&review)?),
        "code_review.list_files" => encode(&tools.files(&review, reset)?),
        "code_review.file_diff" => tools.file_diff(
            &review,
            Arguments::required(arguments.path.as_ref(), "path")?,
            arguments.source.as_deref(),
            arguments.context,
            arguments.max_lines,
        ),
        "code_review.file_content" => tools.file_content(
            &review,
            Arguments::required(arguments.path.as_ref(), "path")?,
            arguments.source.as_deref(),
            arguments.side.unwrap_or_default(),
            arguments.start_line,
            arguments.count,
        ),
        "code_review.list_threads" => encode(&tools.threads(&review, &arguments.thread_query())?),
        "code_review.get_thread" => encode(&tools.thread(
            &review,
            Arguments::required(arguments.thread_id.as_ref(), "thread_id")?,
        )?),
        "code_review.pull_request" => pull_request(&review),
        "code_review.navigate" => {
            let label = tools.locate(&review, &arguments.location())?;
            Ok(format!("showing {label} to the reviewer"))
        }
        "code_review.add_comment" => {
            let body = Arguments::required(arguments.body.as_ref(), "body")?;
            let classification =
                (arguments.kind.is_some() || arguments.severity.is_some()).then(|| {
                    (
                        arguments.kind.unwrap_or_default(),
                        arguments.severity.unwrap_or_default(),
                    )
                });
            encode(&tools.add_comment(&review, &arguments.location(), body, classification)?)
        }
        "code_review.reply" => encode(&tools.reply(
            &review,
            Arguments::required(arguments.thread_id.as_ref(), "thread_id")?,
            Arguments::required(arguments.body.as_ref(), "body")?,
        )?),
        "code_review.resolve_thread" => encode(&tools.resolve(
            &review,
            Arguments::required(arguments.thread_id.as_ref(), "thread_id")?,
            arguments.resolved.unwrap_or(true),
        )?),
        "code_review.mark_viewed" => encode(&tools.mark_viewed_selection(
            &review,
            &bcode_code_review::assistant::manage::FileSelection {
                paths: arguments.paths.clone().unwrap_or_default(),
                glob: arguments.glob.clone(),
                layers: arguments.layers.clone(),
                all: arguments.all.unwrap_or(false),
                unchanged_since_look: arguments.unchanged_since_look.unwrap_or(false),
            },
            arguments.viewed.unwrap_or(true),
            reset,
        )?),
        "code_review.add_comments" | "code_review.resolve_threads" => {
            run_batch(&mut tools, &review, name, arguments)
        }
        "code_review.suggest_change" => {
            Arguments::required(arguments.path.as_ref(), "path")?;
            encode(&tools.suggest(
                &review,
                &arguments.location(),
                arguments.body.as_deref().unwrap_or_default(),
                arguments.replacement.as_deref(),
                arguments.rationale.as_deref(),
                now_ms(),
            )?)
        }
        "code_review.publish" => {
            let options = descriptor
                .output_path
                .as_ref()
                .map_or(Value::Null, |path| json!({"output_path": path}));
            tools.publish(
                &review,
                arguments.publisher.as_deref().unwrap_or("local_markdown"),
                arguments.submit.unwrap_or(false),
                options,
            )
        }
        _ => Err(AssistantError::Invalid(format!(
            "unknown review tool {name}"
        ))),
    }
}

/// The review a tool acts on: the one named by `review`, else the session's.
fn review_ref(arguments: &Arguments, descriptor: &Descriptor) -> Result<ReviewRef, AssistantError> {
    Ok(match (&arguments.review, &descriptor.session_id) {
        (Some(review), _) if !review.trim().is_empty() => ReviewRef::Workspace(review.clone()),
        (_, Some(session)) => ReviewRef::Session(session.clone()),
        _ => {
            return Err(AssistantError::NotBound(
                "this session is not attached to a review; pass `review`".to_owned(),
            ));
        }
    })
}

/// Run a batch tool on `review`.
fn run_batch(
    tools: &mut ReviewTools<'_>,
    review: &bcode_code_review::assistant::OpenReview,
    name: &str,
    arguments: &Arguments,
) -> Result<String, AssistantError> {
    match name {
        "code_review.add_comments" => {
            if arguments.comments.len() > MAX_BATCH {
                return Err(AssistantError::Invalid(format!(
                    "at most {MAX_BATCH} comments per call"
                )));
            }
            let items: Vec<BatchItem> = arguments
                .comments
                .iter()
                .map(|comment| {
                    let result =
                        Arguments::required(comment.body.as_ref(), "body").and_then(|body| {
                            let classification =
                                (comment.kind.is_some() || comment.severity.is_some()).then(|| {
                                    (
                                        comment.kind.unwrap_or_default(),
                                        comment.severity.unwrap_or_default(),
                                    )
                                });
                            tools.add_comment(review, &comment.location(), body, classification)
                        });
                    BatchItem::of(comment.path.clone(), result)
                })
                .collect();
            batch(&items)
        }
        "code_review.resolve_threads" => {
            if arguments.thread_ids.len() > MAX_BATCH {
                return Err(AssistantError::Invalid(format!(
                    "at most {MAX_BATCH} threads per call"
                )));
            }
            let resolved = arguments.resolved.unwrap_or(true);
            let items: Vec<BatchItem> = arguments
                .thread_ids
                .iter()
                .map(|thread| {
                    let result = tools.resolve(review, thread, resolved);
                    BatchItem::of(Some(thread.clone()), result)
                })
                .collect();
            batch(&items)
        }
        _ => Err(AssistantError::Invalid(format!(
            "unknown review tool {name}"
        ))),
    }
}

/// Tools that act on saved reviews as a whole rather than on one open review.
const REVIEW_LEVEL_TOOLS: &[&str] = &[
    "code_review.list_reviews",
    "code_review.find_paths",
    "code_review.create_review",
    "code_review.update_review",
    "code_review.delete_review",
];

/// Run the tools that act on saved reviews as a whole; `None` for any other tool.
fn run_reviews(
    data: &ReviewData,
    tools: &ReviewTools<'_>,
    repo: &std::path::Path,
    name: &str,
    arguments: &mut Arguments,
) -> Option<Result<String, AssistantError>> {
    REVIEW_LEVEL_TOOLS
        .contains(&name)
        .then(|| review_level(data, tools, repo, name, arguments))
}

fn review_level(
    data: &ReviewData,
    tools: &ReviewTools<'_>,
    repo: &std::path::Path,
    name: &str,
    arguments: &mut Arguments,
) -> Result<String, AssistantError> {
    match name {
        "code_review.list_reviews" => encode(&tools.review_entries(
            repo,
            arguments.include_archived.unwrap_or(false),
            arguments.query.as_deref(),
        )?),
        "code_review.find_paths" => {
            let query = Arguments::required(arguments.query.as_ref(), "query")?;
            let (root, _) = create::repository_of(repo, arguments.directory.as_deref())?;
            let found = data.paths_find(&ReviewPathsFindRequest {
                repo_path: root,
                query: query.to_owned(),
                kinds: std::mem::take(&mut arguments.kinds),
                include_ignored: arguments.include_ignored.unwrap_or(false),
                limit: arguments
                    .limit
                    .map(|limit| u32::try_from(limit).unwrap_or(u32::MAX)),
            })?;
            encode(&found)
        }
        "code_review.create_review" => {
            let plan = create::plan(
                repo,
                arguments.directory.as_deref(),
                std::mem::take(&mut arguments.sources),
            )?;
            let created =
                tools.create_review(&plan.repo_root, arguments.title.clone(), plan.sources)?;
            encode(&json!({
                "message": format!("created the review {:?} ({})", created.title, created.id),
                "review": created,
                "other_repository": !same_repository(repo, &plan.repo_root),
            }))
        }
        "code_review.update_review" => {
            let ids = review_ids(arguments)?;
            if arguments.title.is_some() && ids.len() > 1 {
                return Err(AssistantError::Invalid(
                    "a title renames one review; pass a single `review`".to_owned(),
                ));
            }
            let change = bcode_code_review::assistant::manage::ReviewChange {
                title: arguments.title.clone(),
                archived: arguments.archived,
            };
            let items: Vec<BatchItem> = ids
                .into_iter()
                .map(|id| {
                    let result = tools.update_review(repo, &id, &change);
                    BatchItem::of(Some(id), result)
                })
                .collect();
            batch(&items)
        }
        _ => {
            let items: Vec<BatchItem> = review_ids(arguments)?
                .into_iter()
                .map(|id| {
                    let result = tools.delete_review(repo, &id);
                    BatchItem::of(Some(id), result)
                })
                .collect();
            batch(&items)
        }
    }
}

/// Providers' remembered pull requests and stacks (the review cache on disk, without any
/// provider), so tools resolve saved pull request reviews without the network.
fn remembered_providers() -> &'static bcode_code_review::provider::ProviderRegistry {
    static REGISTRY: std::sync::OnceLock<bcode_code_review::provider::ProviderRegistry> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| {
        bcode_code_review::provider::ProviderRegistry::new().with_cache(std::sync::Arc::new(
            bcode_code_review::cache::ReviewCache::for_user(),
        ))
    })
}

/// Whether `a` and `b` are in the same repository.
fn same_repository(a: &std::path::Path, b: &std::path::Path) -> bool {
    let root = |path: &std::path::Path| {
        git(path, &["rev-parse", "--show-toplevel"])
            .map(PathBuf::from)
            .and_then(|root| root.canonicalize().ok())
    };
    root(a) == root(b)
}

/// The pull request behind `review`: its pull request sources, else its branch.
fn pull_request(
    review: &bcode_code_review::assistant::OpenReview,
) -> Result<String, AssistantError> {
    use bcode_forge_models::{PullRequestSource as _, RepositoryRef};
    let numbered: Vec<(String, u64)> = review
        .workspace
        .sources
        .iter()
        .filter_map(|source| match &source.kind {
            ReviewSourceKind::PullRequest {
                repository, number, ..
            }
            | ReviewSourceKind::StackLayer {
                repository, number, ..
            } => Some((repository.clone(), *number)),
            ReviewSourceKind::Stack {
                repository, top, ..
            } => Some((repository.clone(), *top)),
            _ => None,
        })
        .collect();
    let origin = git(&review.repo_root, &["remote", "get-url", "origin"])
        .and_then(|url| RepositoryRef::from_remote_url(&url));
    let repository = numbered
        .first()
        .and_then(|(name, _)| {
            let (owner, repo) = name.split_once('/')?;
            Some(match &origin {
                Some(origin) if origin.full_name() == *name => origin.clone(),
                _ => RepositoryRef {
                    host: bcode_forge::GITHUB_HOST.to_owned(),
                    owner: owner.to_owned(),
                    name: repo.to_owned(),
                },
            })
        })
        .or(origin)
        .ok_or_else(|| {
            AssistantError::NotFound(
                "the review names no pull request and the repository has no GitHub origin"
                    .to_owned(),
            )
        })?;
    let branch = git(&review.repo_root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let list = bcode_forge::GitHub::new(review.repo_root.clone())
        .pull_requests(&repository)
        .map_err(|error| AssistantError::NotFound(format!("GitHub: {error}")))?;
    let found = list.pull_requests.into_iter().find(|pull| {
        if numbered.is_empty() {
            branch.as_deref() == Some(pull.head.as_str())
        } else {
            numbered.iter().any(|(_, number)| *number == pull.number)
        }
    });
    found.map_or_else(
        || {
            Err(AssistantError::NotFound(format!(
                "no recent pull request of {} matches this review",
                repository.full_name()
            )))
        },
        |pull| encode(&pull),
    )
}

fn git(repo: &std::path::Path, args: &[&str]) -> Option<String> {
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

fn encode<T: Serialize>(value: &T) -> Result<String, AssistantError> {
    serde_json::to_string_pretty(value)
        .map_err(|error| AssistantError::Invalid(format!("cannot encode the result: {error}")))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// `output` cut to [`MAX_OUTPUT_BYTES`] on a character boundary, noting the cut.
fn bounded(mut output: String) -> String {
    if output.len() <= MAX_OUTPUT_BYTES {
        return output;
    }
    let mut end = MAX_OUTPUT_BYTES;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    output.truncate(end);
    output.push_str("\n… output cut; ask for less");
    output
}

fn failure(message: &str) -> ToolInvocationResponse {
    ToolInvocationResponse {
        output: message.to_owned(),
        is_error: true,
        content: Vec::new(),
        full_output: None,
        result: None,
    }
}

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value)
        .unwrap_or_else(|error| ServiceResponse::error("encode_failed", error.to_string()))
}

use bcode_code_review::assistant::create;

#[cfg(test)]
mod tests;
