#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Ralph loop persistence and orchestration primitives.

//! Local Ralph loop state management for the TUI.

use bcode_session_models::{SessionEvent, SessionEventKind};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use switchy::database::query::{FilterableQuery as _, where_eq};
use switchy::database::schema::{Column, DataType, create_table};
use switchy::database::{Database, DatabaseError, Row};
use switchy::schema::discovery::code::{CodeMigration, CodeMigrationSource};
use switchy::schema::runner::MigrationRunner;

const RALPH_STATE_SUBDIR: &str = "ralph";
const PROGRESS_DOC_FILE_NAME: &str = "progress.md";
// Legacy compatibility snapshot. `ralph.db` is the canonical loop metadata store;
// this file is maintained temporarily for backfill/older-state compatibility.
const LOOP_METADATA_FILE_NAME: &str = "loop.json";
const CONTEXT_PACK_FILE_NAME: &str = "context-pack.json";
const DATABASE_FILE_NAME: &str = "ralph.db";
const MIGRATIONS_TABLE: &str = "ralph_schema_migrations";
const DATABASE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DATABASE_OPEN_INITIAL_RETRY_DELAY: Duration = Duration::from_millis(20);
const DATABASE_OPEN_MAX_RETRY_DELAY: Duration = Duration::from_millis(250);
const DATABASE_OPEN_RETRY_ATTEMPTS: u32 = 5;

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
}

/// Markdown checklist summary for a Ralph progress doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressDocChecklistSummary {
    /// Number of checked checklist items.
    pub checked_count: usize,
    /// Number of unchecked checklist items.
    pub unchecked_count: usize,
    /// Stable fingerprint for checklist lines.
    pub checklist_fingerprint: u64,
}

impl ProgressDocChecklistSummary {
    /// Return whether the checklist has no remaining unchecked items.
    #[must_use]
    pub const fn is_completion_candidate(self) -> bool {
        self.checked_count > 0 && self.unchecked_count == 0
    }
}

/// Analyze checklist state from markdown text.
#[must_use]
pub fn analyze_progress_doc_text(text: &str) -> ProgressDocChecklistSummary {
    let mut summary = ProgressDocChecklistSummary {
        checked_count: 0,
        unchecked_count: 0,
        checklist_fingerprint: FNV_OFFSET_BASIS,
    };
    for line in text.lines().filter_map(checklist_line) {
        match line.state {
            ChecklistState::Checked => {
                summary.checked_count = summary.checked_count.saturating_add(1);
            }
            ChecklistState::Unchecked => {
                summary.unchecked_count = summary.unchecked_count.saturating_add(1);
            }
        }
        update_fingerprint(
            &mut summary.checklist_fingerprint,
            line.normalized.as_bytes(),
        );
    }
    summary
}

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChecklistState {
    Checked,
    Unchecked,
}

struct ChecklistLine<'a> {
    state: ChecklistState,
    normalized: &'a str,
}

fn checklist_line(line: &str) -> Option<ChecklistLine<'_>> {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("- [ ]") {
        return Some(ChecklistLine {
            state: ChecklistState::Unchecked,
            normalized: rest.trim(),
        });
    }
    let rest = trimmed
        .strip_prefix("- [x]")
        .or_else(|| trimmed.strip_prefix("- [X]"))?;
    Some(ChecklistLine {
        state: ChecklistState::Checked,
        normalized: rest.trim(),
    })
}

fn update_fingerprint(fingerprint: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *fingerprint ^= u64::from(*byte);
        *fingerprint = fingerprint.wrapping_mul(FNV_PRIME);
    }
    *fingerprint ^= u64::from(b'\n');
    *fingerprint = fingerprint.wrapping_mul(FNV_PRIME);
}

/// Ralph orchestration prompt kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RalphPromptKind {
    /// Bounded work iteration prompt.
    Work,
    /// Audit prompt.
    Audit,
    /// Replan prompt.
    Replan,
}

/// Build a Ralph orchestration prompt from the latest progress doc state.
///
/// # Errors
///
/// Returns an error when the progress doc cannot be read.
pub fn build_prompt(
    summary: &RalphLoopSummary,
    kind: RalphPromptKind,
) -> Result<String, RalphStateError> {
    let progress_doc = std::fs::read_to_string(&summary.progress_doc_path)?;
    Ok(match kind {
        RalphPromptKind::Work => work_prompt(summary, &progress_doc),
        RalphPromptKind::Audit => audit_prompt(summary, &progress_doc),
        RalphPromptKind::Replan => replan_prompt(summary, &progress_doc),
    })
}

fn work_prompt(summary: &RalphLoopSummary, progress_doc: &str) -> String {
    format!(
        "Read the Ralph progress doc below, complete exactly one meaningful bounded chunk, update the doc honestly, and run relevant validation if practical.\n\n\
         Constraints:\n\
         * Do not mark checklist items complete unless verified.\n\
         * Preserve completed work and decisions.\n\
         * Stop and ask if permission, validation, or product intent is unclear.\n\
         * Keep changes focused on the progress doc goal.\n\n\
         Ralph loop: {loop_name}\n\
         Progress doc path: {progress_doc_path}\n\
         Checked items: {checked}\n\
         Unchecked items: {unchecked}\n\n\
         Progress doc:\n\n{progress_doc}",
        loop_name = summary.loop_name,
        progress_doc_path = summary.progress_doc_path.display(),
        checked = summary.checklist_summary.checked_count,
        unchecked = summary.checklist_summary.unchecked_count
    )
}

fn audit_prompt(summary: &RalphLoopSummary, progress_doc: &str) -> String {
    format!(
        "Audit the repository state against this Ralph progress doc. Verify completed checklist items, validation claims, decisions, and handoff notes. Do not implement new work except minimal inspection needed for the audit. Convert unverified completed items back to unchecked items and record blockers/questions.\n\n\
         Ralph loop: {loop_name}\n\
         Progress doc path: {progress_doc_path}\n\n\
         Progress doc:\n\n{progress_doc}",
        loop_name = summary.loop_name,
        progress_doc_path = summary.progress_doc_path.display()
    )
}

fn replan_prompt(summary: &RalphLoopSummary, progress_doc: &str) -> String {
    format!(
        "Replan this Ralph progress doc. Preserve verified completed work, decisions, and validation results. Convert incomplete or unverified work into clear unchecked checklist items. Keep the plan bounded and actionable for the next single work iteration.\n\n\
         Ralph loop: {loop_name}\n\
         Progress doc path: {progress_doc_path}\n\n\
         Progress doc:\n\n{progress_doc}",
        loop_name = summary.loop_name,
        progress_doc_path = summary.progress_doc_path.display()
    )
}

/// Input for Ralph loop stop-decision evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RalphStopDecisionInput {
    /// Current lifecycle status.
    pub status: RalphLoopStatus,
    /// Completed iteration count.
    pub iteration_count: u64,
    /// Maximum allowed work iterations.
    pub max_iterations: u64,
    /// Consecutive iterations with no checklist fingerprint change.
    pub no_progress_count: u64,
    /// Maximum allowed consecutive no-progress iterations.
    pub no_progress_limit: u64,
    /// Progress doc checklist summary.
    pub checklist_summary: ProgressDocChecklistSummary,
    /// Whether a permission denial blocked the loop.
    pub permission_denied: bool,
    /// Whether validation is currently blocked.
    pub validation_blocked: bool,
    /// Whether the loop needs a user answer before proceeding.
    pub user_question: bool,
}

/// Ralph loop stop decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RalphStopDecision {
    /// Continue the loop.
    Continue,
    /// Stop because the progress doc appears complete and needs final audit.
    CompletionCandidate,
    /// Stop because the maximum iteration count was reached.
    MaxIterations,
    /// Stop because repeated iterations made no checklist progress.
    RepeatedNoProgress,
    /// Stop because permission was denied.
    PermissionDenied,
    /// Stop because validation is blocked.
    ValidationBlocked,
    /// Stop because a user answer is required.
    UserQuestion,
    /// Stop because the loop is already in a terminal status.
    TerminalStatus,
}

/// Decide whether a Ralph loop should stop before another work iteration.
#[must_use]
pub const fn decide_stop(input: RalphStopDecisionInput) -> RalphStopDecision {
    if matches!(
        input.status,
        RalphLoopStatus::Stopped | RalphLoopStatus::Done
    ) {
        return RalphStopDecision::TerminalStatus;
    }
    if input.permission_denied {
        return RalphStopDecision::PermissionDenied;
    }
    if input.validation_blocked {
        return RalphStopDecision::ValidationBlocked;
    }
    if input.user_question {
        return RalphStopDecision::UserQuestion;
    }
    if input.checklist_summary.is_completion_candidate() {
        return RalphStopDecision::CompletionCandidate;
    }
    if input.max_iterations > 0 && input.iteration_count >= input.max_iterations {
        return RalphStopDecision::MaxIterations;
    }
    if input.no_progress_limit > 0 && input.no_progress_count >= input.no_progress_limit {
        return RalphStopDecision::RepeatedNoProgress;
    }
    RalphStopDecision::Continue
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
    /// Progress doc checklist summary.
    pub checklist_summary: ProgressDocChecklistSummary,
    updated_at_ms: u128,
}

/// Return the most recently updated Ralph loop for a repository.
///
/// # Errors
///
/// Returns an error when state directory entries or metadata files cannot be
/// read. Missing repository state is treated as an empty result.
pub fn latest_loop(repo_root: &Path) -> Result<Option<RalphLoopSummary>, RalphStateError> {
    let repo_root = repo_root.to_path_buf();
    let db_repo_root = repo_root.clone();
    let db_summary = with_database(move |database| {
        Box::pin(async move {
            let rows = database
                .select("ralph_loops")
                .columns(&[
                    "state_dir",
                    "loop_name",
                    "progress_doc_path",
                    "work_area_path",
                    "session_id",
                    "status",
                    "iteration_count",
                    "max_iterations",
                    "no_progress_count",
                    "no_progress_limit",
                    "updated_at_ms",
                ])
                .filter(Box::new(where_eq(
                    "repo_root",
                    db_repo_root.display().to_string(),
                )))
                .execute(database)
                .await?;
            rows.into_iter()
                .map(|row| summary_from_loop_row(&row))
                .collect::<Result<Vec<_>, _>>()
                .map(|mut summaries| {
                    summaries.sort_by(|left, right| right.updated_at_ms.cmp(&left.updated_at_ms));
                    summaries.into_iter().next()
                })
        })
    })?;
    if db_summary.is_some() {
        return Ok(db_summary);
    }
    latest_loop_from_legacy_files(repo_root.as_path())
}

fn latest_loop_from_legacy_files(
    repo_root: &Path,
) -> Result<Option<RalphLoopSummary>, RalphStateError> {
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
    if let Some(summary) = &latest {
        let legacy_state = CreatedRalphLoopState {
            state_dir: summary.state_dir.clone(),
            progress_doc_path: summary.progress_doc_path.clone(),
            metadata_path: summary.state_dir.join(LOOP_METADATA_FILE_NAME),
            context_pack_path: summary.state_dir.join(CONTEXT_PACK_FILE_NAME),
        };
        upsert_loop_row(&legacy_state)?;
    }
    Ok(latest)
}

fn summary_from_loop_row(row: &Row) -> Result<RalphLoopSummary, RalphStateError> {
    let progress_doc_path = PathBuf::from(required_text(row, "progress_doc_path")?);
    let checklist_summary = if progress_doc_path.exists() {
        analyze_progress_doc_text(&std::fs::read_to_string(&progress_doc_path)?)
    } else {
        ProgressDocChecklistSummary {
            checked_count: 0,
            unchecked_count: 0,
            checklist_fingerprint: FNV_OFFSET_BASIS,
        }
    };
    let status = required_text(row, "status")?;
    let iteration_count = i64_to_u64(required_i64(row, "iteration_count")?);
    let max_iterations = i64_to_u64(required_i64(row, "max_iterations")?);
    let no_progress_count = i64_to_u64(required_i64(row, "no_progress_count")?);
    let no_progress_limit = i64_to_u64(required_i64(row, "no_progress_limit")?);
    Ok(RalphLoopSummary {
        loop_name: required_text(row, "loop_name")?,
        status: status.clone(),
        state_dir: PathBuf::from(required_text(row, "state_dir")?),
        progress_doc_path,
        work_area_path: optional_text(row, "work_area_path").map(PathBuf::from),
        session_id: optional_text(row, "session_id"),
        iteration_count,
        next_action: next_action_for_decision(decide_stop(RalphStopDecisionInput {
            status: status_from_str(&status),
            iteration_count,
            max_iterations,
            no_progress_count,
            no_progress_limit,
            checklist_summary,
            permission_denied: false,
            validation_blocked: false,
            user_question: false,
        }))
        .to_owned(),
        checklist_summary,
        updated_at_ms: u128::try_from(required_i64(row, "updated_at_ms")?).unwrap_or(0),
    })
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
    let checklist_summary = if progress_doc_path.exists() {
        analyze_progress_doc_text(&std::fs::read_to_string(&progress_doc_path)?)
    } else {
        ProgressDocChecklistSummary {
            checked_count: 0,
            unchecked_count: 0,
            checklist_fingerprint: FNV_OFFSET_BASIS,
        }
    };
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
        next_action: next_action_for_decision(decide_stop(RalphStopDecisionInput {
            status: status_from_str(
                metadata
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
            ),
            iteration_count: metadata_u64(&metadata, "iteration_count"),
            max_iterations: metadata_u64(&metadata, "max_iterations"),
            no_progress_count: metadata_u64(&metadata, "no_progress_count"),
            no_progress_limit: metadata_u64(&metadata, "no_progress_limit"),
            checklist_summary,
            permission_denied: false,
            validation_blocked: false,
            user_question: false,
        }))
        .to_owned(),
        checklist_summary,
        updated_at_ms: metadata_u128(&metadata, "updated_at_ms"),
    }))
}

fn required_text(row: &Row, column: &'static str) -> Result<String, RalphStateError> {
    row.get(column)
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .ok_or(RalphStateError::MissingColumn(column))
}

fn optional_text(row: &Row, column: &'static str) -> Option<String> {
    row.get(column)
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
}

fn required_i64(row: &Row, column: &'static str) -> Result<i64, RalphStateError> {
    row.get(column)
        .and_then(|value| value.as_i64())
        .ok_or(RalphStateError::MissingColumn(column))
}

fn i64_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
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

fn status_from_str(status: &str) -> RalphLoopStatus {
    match status {
        "created" => RalphLoopStatus::Created,
        "planning" => RalphLoopStatus::Planning,
        "awaiting_approval" => RalphLoopStatus::AwaitingApproval,
        "running" => RalphLoopStatus::Running,
        "auditing" => RalphLoopStatus::Auditing,
        "replanning" => RalphLoopStatus::Replanning,
        "stopped" => RalphLoopStatus::Stopped,
        "done" => RalphLoopStatus::Done,
        _ => RalphLoopStatus::Blocked,
    }
}

const fn next_action_for_decision(decision: RalphStopDecision) -> &'static str {
    match decision {
        RalphStopDecision::Continue => "run the next bounded work iteration",
        RalphStopDecision::CompletionCandidate => "audit completion candidate before marking done",
        RalphStopDecision::MaxIterations => "inspect progress and replan before continuing",
        RalphStopDecision::RepeatedNoProgress => {
            "replan because recent iterations made no progress"
        }
        RalphStopDecision::PermissionDenied => "resolve permission denial before continuing",
        RalphStopDecision::ValidationBlocked => "resolve validation blocker before continuing",
        RalphStopDecision::UserQuestion => "answer the pending question before continuing",
        RalphStopDecision::TerminalStatus => "review final state and handoff notes",
    }
}

/// Ralph lifecycle event kind stored in the Ralph database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RalphLifecycleEventKind {
    /// Loop state was created.
    Created,
    /// Context pack was captured.
    ContextCaptured,
    /// Progress doc was generated or refreshed.
    ProgressDocGenerated,
    /// Isolated work area was created.
    WorkAreaCreated,
    /// Status was viewed.
    StatusViewed,
    /// Progress doc path was opened/viewed.
    ProgressOpened,
    /// Orchestration prompt was prepared.
    PromptPrepared,
}

impl RalphLifecycleEventKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::ContextCaptured => "context_captured",
            Self::ProgressDocGenerated => "progress_doc_generated",
            Self::WorkAreaCreated => "work_area_created",
            Self::StatusViewed => "status_viewed",
            Self::ProgressOpened => "progress_opened",
            Self::PromptPrepared => "prompt_prepared",
        }
    }
}

/// Append a Ralph lifecycle event to the loop database.
///
/// # Errors
///
/// Returns an error when the Ralph database cannot be opened, migrated, or
/// written.
pub fn append_lifecycle_event(
    state: &CreatedRalphLoopState,
    kind: RalphLifecycleEventKind,
    message: &str,
) -> Result<(), RalphStateError> {
    let state_dir = state.state_dir.clone();
    let message = message.to_owned();
    with_database(move |database| {
        Box::pin(async move {
            insert_lifecycle_event(database, &state_dir, kind, &message, None).await?;
            Ok(())
        })
    })
}

/// Append a Ralph lifecycle event using a discovered loop summary.
///
/// # Errors
///
/// Returns an error when the Ralph database cannot be opened, migrated, or
/// written.
pub fn append_lifecycle_event_for_summary(
    summary: &RalphLoopSummary,
    kind: RalphLifecycleEventKind,
    message: &str,
) -> Result<(), RalphStateError> {
    let state_dir = summary.state_dir.clone();
    let message = message.to_owned();
    with_database(move |database| {
        Box::pin(async move {
            insert_lifecycle_event(database, &state_dir, kind, &message, None).await?;
            Ok(())
        })
    })
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
    upsert_loop_row(&paths)?;
    append_lifecycle_event(
        &paths,
        RalphLifecycleEventKind::Created,
        "Ralph loop state created",
    )?;
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
    append_lifecycle_event(
        state,
        RalphLifecycleEventKind::ContextCaptured,
        "Captured bounded context pack",
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
    append_lifecycle_event(
        state,
        RalphLifecycleEventKind::ProgressDocGenerated,
        "Generated progress doc from context pack",
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
    append_lifecycle_event(
        state,
        RalphLifecycleEventKind::WorkAreaCreated,
        "Created isolated work area",
    )?;
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
    upsert_loop_row(state)?;
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
    no_progress_count: u64,
    iteration_count: u64,
    context_pack_path: &'a Path,
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
            no_progress_count: 0,
            iteration_count: 0,
            context_pack_path: &paths.context_pack_path,
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

fn upsert_loop_row(state: &CreatedRalphLoopState) -> Result<(), RalphStateError> {
    let metadata = read_metadata(state)?;
    let state_dir = state.state_dir.clone();
    with_database(move |database| {
        Box::pin(async move {
            let state_dir_text = state_dir.display().to_string();
            let existing = database
                .select("ralph_loops")
                .columns(&["state_dir"])
                .filter(Box::new(where_eq("state_dir", state_dir_text.clone())))
                .execute(database)
                .await?;
            if existing.is_empty() {
                database
                    .insert("ralph_loops")
                    .value("state_dir", state_dir_text)
                    .value("loop_name", metadata_text(&metadata, "loop_name"))
                    .value("repo_id", metadata_text(&metadata, "repo_id"))
                    .value("repo_root", metadata_text(&metadata, "repo_root"))
                    .value(
                        "progress_doc_path",
                        metadata_text(&metadata, "progress_doc_path"),
                    )
                    .value(
                        "context_pack_path",
                        metadata_text(&metadata, "context_pack_path"),
                    )
                    .value(
                        "work_area_path",
                        optional_metadata_text(&metadata, "work_area_path"),
                    )
                    .value("branch", optional_metadata_text(&metadata, "branch"))
                    .value(
                        "session_id",
                        optional_metadata_text(&metadata, "session_id"),
                    )
                    .value("status", metadata_text(&metadata, "status"))
                    .value(
                        "iteration_count",
                        metadata_i64(&metadata, "iteration_count"),
                    )
                    .value("max_iterations", metadata_i64(&metadata, "max_iterations"))
                    .value(
                        "no_progress_count",
                        metadata_i64(&metadata, "no_progress_count"),
                    )
                    .value(
                        "no_progress_limit",
                        metadata_i64(&metadata, "no_progress_limit"),
                    )
                    .value("created_at_ms", metadata_i64(&metadata, "created_at_ms"))
                    .value("updated_at_ms", metadata_i64(&metadata, "updated_at_ms"))
                    .execute(database)
                    .await?;
            } else {
                database
                    .update("ralph_loops")
                    .value(
                        "work_area_path",
                        optional_metadata_text(&metadata, "work_area_path"),
                    )
                    .value("branch", optional_metadata_text(&metadata, "branch"))
                    .value(
                        "session_id",
                        optional_metadata_text(&metadata, "session_id"),
                    )
                    .value("status", metadata_text(&metadata, "status"))
                    .value(
                        "iteration_count",
                        metadata_i64(&metadata, "iteration_count"),
                    )
                    .value(
                        "no_progress_count",
                        metadata_i64(&metadata, "no_progress_count"),
                    )
                    .value("updated_at_ms", metadata_i64(&metadata, "updated_at_ms"))
                    .filter(Box::new(where_eq("state_dir", state_dir_text)))
                    .execute(database)
                    .await?;
            }
            Ok(())
        })
    })
}

fn metadata_text(metadata: &Map<String, Value>, key: &str) -> String {
    optional_metadata_text(metadata, key).unwrap_or_default()
}

fn optional_metadata_text(metadata: &Map<String, Value>, key: &str) -> Option<String> {
    metadata
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn metadata_i64(metadata: &Map<String, Value>, key: &str) -> i64 {
    i64::try_from(metadata_u128(metadata, key)).unwrap_or(i64::MAX)
}

fn with_database<T>(
    operation: impl for<'a> FnOnce(
        &'a dyn Database,
    )
        -> Pin<Box<dyn Future<Output = Result<T, RalphStateError>> + 'a>>
    + Send
    + 'static,
) -> Result<T, RalphStateError>
where
    T: Send + 'static,
{
    let state_root = bcode_config::default_state_dir().join(RALPH_STATE_SUBDIR);
    let database_path = state_root.join(DATABASE_FILE_NAME);
    std::fs::create_dir_all(&state_root)?;
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread Tokio runtime should build");
        runtime.block_on(async {
            let database = open_database(&database_path).await?;
            run_migrations(database.as_ref()).await?;
            operation(database.as_ref()).await
        })
    })
    .join()
    .map_err(|_| RalphStateError::DatabaseWorkerPanicked)?
}

async fn open_database(path: &Path) -> Result<Box<dyn Database>, RalphStateError> {
    let mut attempt = 0_u32;
    let mut delay = DATABASE_OPEN_INITIAL_RETRY_DELAY;
    loop {
        match switchy::database_connection::builder()
            .turso()
            .with_path(path)
            .with_busy_timeout(DATABASE_BUSY_TIMEOUT)
            .with_multiprocess_wal(false)
            .build()
            .await
        {
            Ok(database) => return Ok(database),
            Err(error)
                if is_database_lock_error(&error) && attempt < DATABASE_OPEN_RETRY_ATTEMPTS =>
            {
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2).min(DATABASE_OPEN_MAX_RETRY_DELAY);
            }
            Err(error) => return Err(RalphStateError::DatabaseOpen(error.to_string())),
        }
    }
}

fn is_database_lock_error(error: &impl std::fmt::Display) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("database is locked") || message.contains("busy")
}

async fn run_migrations(database: &dyn Database) -> Result<(), RalphStateError> {
    let runner = MigrationRunner::new(Box::new(ralph_migrations()))
        .with_table_name(MIGRATIONS_TABLE.to_owned());
    runner
        .run(database)
        .await
        .map_err(|error| RalphStateError::Migration(error.to_string()))?;
    Ok(())
}

fn ralph_migrations() -> CodeMigrationSource<'static> {
    let mut source = CodeMigrationSource::new();
    source.add_migration(loops_table_migration());
    source.add_migration(events_table_migration());
    source
}

fn loops_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "001_ralph_loops".to_owned(),
        Box::new(
            create_table("ralph_loops")
                .if_not_exists(true)
                .column(text_column("state_dir"))
                .column(text_column("loop_name"))
                .column(text_column("repo_id"))
                .column(text_column("repo_root"))
                .column(text_column("progress_doc_path"))
                .column(text_column("context_pack_path"))
                .column(nullable_text_column("work_area_path"))
                .column(nullable_text_column("branch"))
                .column(nullable_text_column("session_id"))
                .column(text_column("status"))
                .column(int_column("iteration_count"))
                .column(int_column("max_iterations"))
                .column(int_column("no_progress_count"))
                .column(int_column("no_progress_limit"))
                .column(int_column("created_at_ms"))
                .column(int_column("updated_at_ms"))
                .primary_key("state_dir"),
        ),
        None,
    )
}

fn events_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "002_ralph_events".to_owned(),
        Box::new(
            create_table("ralph_events")
                .if_not_exists(true)
                .column(text_column("event_id"))
                .column(text_column("state_dir"))
                .column(text_column("kind"))
                .column(text_column("message"))
                .column(nullable_text_column("payload_json"))
                .column(int_column("occurred_at_ms"))
                .primary_key("event_id"),
        ),
        None,
    )
}

fn text_column(name: &str) -> Column {
    Column {
        name: name.to_owned(),
        nullable: false,
        auto_increment: false,
        data_type: DataType::Text,
        default: None,
    }
}

fn nullable_text_column(name: &str) -> Column {
    Column {
        name: name.to_owned(),
        nullable: true,
        auto_increment: false,
        data_type: DataType::Text,
        default: None,
    }
}

fn int_column(name: &str) -> Column {
    Column {
        name: name.to_owned(),
        nullable: false,
        auto_increment: false,
        data_type: DataType::BigInt,
        default: None,
    }
}

async fn insert_lifecycle_event(
    database: &dyn Database,
    state_dir: &Path,
    kind: RalphLifecycleEventKind,
    message: &str,
    payload_json: Option<&str>,
) -> Result<(), RalphStateError> {
    database
        .insert("ralph_events")
        .value("event_id", uuid::Uuid::new_v4().to_string())
        .value("state_dir", state_dir.display().to_string())
        .value("kind", kind.as_str())
        .value("message", message.to_owned())
        .value("payload_json", payload_json.map(ToOwned::to_owned))
        .value("occurred_at_ms", u128_to_i64(now_ms()))
        .execute(database)
        .await?;
    Ok(())
}

fn u128_to_i64(value: u128) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
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
    /// State database failed.
    #[error("Ralph state database failed: {0}")]
    Database(#[from] DatabaseError),
    /// State database open failed.
    #[error("Ralph state database open failed: {0}")]
    DatabaseOpen(String),
    /// State database migration failed.
    #[error("Ralph state database migration failed: {0}")]
    Migration(String),
    /// Required database column was missing or had an unexpected type.
    #[error("Ralph state database row missing column {0}")]
    MissingColumn(&'static str),
    /// State database worker panicked.
    #[error("Ralph state database worker panicked")]
    DatabaseWorkerPanicked,
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

    #[test]
    fn analyzes_progress_doc_checklists() {
        let summary = analyze_progress_doc_text(
            "# Progress\n\n- [x] done\n- [ ] pending\n  - [X] nested done\nnot a checklist\n",
        );
        assert_eq!(summary.checked_count, 2);
        assert_eq!(summary.unchecked_count, 1);
        assert!(!summary.is_completion_candidate());
    }

    #[test]
    fn detects_completion_candidates() {
        let summary = analyze_progress_doc_text("- [x] implemented\n- [X] validated\n");
        assert_eq!(summary.checked_count, 2);
        assert_eq!(summary.unchecked_count, 0);
        assert!(summary.is_completion_candidate());
    }

    #[test]
    fn checklist_fingerprint_changes_when_items_change() {
        let first = analyze_progress_doc_text("- [ ] first\n");
        let second = analyze_progress_doc_text("- [ ] second\n");
        assert_ne!(first.checklist_fingerprint, second.checklist_fingerprint);
    }

    #[test]
    fn stop_decision_detects_completion_candidate() {
        let decision = decide_stop(RalphStopDecisionInput {
            status: RalphLoopStatus::Running,
            iteration_count: 1,
            max_iterations: 5,
            no_progress_count: 0,
            no_progress_limit: 2,
            checklist_summary: analyze_progress_doc_text("- [x] done\n"),
            permission_denied: false,
            validation_blocked: false,
            user_question: false,
        });
        assert_eq!(decision, RalphStopDecision::CompletionCandidate);
    }

    #[test]
    fn stop_decision_prioritizes_blockers() {
        let decision = decide_stop(RalphStopDecisionInput {
            status: RalphLoopStatus::Running,
            iteration_count: 5,
            max_iterations: 5,
            no_progress_count: 2,
            no_progress_limit: 2,
            checklist_summary: analyze_progress_doc_text("- [ ] pending\n"),
            permission_denied: true,
            validation_blocked: true,
            user_question: true,
        });
        assert_eq!(decision, RalphStopDecision::PermissionDenied);
    }

    #[test]
    fn stop_decision_detects_max_iterations() {
        let decision = decide_stop(RalphStopDecisionInput {
            status: RalphLoopStatus::Running,
            iteration_count: 5,
            max_iterations: 5,
            no_progress_count: 0,
            no_progress_limit: 2,
            checklist_summary: analyze_progress_doc_text("- [ ] pending\n"),
            permission_denied: false,
            validation_blocked: false,
            user_question: false,
        });
        assert_eq!(decision, RalphStopDecision::MaxIterations);
    }

    #[test]
    fn stop_decision_detects_repeated_no_progress() {
        let decision = decide_stop(RalphStopDecisionInput {
            status: RalphLoopStatus::Running,
            iteration_count: 1,
            max_iterations: 5,
            no_progress_count: 2,
            no_progress_limit: 2,
            checklist_summary: analyze_progress_doc_text("- [ ] pending\n"),
            permission_denied: false,
            validation_blocked: false,
            user_question: false,
        });
        assert_eq!(decision, RalphStopDecision::RepeatedNoProgress);
    }

    #[test]
    fn latest_loop_reads_database_rows_and_records_events() {
        let temp = tempfile::tempdir().expect("tempdir should create");
        let repo_root = temp.path().join("repo");
        std::fs::create_dir_all(&repo_root).expect("repo should create");
        let state = create_initial_loop_state(
            &format!("db-backed-{}", uuid::Uuid::new_v4()),
            &repo_root,
            Some("DB backed test"),
        )
        .expect("loop state should create");
        append_lifecycle_event(
            &state,
            RalphLifecycleEventKind::StatusViewed,
            "status viewed in test",
        )
        .expect("event should append");

        let summary = latest_loop(&repo_root)
            .expect("latest loop should query")
            .expect("latest loop should exist");
        assert_eq!(summary.state_dir, state.state_dir);
        assert_eq!(summary.loop_name, state_loop_name(&state));
        assert_eq!(summary.progress_doc_path, state.progress_doc_path);
        let event_count = event_count_for_state(&state.state_dir).expect("events should query");
        assert!(event_count >= 2);
    }

    fn event_count_for_state(state_dir: &Path) -> Result<usize, RalphStateError> {
        let state_dir = state_dir.to_path_buf();
        with_database(move |database| {
            Box::pin(async move {
                let rows = database
                    .select("ralph_events")
                    .columns(&["event_id"])
                    .filter(Box::new(where_eq(
                        "state_dir",
                        state_dir.display().to_string(),
                    )))
                    .execute(database)
                    .await?;
                Ok(rows.len())
            })
        })
    }

    fn state_loop_name(state: &CreatedRalphLoopState) -> String {
        read_metadata(state)
            .expect("metadata should read")
            .get("loop_name")
            .and_then(Value::as_str)
            .expect("loop name should exist")
            .to_owned()
    }
}
