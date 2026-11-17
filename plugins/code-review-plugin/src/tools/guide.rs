//! Guided-review tools (`code_review.guide.*`) and the code reads at pinned revisions
//! (`code_review.search_code`, `code_review.diff_hunks`, `code_review.related_files`): their
//! schemas, the resources each call declares, and their invocation.
//!
//! These tools read their own argument shapes, so they are dispatched before the shared
//! [`super::Arguments`].

use super::{Descriptor, encode, inside};
use bcode_code_review::assistant::code::{CodeSearch, RelatedKind, SearchMode};
use bcode_code_review::assistant::guide::{
    DeclaredContext, GuideExport, GuideGet, GuideInclude, GuideStart, GuideWriteOutcome,
    ScopeInput, StartMode,
};
use bcode_code_review::assistant::{AssistantError, OpenReview, ReviewRef, ReviewTools, Side};
use bcode_code_review::guide::config::{GuideConfig, StrategyChoice};
use bcode_code_review::guide::context::context_paths;
use bcode_code_review::guide::write::{
    GuideOp, MAX_OPS, OutlineInput, SectionWrite, batch_citations,
};
use bcode_code_review::{ReviewData, ReviewStore};
use bcode_code_review_models::{CitationRequest, GuideDepth, GuideStrategy, ReviewLineInterval};
use bcode_plugin_sdk::ToolPolicyOperation;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

/// Tools served here.
pub const TOOLS: &[&str] = &[
    "code_review.search_code",
    "code_review.diff_hunks",
    "code_review.related_files",
    "code_review.guide.start",
    "code_review.guide.write",
    "code_review.guide.get",
    "code_review.guide.check",
    "code_review.guide.plan",
    "code_review.guide.export",
];

/// Paths a call declares as reads at most.
const MAX_DECLARED_PATHS: usize = 200;

fn citation_schema() -> Value {
    json!({
        "type": "object",
        "description": "A cited line range. Give `quote` (the text at the lines) so the citation is verified.",
        "properties": {
            "id": {"type": "string", "description": "Id wanted for it (default the next free c<n>)."},
            "source": {"type": "string", "description": "Source id (default: the source whose diff has the path)."},
            "layer": {"type": "string", "description": "Stack layer as #102 or a source id (alternative to source)."},
            "path": {"type": "string", "description": "Repository-relative path."},
            "side": {"type": "string", "enum": ["new", "old"], "description": "new (default) or old (removed lines, at the base)."},
            "start_line": {"type": "integer", "minimum": 1},
            "end_line": {"type": "integer", "minimum": 1},
            "quote": {"type": "string", "description": "Text expected within the lines; verifies the citation and moves it when the text is found once elsewhere."},
            "symbol": {"type": "string", "description": "Identifier the citation is about; its definitions are suggested when the quote is not found."},
            "relation_hint": {"type": "string", "enum": ["diff", "context"], "description": "Whether you believe the lines are changed lines (diff) or code around or outside the change (context)."},
        },
        "required": ["path", "start_line", "end_line"],
        "additionalProperties": false,
    })
}

fn cite_list_schema(description: &str) -> Value {
    json!({
        "type": "array",
        "description": description,
        "items": {"anyOf": [{"type": "string", "description": "An existing citation id."}, citation_schema()]},
    })
}

fn finding_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": {"type": "string"},
            "kind": {"type": "string", "enum": ["risk", "question", "suggestion", "note"]},
            "severity": {"type": "string", "enum": ["info", "nit", "warning", "blocker"]},
            "text": {"type": "string"},
            "citations": {
                "type": "array",
                "description": "Citation ids, positions (from 1) in the operation's citations, or inline citations.",
                "items": {"anyOf": [{"type": "integer", "minimum": 1}, {"type": "string"}, citation_schema()]},
            },
        },
        "required": ["kind", "text"],
        "additionalProperties": false,
    })
}

/// Fields of a section, shared by outlines, section writes, and area results.
fn section_properties() -> serde_json::Map<String, Value> {
    super::json_map(json!({
        "id": {"type": "string", "description": "Stable section id (s1, s3.1); never reused once removed."},
        "parent": {"type": "string", "description": "Parent section id (new sections and moves); top level when absent."},
        "after": {"type": "string", "description": "Sibling it follows (new sections and moves); last when absent."},
        "kind": {"type": "string", "description": "purpose, scope, reading_order, step, findings, note, topic (with key), or an adaptive or template section key such as risks."},
        "key": {"type": "string", "description": "Topic key, for kind topic."},
        "title": {"type": "string"},
        "body": {"type": "string", "description": "Markdown. Cite with [[c7]] (a citation id) or [[2]] (the second entry of this operation's citations)."},
        "citations": cite_list_schema("Citations in reading order (each a stop in the walkthrough): ids or inline citations."),
        "findings": {"type": "array", "items": finding_schema(), "description": "Risks, questions, suggestions, notes (replacing earlier ones)."},
    }))
}

fn outline_item_schema() -> Value {
    let mut properties = section_properties();
    properties.insert(
        "children".to_owned(),
        json!({"type": "array", "items": {"type": "object"}, "description": "Child sections, same shape."}),
    );
    json!({"type": "object", "properties": properties, "required": ["id"]})
}

/// Schema of `code_review.guide.start`.
pub fn start_schema() -> Value {
    super::with_review(
        json!({
            "guide_id": {"type": "string", "description": "The guide, for new_version or resume."},
            "scope": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["review", "stack", "layer", "since"], "description": "review (default): the whole review; stack: the story across stepped layers; layer: one layer; since: changes since a comparison baseline."},
                    "layer": {"type": "string", "description": "The layer (#102 or a source id), for layer."},
                    "baseline": {"type": "object", "description": "Comparison baseline, for since (default last_looked)."},
                },
                "additionalProperties": false,
            },
            "mode": {"type": "string", "enum": ["new", "new_version", "resume"], "description": "new (default; a new version when the scope already has a guide), new_version, or resume (continue writing a version, taking it over)."},
            "version": {"type": "integer", "minimum": 1, "description": "The version to resume, or that a new version is based on (default the current one)."},
            "copy": {"type": "boolean", "description": "Start a new version with a copy of the version it is based on (to restructure or refresh it) instead of empty."},
            "title": {"type": "string"},
            "depth": {"type": "string", "enum": ["brief", "standard", "deep"]},
            "strategy": {"type": "string", "enum": ["single", "per_layer", "areas"], "description": "How the version is written (recorded with it)."},
            "template": {"type": "string", "description": "default, explain, freeform, or a configured template."},
            "directions": {"type": "string", "description": "The user's directions for this guide, as they gave them (\"focus on the retry semantics; skip tests\")."},
            "outline": {"type": "array", "items": outline_item_schema(), "description": "Planned outline: sections appear at once as planned, to be written with code_review.guide.write."},
        }),
        &[],
    )
}

/// Schema of `code_review.guide.write`.
pub fn write_schema() -> Value {
    let mut op = section_properties();
    op.insert("op".to_owned(), json!({"type": "string", "enum": ["set_outline", "put_section", "add_expansion", "promote", "discard", "remove_section", "move_section", "record_checks", "area_result", "set_status"]}));
    op.insert("sections".to_owned(), json!({"type": "array", "items": outline_item_schema(), "description": "set_outline: the whole tree; area_result: the area's sections."}));
    op.insert(
        "under".to_owned(),
        json!({"type": "string", "description": "add_expansion: the section expanded."}),
    );
    op.insert(
        "section".to_owned(),
        json!({"type": "string", "description": "promote, discard, record_checks: the section."}),
    );
    op.insert(
        "checks".to_owned(),
        json!({"type": "array", "items": {
        "type": "object",
        "properties": {
            "claim": {"type": "string"},
            "status": {"type": "string", "enum": ["supported", "unsupported", "unclear"]},
            "note": {"type": "string"},
            "citations": {"type": "array", "items": {"type": "string"}},
        },
        "required": ["claim", "status"],
        "additionalProperties": false,
    }, "description": "record_checks: the claim checks."}),
    );
    op.insert(
        "area_id".to_owned(),
        json!({"type": "string", "description": "area_result: the area."}),
    );
    op.insert("status".to_owned(), json!({"type": "string", "enum": ["generating", "complete", "failed", "cancelled", "running"], "description": "set_status: generating, complete, failed, cancelled; area_result: running, complete (default), failed."}));
    op.insert(
        "message".to_owned(),
        json!({"type": "string", "description": "Why it failed."}),
    );
    super::with_review(
        json!({
            "guide_id": {"type": "string"},
            "version": {"type": "integer", "minimum": 1, "description": "Default: the version this conversation's grant names, else the current one."},
            "expected_revision": {"type": "integer", "minimum": 0, "description": "Write only if the version is still at this revision (from the last write or get); otherwise a conflict is returned."},
            "strict": {"type": "boolean", "description": "Write nothing when any citation has a problem (default false: problems are kept and reported)."},
            "ops": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_OPS,
                "items": {"type": "object", "properties": op, "required": ["op"], "additionalProperties": false},
                "description": "Applied in order, all or none. set_outline {sections}; put_section {id, parent?, after?, kind?, key?, title?, body?, citations?, findings?} creates (kind and title required) or changes a section; add_expansion {under, id?, title, body, citations?, findings?}; promote {section}; discard {section}; remove_section {id}; move_section {id, parent?, after?}; record_checks {section, checks}; area_result {area_id, sections, status?, message?} (alone); set_status {status, message?}.",
            },
        }),
        &["guide_id", "ops"],
    )
}

/// Schema of `code_review.guide.get`.
pub fn get_schema() -> Value {
    super::with_review(
        json!({
            "guide_id": {"type": "string", "description": "The guide; omit to use the review's only guide, or to list its guides."},
            "version": {"type": "integer", "minimum": 1, "description": "Default the current version."},
            "sections": {"type": "array", "items": {"type": "string"}, "description": "Only these sections' bodies (default all)."},
            "include": {"type": "array", "items": {"type": "string", "enum": ["outline", "bodies", "citations", "freshness", "areas", "threads", "versions", "marks"]}, "description": "Default outline, bodies, citations."},
        }),
        &[],
    )
}

/// Schema of `code_review.guide.check`.
pub fn check_schema() -> Value {
    let mut section = section_properties();
    section.remove("parent");
    section.remove("after");
    super::with_review(
        json!({
            "guide_id": {"type": "string", "description": "Check against this guide's pinned heads (default the review's only guide, else the review's current heads)."},
            "version": {"type": "integer", "minimum": 1},
            "citations": {"type": "array", "items": citation_schema(), "maxItems": 100},
            "section": {"type": "object", "properties": section, "required": ["id"], "additionalProperties": false, "description": "A section as put_section would write it: its citations are checked and it is linted, nothing is written."},
        }),
        &[],
    )
}

/// Schema of `code_review.guide.plan`.
pub fn plan_schema() -> Value {
    super::with_review(
        json!({
            "strategy": {"type": "string", "enum": ["auto", "single", "fan_out"], "description": "Default the configuration's (auto)."},
            "depth": {"type": "string", "enum": ["brief", "standard", "deep"]},
            "template": {"type": "string"},
        }),
        &[],
    )
}

/// Schema of `code_review.guide.export`.
pub fn export_schema() -> Value {
    super::with_review(
        json!({
            "guide_id": {"type": "string"},
            "version": {"type": "integer", "minimum": 1},
            "format": {"type": "string", "enum": ["markdown"]},
            "sections": {"type": "array", "items": {"type": "string"}, "description": "Only these sections and their children."},
            "include_unverified": {"type": "boolean", "description": "Include unverified citations (default true)."},
            "links": {"type": "string", "enum": ["permalink", "relative"], "description": "permalink (default): forge links for pull request sources."},
        }),
        &[],
    )
}

/// Schema of `code_review.search_code`.
pub fn search_schema() -> Value {
    super::with_review(
        json!({
            "query": {"type": "string", "description": "Text, word, POSIX extended regular expression, or identifier."},
            "mode": {"type": "string", "enum": ["text", "word", "regex", "symbol"], "description": "text (default), word, regex, or symbol (definitions, then other uses)."},
            "paths": {"type": "array", "items": {"type": "string"}, "maxItems": 20, "description": "Path globs to search in (src/**, *.toml)."},
            "side": {"type": "string", "enum": ["new", "old"], "description": "new (default): a head; old: a base."},
            "source": {"type": "string", "description": "Source (id, label, or #102) whose revision is read; default the newest head, or the first base for old."},
            "max": {"type": "integer", "minimum": 1, "maximum": 200, "description": "Matches returned (default 100)."},
            "ignore_case": {"type": "boolean"},
        }),
        &["query"],
    )
}

/// Schema of `code_review.diff_hunks`.
pub fn hunks_schema() -> Value {
    super::with_review(
        json!({
            "path": {"type": "string"},
            "source": {"type": "string", "description": "Source id, when the file is in several sources."},
            "side": {"type": "string", "enum": ["new", "old"], "description": "Side the lines are on (default new)."},
            "start_line": {"type": "integer", "minimum": 1},
            "end_line": {"type": "integer", "minimum": 1},
            "context": {"type": "integer", "minimum": 0, "maximum": 20, "description": "Context lines (default 3)."},
        }),
        &["path"],
    )
}

/// Schema of `code_review.related_files`.
pub fn related_schema() -> Value {
    super::with_review(
        json!({
            "path": {"type": "string"},
            "kinds": {"type": "array", "items": {"type": "string", "enum": ["tests", "callers", "config", "docs"]}, "description": "Default all."},
            "source": {"type": "string", "description": "Source whose head is read (default the file's source)."},
            "max": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Files returned (default 30)."},
        }),
        &["path"],
    )
}

/// Arguments of the tools served here; each tool reads the ones it takes.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct GuideArguments {
    review: Option<String>,
    guide_id: Option<String>,
    scope: Option<ScopeInput>,
    mode: Option<String>,
    version: Option<u32>,
    copy: Option<bool>,
    title: Option<String>,
    depth: Option<GuideDepth>,
    strategy: Option<String>,
    template: Option<String>,
    directions: Option<String>,
    outline: Vec<OutlineInput>,
    expected_revision: Option<u64>,
    strict: Option<bool>,
    ops: Vec<Value>,
    sections: Vec<String>,
    include: Vec<GuideInclude>,
    citations: Vec<CitationRequest>,
    section: Option<SectionWrite>,
    format: Option<String>,
    include_unverified: Option<bool>,
    links: Option<String>,
    query: Option<String>,
    paths: Vec<String>,
    side: Option<Side>,
    source: Option<String>,
    max: Option<usize>,
    ignore_case: Option<bool>,
    path: Option<String>,
    start_line: Option<u32>,
    end_line: Option<u32>,
    context: Option<u32>,
    kinds: Vec<RelatedKind>,
}

/// The write operations of `ops`, each named by its position when it cannot be read.
fn parse_ops(ops: &[Value]) -> Result<Vec<GuideOp>, AssistantError> {
    ops.iter()
        .enumerate()
        .map(|(index, op)| {
            serde_json::from_value::<GuideOp>(op.clone()).map_err(|error| {
                AssistantError::Invalid(format!("ops[{index}] cannot be read: {error}"))
            })
        })
        .collect()
}

/// Paths of the inline citations of a write or check, as written.
///
/// # Errors
///
/// * A message for operations that cannot be read: the invocation could not run them either,
///   and their citations would otherwise go undeclared.
fn citation_paths(name: &str, parsed: &GuideArguments) -> Result<Vec<String>, String> {
    let mut requests: Vec<CitationRequest> = match name {
        "code_review.guide.write" => {
            batch_citations(&parse_ops(&parsed.ops).map_err(|error| error.to_string())?)
        }
        _ => parsed.citations.clone(),
    };
    if let Some(section) = parsed.section.clone() {
        requests.extend(batch_citations(&[GuideOp::PutSection(section)]));
    }
    let mut paths: Vec<String> = Vec::new();
    for request in requests {
        if !paths.contains(&request.path) {
            paths.push(request.path);
        }
    }
    Ok(paths)
}

/// The resources a call of `name` declares besides being read-only (or a guide write), and the
/// context files it may read.
///
/// `arguments` are the ones the invocation receives (blank values already removed), so every
/// path the invocation reads is one declared here. Arguments that cannot be read are refused
/// rather than declaring nothing.
///
/// # Errors
///
/// * A message for arguments or operations that cannot be read.
/// * A message for a path that leaves the repository.
/// * A message for a call naming more than [`MAX_DECLARED_PATHS`] paths.
pub fn declared(
    name: &str,
    root: &Path,
    arguments: &Value,
    config: &GuideConfig,
) -> Result<(Vec<ToolPolicyOperation>, Option<DeclaredContextPaths>), String> {
    let parsed: GuideArguments = serde_json::from_value(arguments.clone())
        .map_err(|error| format!("invalid arguments: {error}"))?;
    let mut paths: Vec<String> = Vec::new();
    let mut context = None;
    match name {
        "code_review.search_code" => {
            if parsed.paths.is_empty() {
                paths.push(root.display().to_string());
            }
            for path in &parsed.paths {
                let path = path.trim().trim_start_matches("./");
                paths.push(inside(root, path, "paths")?.display().to_string());
            }
        }
        "code_review.diff_hunks" | "code_review.related_files" => {
            if let Some(path) = parsed.path.as_deref().filter(|path| !path.is_empty()) {
                paths.push(inside(root, path, "path")?.display().to_string());
            }
        }
        "code_review.guide.write" | "code_review.guide.check" => {
            for path in citation_paths(name, &parsed)? {
                paths.push(inside(root, &path, "path")?.display().to_string());
            }
        }
        "code_review.guide.start" | "code_review.guide.plan" => {
            let (files, problems) = context_paths(root, config);
            paths.extend(
                files
                    .iter()
                    .map(|path| root.join(path).display().to_string()),
            );
            context = Some(DeclaredContextPaths {
                paths: files,
                problems,
            });
        }
        _ => {}
    }
    if paths.len() > MAX_DECLARED_PATHS {
        return Err(format!(
            "a call reads at most {MAX_DECLARED_PATHS} distinct paths; this one names {}; split it",
            paths.len()
        ));
    }
    let operations = if paths.is_empty() {
        Vec::new()
    } else {
        vec![ToolPolicyOperation::Read { paths }]
    };
    Ok((operations, context))
}

/// Context files a call declared, carried to its invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct DeclaredContextPaths {
    /// Repository-relative files.
    #[serde(default)]
    pub paths: Vec<String>,
    /// What in the configuration could not be used.
    #[serde(default)]
    pub problems: Vec<String>,
}

/// What a call returns, and whether it is an error.
pub struct Answer {
    /// The output.
    pub output: String,
    /// Whether it is an error.
    pub is_error: bool,
}

impl Answer {
    const fn ok(output: String) -> Self {
        Self {
            output,
            is_error: false,
        }
    }
}

/// `value` when it is given and not blank.
fn required(value: Option<&String>, field: &str) -> Result<String, AssistantError> {
    value
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| AssistantError::Invalid(format!("`{field}` is required")))
}

/// The choice `value` names among `choices` (`None` when it is absent or blank).
fn choice<T: Copy>(
    value: Option<&str>,
    field: &str,
    choices: &[(&str, T)],
) -> Result<Option<T>, AssistantError> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map_or(Ok(None), |value| {
            choices
                .iter()
                .find(|(name, _)| *name == value)
                .map(|(_, choice)| Some(*choice))
                .ok_or_else(|| {
                    AssistantError::Invalid(format!(
                        "`{field}` is {}, not {value}",
                        choices
                            .iter()
                            .map(|(name, _)| *name)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })
        })
}

/// The `guide.start` request `arguments` describe.
fn start_of(arguments: GuideArguments) -> Result<GuideStart, AssistantError> {
    Ok(GuideStart {
        mode: choice(
            arguments.mode.as_deref(),
            "mode",
            &[
                ("new", StartMode::New),
                ("new_version", StartMode::NewVersion),
                ("resume", StartMode::Resume),
            ],
        )?
        .unwrap_or_default(),
        strategy: choice(
            arguments.strategy.as_deref(),
            "strategy",
            &[
                ("single", GuideStrategy::Single),
                ("per_layer", GuideStrategy::PerLayer),
                ("areas", GuideStrategy::Areas),
            ],
        )?,
        guide_id: arguments.guide_id,
        scope: arguments.scope,
        version: arguments.version,
        copy: arguments.copy.unwrap_or(false),
        title: arguments.title,
        depth: arguments.depth,
        template: arguments.template,
        directions: arguments.directions,
        outline: arguments.outline,
    })
}

/// Run `name`.
///
/// # Errors
///
/// * The [`AssistantError`] of a call that failed.
pub fn run(
    data: &ReviewData,
    store: &ReviewStore,
    name: &str,
    descriptor: &Descriptor,
    arguments: Value,
    config: &GuideConfig,
    config_problems: &[String],
) -> Result<Answer, AssistantError> {
    let arguments: GuideArguments = serde_json::from_value(arguments)
        .map_err(|error| AssistantError::Invalid(format!("invalid arguments: {error}")))?;
    let repo = descriptor.working_directory.as_path();
    let mut tools = ReviewTools::new(data, store).with_providers(super::remembered_providers());
    let reference = match (&arguments.review, &descriptor.session_id) {
        (Some(review), _) if !review.trim().is_empty() => ReviewRef::Workspace(review.clone()),
        (_, Some(session)) => ReviewRef::Session(session.clone()),
        _ => {
            return Err(AssistantError::NotBound(
                "this session is not attached to a review; pass `review`".to_owned(),
            ));
        }
    };
    let mut review = tools.open(repo, &reference)?;
    if let (ReviewRef::Workspace(_), Some(session)) = (&reference, &descriptor.session_id) {
        review.guide_grant = tools.session_grant(repo, session)?;
    }
    let call = Call {
        tools: &tools,
        review: &review,
        session: descriptor.session_id.as_deref(),
        config,
        declared: DeclaredContext {
            paths: descriptor
                .context
                .as_ref()
                .map(|context| context.paths.clone())
                .unwrap_or_default(),
            problems: config_problems
                .iter()
                .cloned()
                .chain(
                    descriptor
                        .context
                        .iter()
                        .flat_map(|context| context.problems.iter().cloned()),
                )
                .collect(),
        },
    };
    match name {
        "code_review.search_code" | "code_review.diff_hunks" | "code_review.related_files" => {
            call.code(name, arguments)
        }
        _ => call.guide(name, arguments),
    }
}

/// `value` as compact JSON.
fn compact(value: &Value) -> Result<String, AssistantError> {
    serde_json::to_string(value)
        .map_err(|error| AssistantError::Invalid(format!("cannot encode the result: {error}")))
}

/// One call on an opened review.
struct Call<'a> {
    tools: &'a ReviewTools<'a>,
    review: &'a OpenReview,
    session: Option<&'a str>,
    config: &'a GuideConfig,
    declared: DeclaredContext,
}

impl Call<'_> {
    fn code(&self, name: &str, arguments: GuideArguments) -> Result<Answer, AssistantError> {
        let (tools, review) = (self.tools, self.review);
        match name {
            "code_review.search_code" => {
                let search = CodeSearch {
                    query: required(arguments.query.as_ref(), "query")?,
                    mode: choice(
                        arguments.mode.as_deref(),
                        "mode",
                        &[
                            ("text", SearchMode::Text),
                            ("word", SearchMode::Word),
                            ("regex", SearchMode::Regex),
                            ("symbol", SearchMode::Symbol),
                        ],
                    )?
                    .unwrap_or_default(),
                    paths: arguments.paths,
                    side: arguments.side.unwrap_or_default(),
                    source: arguments.source,
                    max: arguments.max,
                    ignore_case: arguments.ignore_case.unwrap_or(false),
                };
                Ok(Answer::ok(encode(&tools.search_code(review, &search)?)?))
            }
            "code_review.diff_hunks" => {
                let lines = match (arguments.start_line, arguments.end_line) {
                    (Some(start), end) => Some(ReviewLineInterval {
                        start,
                        end: end.unwrap_or(start),
                    }),
                    (None, Some(end)) => Some(ReviewLineInterval { start: end, end }),
                    (None, None) => None,
                };
                Ok(Answer::ok(tools.diff_hunks(
                    review,
                    &required(arguments.path.as_ref(), "path")?,
                    arguments.source.as_deref(),
                    arguments.side.unwrap_or_default(),
                    lines,
                    arguments.context,
                )?))
            }
            _ => Ok(Answer::ok(encode(&tools.related_files(
                review,
                &required(arguments.path.as_ref(), "path")?,
                arguments.source.as_deref(),
                &arguments.kinds,
                arguments.max,
            )?)?)),
        }
    }

    fn guide(&self, name: &str, arguments: GuideArguments) -> Result<Answer, AssistantError> {
        let (tools, review, config) = (self.tools, self.review, self.config);
        match name {
            "code_review.guide.start" => {
                let start = start_of(arguments)?;
                Ok(Answer::ok(encode(&tools.guide_start(
                    review,
                    self.session,
                    &start,
                    config,
                    &self.declared,
                )?)?))
            }
            "code_review.guide.write" => {
                let ops = parse_ops(&arguments.ops)?;
                let outcome = tools.guide_write(
                    review,
                    self.session,
                    &required(arguments.guide_id.as_ref(), "guide_id")?,
                    arguments.version,
                    arguments.expected_revision,
                    arguments.strict.unwrap_or(false),
                    ops,
                    config,
                )?;
                Ok(match outcome {
                    GuideWriteOutcome::Written(value) => Answer::ok(encode(&value)?),
                    GuideWriteOutcome::Refused(value) => Answer {
                        output: encode(&value)?,
                        is_error: true,
                    },
                })
            }
            // Compact, as the answer's budget is measured.
            "code_review.guide.get" => Ok(Answer::ok(compact(&tools.guide_get(
                review,
                &GuideGet {
                    guide_id: arguments.guide_id,
                    version: arguments.version,
                    sections: arguments.sections,
                    include: arguments.include,
                },
            )?)?)),
            "code_review.guide.check" => Ok(Answer::ok(encode(&tools.guide_check(
                review,
                arguments.guide_id.as_deref(),
                arguments.version,
                &arguments.citations,
                arguments.section,
                config,
            )?)?)),
            "code_review.guide.plan" => {
                let strategy = choice(
                    arguments.strategy.as_deref(),
                    "strategy",
                    &[
                        ("auto", StrategyChoice::Auto),
                        ("single", StrategyChoice::Single),
                        ("fan_out", StrategyChoice::FanOut),
                    ],
                )?;
                Ok(Answer::ok(encode(&tools.guide_plan(
                    review,
                    config,
                    strategy,
                    arguments.depth,
                    arguments.template.as_deref(),
                    &self.declared,
                )?)?))
            }
            "code_review.guide.export" => Ok(Answer::ok(tools.guide_export(
                review,
                &GuideExport {
                    guide_id: arguments.guide_id,
                    version: arguments.version,
                    format: arguments.format,
                    sections: arguments.sections,
                    include_unverified: arguments.include_unverified,
                    links: arguments.links,
                },
            )?)),
            _ => Err(AssistantError::Invalid(format!(
                "unknown review tool {name}"
            ))),
        }
    }
}
