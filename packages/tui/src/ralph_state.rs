//! Local Ralph loop state management for the TUI.

use bcode_session_models::{SessionEvent, SessionEventKind};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const RALPH_STATE_SUBDIR: &str = "ralph";
const PROGRESS_DOC_FILE_NAME: &str = "progress.md";
const LOOP_METADATA_FILE_NAME: &str = "loop.json";
const CONTEXT_PACK_FILE_NAME: &str = "context-pack.json";
const AUDIT_HISTORY_FILE_NAME: &str = "audit-history.jsonl";

/// Ralph loop lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RalphLoopStatus {
    /// Loop was created and has local state.
    Created,
    /// Loop is collecting or refreshing conversation context.
    Planning,
    /// Loop is waiting for user approval.
    AwaitingApproval,
    /// Loop is running a bounded work iteration.
    Running,
    /// Loop is auditing repository state against the progress doc.
    Auditing,
    /// Loop is updating the remaining plan.
    Replanning,
    /// Loop stopped before completion.
    Stopped,
    /// Loop is blocked on validation, permission, or a user question.
    Blocked,
    /// Loop is complete.
    Done,
}

impl RalphLoopStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Planning => "planning",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Running => "running",
            Self::Auditing => "auditing",
            Self::Replanning => "replanning",
            Self::Stopped => "stopped",
            Self::Blocked => "blocked",
            Self::Done => "done",
        }
    }
}

const ALL_RALPH_LOOP_STATUSES: [RalphLoopStatus; 9] = [
    RalphLoopStatus::Created,
    RalphLoopStatus::Planning,
    RalphLoopStatus::AwaitingApproval,
    RalphLoopStatus::Running,
    RalphLoopStatus::Auditing,
    RalphLoopStatus::Replanning,
    RalphLoopStatus::Stopped,
    RalphLoopStatus::Blocked,
    RalphLoopStatus::Done,
];

/// Created Ralph loop state paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedRalphLoopState {
    /// Directory containing this Ralph loop's local state.
    pub state_dir: PathBuf,
    /// Canonical progress document path.
    pub progress_doc_path: PathBuf,
    /// Loop metadata path.
    pub metadata_path: PathBuf,
    /// Context pack sidecar path.
    pub context_pack_path: PathBuf,
    /// Audit history sidecar path.
    pub audit_history_path: PathBuf,
}

/// Summary of a discoverable Ralph loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RalphLoopSummary {
    /// User-facing loop name.
    pub loop_name: String,
    /// Current lifecycle status.
    pub status: String,
    /// Loop state directory.
    pub state_dir: PathBuf,
    /// Canonical progress document path.
    pub progress_doc_path: PathBuf,
    /// Isolated work area path, when created.
    pub work_area_path: Option<PathBuf>,
    /// Session ID rooted at the isolated work area, when created.
    pub session_id: Option<String>,
    /// Completed iteration count.
    pub iteration_count: u64,
    /// Suggested next action.
    pub next_action: String,
    updated_at_ms: u128,
}

/// Return the most recently updated Ralph loop for a repository.
///
/// # Errors
///
/// Returns an error when state directory entries or metadata files cannot be
/// read. Missing repository state is treated as an empty result.
pub fn latest_loop(repo_root: &Path) -> Result<Option<RalphLoopSummary>, RalphStateError> {
    let root = repo_state_root(repo_root);
    if !root.exists() {
        return Ok(None);
    }
    let mut latest = None;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let metadata_path = entry.path().join(LOOP_METADATA_FILE_NAME);
        if !metadata_path.exists() {
            continue;
        }
        let Some(summary) = read_loop_summary(&metadata_path)? else {
            continue;
        };
        if latest
            .as_ref()
            .is_none_or(|current: &RalphLoopSummary| summary.updated_at_ms > current.updated_at_ms)
        {
            latest = Some(summary);
        }
    }
    Ok(latest)
}

fn read_loop_summary(metadata_path: &Path) -> Result<Option<RalphLoopSummary>, RalphStateError> {
    let bytes = std::fs::read(metadata_path)?;
    let metadata =
        serde_json::from_slice::<Map<String, Value>>(&bytes).map_err(RalphStateError::Json)?;
    let Some(state_dir) = metadata_path.parent().map(Path::to_path_buf) else {
        return Ok(None);
    };
    let progress_doc_path = metadata
        .get("progress_doc_path")
        .and_then(Value::as_str)
        .map_or_else(|| state_dir.join(PROGRESS_DOC_FILE_NAME), PathBuf::from);
    Ok(Some(RalphLoopSummary {
        loop_name: metadata
            .get("loop_name")
            .and_then(Value::as_str)
            .unwrap_or("Ralph loop")
            .to_owned(),
        status: metadata
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        state_dir,
        progress_doc_path,
        work_area_path: metadata
            .get("work_area_path")
            .and_then(Value::as_str)
            .map(PathBuf::from),
        session_id: metadata
            .get("session_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        iteration_count: metadata_u64(&metadata, "iteration_count"),
        next_action: next_action_for_status(
            metadata
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
        )
        .to_owned(),
        updated_at_ms: metadata_u128(&metadata, "updated_at_ms"),
    }))
}

fn metadata_u128(metadata: &Map<String, Value>, key: &str) -> u128 {
    metadata
        .get(key)
        .and_then(|value| match value {
            Value::Number(number) => number.as_u64().map(u128::from),
            Value::String(text) => text.parse::<u128>().ok(),
            _ => None,
        })
        .unwrap_or(0)
}

fn metadata_u64(metadata: &Map<String, Value>, key: &str) -> u64 {
    metadata
        .get(key)
        .and_then(|value| match value {
            Value::Number(number) => number.as_u64(),
            Value::String(text) => text.parse::<u64>().ok(),
            _ => None,
        })
        .unwrap_or(0)
}

fn next_action_for_status(status: &str) -> &'static str {
    match status {
        "created" | "planning" => "review generated progress doc",
        "awaiting_approval" => "approve or edit progress doc before running",
        "running" => "wait for current work iteration or stop the loop",
        "auditing" => "review audit results",
        "replanning" => "review updated unchecked plan items",
        "stopped" => "run /ralph status, inspect blockers, then restart manually",
        "blocked" => "resolve blocker or answer the pending question",
        "done" => "review validation results and final handoff notes",
        _ => "inspect Ralph state and progress doc",
    }
}

/// Create initial local state for a Ralph loop.
///
/// # Errors
///
/// Returns an error when the local state directory or files cannot be written,
/// or when loop metadata cannot be encoded.
pub fn create_initial_loop_state(
    loop_name: &str,
    repo_root: &Path,
    session_title: Option<&str>,
) -> Result<CreatedRalphLoopState, RalphStateError> {
    let paths = allocate_loop_paths(loop_name, repo_root)?;
    std::fs::create_dir_all(&paths.state_dir)?;
    let metadata = LoopMetadata::new(loop_name, repo_root, &paths);
    std::fs::write(
        &paths.metadata_path,
        serde_json::to_vec_pretty(&metadata).map_err(RalphStateError::Json)?,
    )?;
    std::fs::write(
        &paths.progress_doc_path,
        initial_progress_doc(loop_name, repo_root, session_title, &paths),
    )?;
    std::fs::write(
        &paths.context_pack_path,
        initial_context_pack(loop_name, session_title)?,
    )?;
    std::fs::write(&paths.audit_history_path, [])?;
    Ok(paths)
}

/// Write a bounded conversation context pack for a Ralph loop.
///
/// # Errors
///
/// Returns an error when the context pack cannot be encoded or written.
pub fn write_context_pack(
    state: &CreatedRalphLoopState,
    session_title: Option<&str>,
    events: &[SessionEvent],
) -> Result<(), RalphStateError> {
    let pack = ContextPack::from_events(session_title, events);
    std::fs::write(
        &state.context_pack_path,
        serde_json::to_vec_pretty(&pack).map_err(RalphStateError::Json)?,
    )?;
    update_metadata_field(
        state,
        "status",
        Value::String(RalphLoopStatus::Planning.as_str().to_owned()),
    )?;
    Ok(())
}

/// Generate the local progress doc from the current context pack.
///
/// # Errors
///
/// Returns an error when the context pack cannot be read or decoded, or when
/// the progress doc cannot be written.
pub fn generate_progress_doc_from_context(
    state: &CreatedRalphLoopState,
    loop_name: &str,
    repo_root: &Path,
) -> Result<(), RalphStateError> {
    let bytes = std::fs::read(&state.context_pack_path)?;
    let context_pack =
        serde_json::from_slice::<ContextPack>(&bytes).map_err(RalphStateError::Json)?;
    std::fs::write(
        &state.progress_doc_path,
        progress_doc_from_context(loop_name, repo_root, state, &context_pack),
    )?;
    update_metadata_field(
        state,
        "status",
        Value::String(RalphLoopStatus::AwaitingApproval.as_str().to_owned()),
    )?;
    Ok(())
}

/// Record the isolated work area created for a Ralph loop.
///
/// # Errors
///
/// Returns an error when the metadata file cannot be read, decoded, updated, or
/// written.
pub fn record_work_area(
    state: &CreatedRalphLoopState,
    work_area_path: &Path,
    branch: Option<&str>,
    session_id: Option<&str>,
) -> Result<(), RalphStateError> {
    let mut metadata = read_metadata(state)?;
    metadata.insert(
        "work_area_path".to_owned(),
        Value::String(work_area_path.display().to_string()),
    );
    metadata.insert(
        "branch".to_owned(),
        branch.map_or(Value::Null, |branch| Value::String(branch.to_owned())),
    );
    metadata.insert(
        "session_id".to_owned(),
        session_id.map_or(Value::Null, |session_id| {
            Value::String(session_id.to_owned())
        }),
    );
    metadata.insert(
        "status".to_owned(),
        Value::String(RalphLoopStatus::Running.as_str().to_owned()),
    );
    metadata.insert(
        "updated_at_ms".to_owned(),
        Value::from(now_ms().to_string()),
    );
    write_metadata(state, &metadata)?;
    Ok(())
}

fn read_metadata(state: &CreatedRalphLoopState) -> Result<Map<String, Value>, RalphStateError> {
    let bytes = std::fs::read(&state.metadata_path)?;
    serde_json::from_slice::<Map<String, Value>>(&bytes).map_err(RalphStateError::Json)
}

fn write_metadata(
    state: &CreatedRalphLoopState,
    metadata: &Map<String, Value>,
) -> Result<(), RalphStateError> {
    std::fs::write(
        &state.metadata_path,
        serde_json::to_vec_pretty(metadata).map_err(RalphStateError::Json)?,
    )?;
    Ok(())
}

fn update_metadata_field(
    state: &CreatedRalphLoopState,
    key: &str,
    value: Value,
) -> Result<(), RalphStateError> {
    let mut metadata = read_metadata(state)?;
    metadata.insert(key.to_owned(), value);
    metadata.insert(
        "updated_at_ms".to_owned(),
        Value::from(now_ms().to_string()),
    );
    write_metadata(state, &metadata)
}

/// Return the default Ralph state root for a repository.
#[must_use]
pub fn repo_state_root(repo_root: &Path) -> PathBuf {
    bcode_config::default_state_dir()
        .join(RALPH_STATE_SUBDIR)
        .join(repo_state_id(repo_root))
}

fn allocate_loop_paths(
    loop_name: &str,
    repo_root: &Path,
) -> Result<CreatedRalphLoopState, RalphStateError> {
    let root = repo_state_root(repo_root);
    let loop_slug = slugify(loop_name);
    for suffix in 0..100_u8 {
        let candidate_slug = if suffix == 0 {
            loop_slug.clone()
        } else {
            format!("{loop_slug}-{suffix}")
        };
        let state_dir = root.join(candidate_slug);
        if !state_dir.exists() {
            return Ok(CreatedRalphLoopState {
                progress_doc_path: state_dir.join(PROGRESS_DOC_FILE_NAME),
                metadata_path: state_dir.join(LOOP_METADATA_FILE_NAME),
                context_pack_path: state_dir.join(CONTEXT_PACK_FILE_NAME),
                audit_history_path: state_dir.join(AUDIT_HISTORY_FILE_NAME),
                state_dir,
            });
        }
    }
    Err(RalphStateError::LoopNameExhausted(loop_name.to_owned()))
}

#[derive(Debug, Serialize, Deserialize)]
struct ContextPack {
    session_title: Option<String>,
    event_count: usize,
    events: Vec<ContextPackEvent>,
    created_at_ms: u128,
}

impl ContextPack {
    fn from_events(session_title: Option<&str>, events: &[SessionEvent]) -> Self {
        Self {
            session_title: session_title.map(ToOwned::to_owned),
            event_count: events.len(),
            events: events
                .iter()
                .filter_map(ContextPackEvent::from_session_event)
                .collect(),
            created_at_ms: now_ms(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ContextPackEvent {
    sequence: u64,
    kind: String,
    text: String,
}

impl ContextPackEvent {
    fn from_session_event(event: &SessionEvent) -> Option<Self> {
        let (kind, text) = match &event.kind {
            SessionEventKind::UserMessage { text, .. } => ("user_message", text.as_str()),
            SessionEventKind::AssistantMessage { text } => ("assistant_message", text.as_str()),
            SessionEventKind::SystemMessage { text } => ("system_message", text.as_str()),
            SessionEventKind::ContextCompacted { summary, .. } => {
                ("context_compacted", summary.as_str())
            }
            SessionEventKind::SkillInvoked {
                skill_id,
                arguments,
                ..
            } => {
                return Some(Self {
                    sequence: event.sequence,
                    kind: "skill_invoked".to_owned(),
                    text: format!("{skill_id}: {arguments}"),
                });
            }
            _ => return None,
        };
        Some(Self {
            sequence: event.sequence,
            kind: kind.to_owned(),
            text: truncate_context_text(text),
        })
    }
}

fn truncate_context_text(text: &str) -> String {
    const MAX_CONTEXT_EVENT_CHARS: usize = 4_000;
    let mut output = String::new();
    for ch in text.chars().take(MAX_CONTEXT_EVENT_CHARS) {
        output.push(ch);
    }
    if text.chars().count() > MAX_CONTEXT_EVENT_CHARS {
        output.push('…');
    }
    output
}

#[derive(Debug, Serialize)]
struct LoopMetadata<'a> {
    loop_name: &'a str,
    loop_slug: String,
    repo_root: &'a Path,
    repo_id: String,
    progress_doc_path: &'a Path,
    status: RalphLoopStatus,
    stop_reason: Option<&'static str>,
    max_iterations: u64,
    no_progress_limit: u64,
    iteration_count: u64,
    context_pack_path: &'a Path,
    audit_history_path: &'a Path,
    created_at_ms: u128,
    updated_at_ms: u128,
}

impl<'a> LoopMetadata<'a> {
    fn new(loop_name: &'a str, repo_root: &'a Path, paths: &'a CreatedRalphLoopState) -> Self {
        let now_ms = now_ms();
        Self {
            loop_name,
            loop_slug: paths
                .state_dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("ralph-loop")
                .to_owned(),
            repo_root,
            repo_id: repo_state_id(repo_root),
            progress_doc_path: &paths.progress_doc_path,
            status: RalphLoopStatus::Created,
            stop_reason: None,
            max_iterations: 5,
            no_progress_limit: 2,
            iteration_count: 0,
            context_pack_path: &paths.context_pack_path,
            audit_history_path: &paths.audit_history_path,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        }
    }
}

fn initial_progress_doc(
    loop_name: &str,
    repo_root: &Path,
    session_title: Option<&str>,
    paths: &CreatedRalphLoopState,
) -> String {
    let session_title = session_title.unwrap_or("Untitled session");
    format!(
        "# Ralph Loop: {loop_name}\n\n\
         ## Purpose\n\n\
         Track Ralph loop progress captured from Bcode session `{session_title}`.\n\n\
         ## Current status\n\n\
         - **State:** Created\n\
         - **Repository:** `{repo_root}`\n\n\
         ## Definition of done\n\n\
         - [ ] Capture the intended goal, constraints, and non-goals from the current conversation.\n\
         - [ ] Confirm or create the isolated work area for this Ralph loop.\n\
         - [ ] Implement the planned changes in bounded iterations.\n\
         - [ ] Audit the repository state against this progress doc.\n\
         - [ ] Run relevant validation and record the results.\n\n\
         ## Practical checklist\n\n\
         - [ ] Replace this starter checklist with context-specific work items before running automated loop iterations.\n\
         - [ ] Keep completed work checked only after it is actually verified.\n\n\
         ## Decisions\n\n\
         - Ralph created this progress doc in Bcode state, outside the repository.\n\n\
         ## Blockers and questions\n\n\
         - [ ] Confirm the generated checklist reflects the goal before starting long-running work.\n\n\
         ## Session handoff notes\n\n\
         - Canonical progress doc path: `{progress_doc}`\n\
         - Ralph state directory: `{state_dir}`\n",
        repo_root = repo_root.display(),
        progress_doc = paths.progress_doc_path.display(),
        state_dir = paths.state_dir.display()
    )
}

fn initial_context_pack(
    loop_name: &str,
    session_title: Option<&str>,
) -> Result<Vec<u8>, RalphStateError> {
    let value = serde_json::json!({
        "loop_name": loop_name,
        "session_title": session_title,
        "status": "placeholder",
        "known_loop_statuses": ALL_RALPH_LOOP_STATUSES.map(RalphLoopStatus::as_str),
        "notes": [
            "Conversation context capture is not implemented yet.",
            "This sidecar reserves the durable state location for bounded context packs."
        ],
        "created_at_ms": now_ms(),
    });
    serde_json::to_vec_pretty(&value).map_err(RalphStateError::Json)
}

fn progress_doc_from_context(
    loop_name: &str,
    repo_root: &Path,
    paths: &CreatedRalphLoopState,
    context_pack: &ContextPack,
) -> String {
    let latest_user_goal = context_pack
        .events
        .iter()
        .rev()
        .find(|event| event.kind == "user_message")
        .map_or(
            "Review the captured context and refine this goal.",
            |event| event.text.as_str(),
        );
    let recent_context = context_pack
        .events
        .iter()
        .rev()
        .take(8)
        .map(|event| {
            format!(
                "- `{}` #{}: {}",
                event.kind,
                event.sequence,
                markdown_line(&event.text)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let recent_context = if recent_context.is_empty() {
        "- No active-session context was captured. Replace this section manually.".to_owned()
    } else {
        recent_context
    };
    format!(
        "# Ralph Loop: {loop_name}\n\n\
         ## Purpose\n\n\
         {goal}\n\n\
         ## Current status\n\n\
         - **State:** Awaiting approval\n\
         - **Repository:** `{repo_root}`\n\
         - **Captured events:** {event_count}\n\n\
         ## Captured context\n\n\
         {recent_context}\n\n\
         ## Definition of done\n\n\
         - [ ] Confirm the generated goal and checklist match the intended work.\n\
         - [ ] Confirm or create the isolated work area for this Ralph loop.\n\
         - [ ] Implement the planned changes in bounded iterations.\n\
         - [ ] Audit the repository state against this progress doc.\n\
         - [ ] Run relevant validation and record the results.\n\n\
         ## Practical checklist\n\n\
         - [ ] Refine this generated progress doc before starting long-running work.\n\
         - [ ] Convert captured context into specific implementation tasks.\n\
         - [ ] Keep completed work checked only after it is actually verified.\n\n\
         ## Decisions\n\n\
         - Ralph created this progress doc in Bcode state, outside the repository.\n\n\
         ## Blockers and questions\n\n\
         - [ ] Confirm the generated checklist reflects the goal before starting long-running work.\n\n\
         ## Session handoff notes\n\n\
         - Canonical progress doc path: `{progress_doc}`\n\
         - Ralph state directory: `{state_dir}`\n\
         - Context pack path: `{context_pack}`\n",
        goal = markdown_paragraph(latest_user_goal),
        repo_root = repo_root.display(),
        event_count = context_pack.event_count,
        progress_doc = paths.progress_doc_path.display(),
        state_dir = paths.state_dir.display(),
        context_pack = paths.context_pack_path.display()
    )
}

fn markdown_paragraph(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn markdown_line(text: &str) -> String {
    markdown_paragraph(text).replace('`', "'")
}

fn repo_state_id(repo_root: &Path) -> String {
    slugify(&repo_root.to_string_lossy())
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "ralph-loop".to_owned()
    } else {
        slug
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

/// Ralph local state errors.
#[derive(Debug, thiserror::Error)]
pub enum RalphStateError {
    /// State I/O failed.
    #[error("Ralph state I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// State metadata JSON encoding failed.
    #[error("Ralph state JSON failed: {0}")]
    Json(serde_json::Error),
    /// Could not allocate a unique loop state directory.
    #[error("could not allocate a unique Ralph loop state directory for {0}")]
    LoopNameExhausted(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_normalizes_loop_names() {
        assert_eq!(slugify("Session Import Cleanup"), "session-import-cleanup");
        assert_eq!(slugify("  ...  "), "ralph-loop");
        assert_eq!(slugify("Ralph's Loop!"), "ralph-s-loop");
    }

    #[test]
    fn repo_state_root_uses_bcode_state_dir() {
        let root = repo_state_root(Path::new("/tmp/example repo"));
        assert!(root.ends_with(Path::new("ralph/tmp-example-repo")));
    }
}
