//! `feedback.*` model tools. Every tool acts as [`Actor::Agent`] and returns redacted items.

use std::collections::BTreeSet;

use bcode_feedback_models::{
    Actor, Facet, FeedbackId, FeedbackItem, FeedbackQuery, FeedbackStatus, LinkFeedbackRequest,
    LinkKind, MAX_PAGE_LIMIT, RecordFeedbackRequest, SetFeedbackStatusRequest, Severity, Triage,
    TriageFeedbackRequest, TriageState,
};
use bcode_plugin_sdk::prelude::*;
use bcode_plugin_sdk::{ToolPolicyOperation, ToolPolicyPreparation, prepare_tool_service_response};
use bcode_tool::{
    ToolDefinition, ToolDiscoveryPolicy, ToolInvocationRequest, ToolInvocationResponse, ToolList,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{FeedbackPlugin, now_ms};

const SEARCH: &str = "feedback.search";
const GET: &str = "feedback.get";
const RECORD: &str = "feedback.record";
const TRIAGE: &str = "feedback.triage";
const SET_STATUS: &str = "feedback.set_status";
const LINK: &str = "feedback.link";

/// Tools that only read the store.
const READ_ONLY: [&str; 2] = [SEARCH, GET];

/// Plugin configuration key that, set to `"disabled"`, stops agents resolving feedback.
const AGENT_RESOLUTION_KEY: &str = "agent_resolution";

pub fn invoke(plugin: &mut FeedbackPlugin, context: &NativeServiceContext) -> ServiceResponse {
    match context.request.operation.as_str() {
        bcode_tool::OP_LIST_TOOLS => ServiceResponse::json(
            &ToolList::with_runtime_stable_discovery(definitions(), |definition| {
                ToolDiscoveryPolicy {
                    version: 1,
                    read_only: READ_ONLY.contains(&definition.name.as_str()),
                    capabilities: Vec::new(),
                }
            }),
        )
        .unwrap_or_else(|_| ServiceResponse::error("encode_failed", "tool list encoding failed")),
        bcode_tool::OP_PREPARE_TOOL => {
            prepare_tool_service_response(&context.request, definitions(), |_, definition| {
                Ok(if READ_ONLY.contains(&definition.name.as_str()) {
                    ToolPolicyPreparation::new(false, ToolPolicyOperation::ReadOnly)
                } else {
                    ToolPolicyPreparation::new(true, ToolPolicyOperation::Mutating)
                })
            })
        }
        bcode_tool::OP_INVOKE_TOOL => {
            let Ok(request) = context.request.payload_json::<ToolInvocationRequest>() else {
                return ServiceResponse::error("invalid_request", "invalid feedback tool call");
            };
            let agent_resolution_disabled = context
                .config
                .config
                .get(AGENT_RESOLUTION_KEY)
                .and_then(Value::as_str)
                == Some("disabled");
            let result = run(plugin, &request, agent_resolution_disabled);
            ServiceResponse::json(&ToolInvocationResponse {
                is_error: result.is_err(),
                output: result.unwrap_or_else(|error| error),
                content: Vec::new(),
                full_output: None,
                result: None,
            })
            .unwrap_or_else(|_| {
                ServiceResponse::error("encode_failed", "tool result encoding failed")
            })
        }
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported feedback tool operation",
        ),
    }
}

fn run(
    plugin: &mut FeedbackPlugin,
    request: &ToolInvocationRequest,
    agent_resolution_disabled: bool,
) -> Result<String, String> {
    let now = now_ms();
    let store = plugin.store().map_err(|error| error.to_string())?;
    let arguments = request.arguments.clone();
    let item = match request.name.as_str() {
        SEARCH => {
            let args: SearchArgs = parse(arguments)?;
            let page = store
                .list(&args.into_query()?)
                .map_err(|error| error.to_string())?;
            let items: Vec<FeedbackItem> = page.items.iter().map(FeedbackItem::redacted).collect();
            return to_json(&json!({ "items": items, "next_before": page.next_before }));
        }
        GET => {
            let args: IdArgs = parse(arguments)?;
            store.get(parse_id(&args.id)?)
        }
        RECORD => {
            let args: RecordArgs = parse(arguments)?;
            store.record(
                &RecordFeedbackRequest {
                    app: args.app,
                    text: args.text,
                    facets: args
                        .facets
                        .into_iter()
                        .map(|facet| Facet::new(facet.key, facet.value))
                        .collect(),
                },
                now,
            )
        }
        TRIAGE => {
            let args: TriageArgs = parse(arguments)?;
            store.triage(
                &TriageFeedbackRequest {
                    id: parse_id(&args.id)?,
                    triage: Triage {
                        title: args.title,
                        summary: args.summary,
                        category: args.category,
                        severity: args.severity,
                        tags: args.tags,
                        open_questions: args.open_questions,
                    },
                },
                now,
            )
        }
        SET_STATUS => {
            let args: StatusArgs = parse(arguments)?;
            if agent_resolution_disabled && args.status == FeedbackStatus::AgentResolved {
                return Err(
                    "agent resolution is disabled by configuration; record a note instead".into(),
                );
            }
            store.set_status(
                &SetFeedbackStatusRequest {
                    id: parse_id(&args.id)?,
                    status: args.status,
                    actor: Actor::Agent,
                    note: args.note,
                    session_id: args.session_id,
                    commits: args.commits,
                },
                now,
            )
        }
        LINK => {
            let args: LinkArgs = parse(arguments)?;
            store.link(
                &LinkFeedbackRequest {
                    from: parse_id(&args.from)?,
                    to: parse_id(&args.to)?,
                    kind: args.kind,
                },
                now,
            )
        }
        other => return Err(format!("unknown feedback tool {other}")),
    }
    .map_err(|error| error.to_string())?;
    to_json(&item.redacted())
}

fn parse<T: for<'de> Deserialize<'de>>(arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments).map_err(|error| format!("invalid arguments: {error}"))
}

fn parse_id(id: &str) -> Result<FeedbackId, String> {
    id.parse()
        .map_err(|error: bcode_feedback_models::FeedbackValidationError| error.to_string())
}

fn to_json(value: &impl serde::Serialize) -> Result<String, String> {
    serde_json::to_string(value).map_err(|_| "feedback result encoding failed".into())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    app: Option<String>,
    #[serde(default)]
    statuses: BTreeSet<FeedbackStatus>,
    triage_state: Option<TriageState>,
    category: Option<String>,
    #[serde(default)]
    tags: BTreeSet<String>,
    #[serde(default)]
    facets: Vec<FacetArg>,
    text: Option<String>,
    before: Option<String>,
    limit: Option<usize>,
}

impl SearchArgs {
    fn into_query(self) -> Result<FeedbackQuery, String> {
        Ok(FeedbackQuery {
            app: self.app,
            statuses: self.statuses,
            triage_state: self.triage_state,
            category: self.category,
            tags: self.tags,
            facets: self
                .facets
                .into_iter()
                .map(|facet| (facet.key, facet.value))
                .collect(),
            text: self.text,
            before: self.before.as_deref().map(parse_id).transpose()?,
            limit: self.limit.unwrap_or(25),
        })
    }
}

/// One facet as a key/value record.
///
/// Facets travel as an array of records rather than a JSON object map because strict
/// provider tool dialects close every object and cannot express string-keyed maps.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FacetArg {
    key: String,
    value: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdArgs {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordArgs {
    app: String,
    text: String,
    #[serde(default)]
    facets: Vec<FacetArg>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TriageArgs {
    id: String,
    title: String,
    summary: Option<String>,
    category: Option<String>,
    severity: Option<Severity>,
    #[serde(default)]
    tags: BTreeSet<String>,
    #[serde(default)]
    open_questions: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    id: String,
    status: FeedbackStatus,
    note: Option<String>,
    session_id: Option<String>,
    #[serde(default)]
    commits: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkArgs {
    from: String,
    to: String,
    kind: LinkKind,
}

pub fn definitions() -> Vec<ToolDefinition> {
    let id = json!({"type": "string", "description": "Feedback id, such as fb-12"});
    let statuses = json!(["open", "agent_resolved", "verified", "dismissed"]);
    let label = json!({"type": "string", "pattern": "^[a-z0-9._-]{1,64}$"});
    let facets = |description: &str| {
        json!({
            "type": "array",
            "description": description,
            "items": {
                "type": "object",
                "properties": {
                    "key": {"type": "string", "pattern": "^[a-z0-9._-]{1,64}$"},
                    "value": {"type": "string"}
                },
                "required": ["key", "value"],
                "additionalProperties": false
            }
        })
    };
    vec![
        tool(
            SEARCH,
            "Search recorded feedback across every application, newest first. Filters combine; \
             use it to find earlier feedback, duplicates, or related items before triaging. \
             Sensitive facet values are redacted.",
            &json!({
                "app": {"type": "string", "description": "Exact application id, such as bdata or bmux"},
                "statuses": {"type": "array", "items": {"enum": statuses}},
                "triage_state": {"enum": ["pending", "triaged"]},
                "category": label,
                "tags": {"type": "array", "items": label, "description": "Items must have every tag"},
                "facets": facets("Exact facet key/value pairs; items must match every one"),
                "text": {"type": "string", "description": "Case-insensitive substring of the text, title or summary"},
                "before": {"type": "string", "description": "next_before from a previous page"},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_PAGE_LIMIT}
            }),
            &[],
        ),
        tool(
            GET,
            "Read one feedback item.",
            &json!({ "id": id }),
            &["id"],
        ),
        tool(
            RECORD,
            "Record new feedback the person gave in this conversation about an application. \
             Use the person's words for text.",
            &json!({
                "app": {"type": "string", "pattern": "^[a-z0-9._-]{1,64}$", "description": "Application id, such as bcode, bdata or bmux"},
                "text": {"type": "string"},
                "facets": facets("Context such as [{\"key\": \"bcode.model\", \"value\": \"...\"}]")
            }),
            &["app", "text"],
        ),
        tool(
            TRIAGE,
            "Set or replace a feedback item's triage: a one-line title, a summary of the problem \
             and expectation, category, severity, tags, and questions only the person can answer.",
            &json!({
                "id": id,
                "title": {"type": "string", "maxLength": 200},
                "summary": {"type": "string"},
                "category": label,
                "severity": {"enum": ["low", "medium", "high", "critical"]},
                "tags": {"type": "array", "items": label, "maxItems": 16},
                "open_questions": {"type": "array", "items": {"type": "string"}, "maxItems": 8}
            }),
            &["id", "title"],
        ),
        tool(
            SET_STATUS,
            "Mark feedback agent_resolved once a change addresses it, with a note, the session \
             and commits, or reopen it. Only the person can verify or dismiss feedback.",
            &json!({
                "id": id,
                "status": {"enum": ["open", "agent_resolved"]},
                "note": {"type": "string"},
                "session_id": {"type": "string"},
                "commits": {"type": "array", "items": {"type": "string"}, "maxItems": 16}
            }),
            &["id", "status"],
        ),
        tool(
            LINK,
            "Link one feedback item to another: duplicate (same problem), related (shared area \
             or likely fix), or caused_by.",
            &json!({
                "from": id,
                "to": id,
                "kind": {"enum": ["duplicate", "related", "caused_by"]}
            }),
            &["from", "to", "kind"],
        ),
    ]
}

fn tool(name: &str, description: &str, properties: &Value, required: &[&str]) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        }),
    }
}
