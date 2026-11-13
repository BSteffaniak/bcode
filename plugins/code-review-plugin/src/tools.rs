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
use bcode_code_review_models::{ReviewSourceKind, ReviewThreadKind, ReviewThreadSeverity};
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
        description: "List the saved code reviews of this repository (id, title, sources).",
        schema: || object(serde_json::Map::new(), &[]),
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
        description: "List the review's changed files with status, added and removed lines, and whether the reviewer marked each viewed.",
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
        description: "Mark files viewed (or not viewed) at their current content.",
        schema: || {
            with_review(
                json!({
                    "paths": {"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "File paths."},
                    "viewed": {"type": "boolean", "description": "true marks viewed (default), false unmarks."},
                }),
                &["paths"],
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
    let arguments = serde_json::from_value::<Arguments>(request.invocation.arguments.clone())
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
    let arguments = match serde_json::from_value::<Arguments>(invocation.arguments.clone()) {
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
        &arguments,
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
    arguments: &Arguments,
) -> Result<String, AssistantError> {
    let repo = descriptor.working_directory.as_path();
    let mut tools = ReviewTools::new(data, store);
    if name == "code_review.list_reviews" {
        return encode(&tools.list_reviews(repo)?);
    }
    let reference = match (&arguments.review, &descriptor.session_id) {
        (Some(review), _) if !review.trim().is_empty() => ReviewRef::Workspace(review.clone()),
        (_, Some(session)) => ReviewRef::Session(session.clone()),
        _ => {
            return Err(AssistantError::NotBound(
                "this session is not attached to a review; pass `review`".to_owned(),
            ));
        }
    };
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
        "code_review.mark_viewed" => encode(&tools.mark_viewed(
            &review,
            arguments.paths.as_deref().unwrap_or_default(),
            arguments.viewed.unwrap_or(true),
        )?),
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

#[cfg(test)]
mod tests;
