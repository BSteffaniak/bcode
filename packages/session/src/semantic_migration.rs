//! Read-only semantic tool-result migration audit helpers.

use crate::db;
use bcode_session_models::{
    FileChangeResult, SessionEvent, SessionEventKind, SessionId, ShellRunResult,
    ToolInvocationPresentation, ToolInvocationResult,
};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors returned by semantic migration audit operations.
#[derive(Debug, Error)]
pub enum SemanticMigrationAuditError {
    /// Filesystem access failed.
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    /// Session database access failed.
    #[error("session database error: {0}")]
    SessionDb(#[from] crate::db::SessionDbError),
}

/// Read-only audit report for a future semantic-result migration.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SemanticMigrationAuditReport {
    /// Session store root scanned by the audit.
    pub root: PathBuf,
    /// Number of session directories scanned.
    pub sessions_scanned: usize,
    /// Number of session databases successfully decoded.
    pub sessions_decoded: usize,
    /// Number of durable events scanned.
    pub events_scanned: usize,
    /// Tool completion counters.
    pub tool_call_finished: ToolCallFinishedAuditCounts,
    /// Legacy presentation event counters.
    pub presentations: PresentationAuditCounts,
    /// Migration readiness counters.
    pub readiness: MigrationReadinessCounts,
    /// Per-session audit summaries.
    pub sessions: Vec<SessionSemanticMigrationAudit>,
    /// Issues requiring review before removing legacy presentation support.
    pub issues: Vec<SemanticMigrationAuditIssue>,
}

/// Tool completion counters for semantic migration audit.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ToolCallFinishedAuditCounts {
    /// Total `ToolCallFinished` events.
    pub total: usize,
    /// Completion events that already contain semantic result data.
    pub with_semantic_result: usize,
    /// Completion events without semantic result data.
    pub without_semantic_result: usize,
    /// Completion events whose legacy result string is terminal JSON.
    pub legacy_terminal_json: usize,
    /// Completion events whose legacy result string is non-terminal JSON.
    pub non_terminal_json: usize,
    /// Completion events whose legacy result string is plain text.
    pub plain_text: usize,
}

/// Presentation event counters for semantic migration audit.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PresentationAuditCounts {
    /// Total legacy `ToolInvocationPresentation` events.
    pub total: usize,
    /// Terminal presentation events.
    pub terminal: usize,
    /// File-change presentation events.
    pub file_change: usize,
    /// Presentation events with one matching tool completion.
    pub matched_to_completion: usize,
    /// Presentation events without a matching tool completion.
    pub orphan: usize,
    /// Presentation events on tool calls with duplicate presentations.
    pub duplicate: usize,
    /// Presentation events that conflict with existing semantic result data.
    pub conflict: usize,
}

/// Migration readiness counters for semantic migration audit.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MigrationReadinessCounts {
    /// Presentation events that can be removed automatically after migration.
    pub removable_presentations: usize,
    /// Semantic terminal results that can be added automatically.
    pub addable_terminal_results: usize,
    /// Semantic file-change results that can be added automatically.
    pub addable_file_change_results: usize,
    /// Sessions that require manual review before migration.
    pub sessions_requiring_review: usize,
}

/// Per-session semantic migration audit summary.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionSemanticMigrationAudit {
    /// Session id.
    pub session_id: SessionId,
    /// Session database path.
    pub path: PathBuf,
    /// Number of events scanned in this session.
    pub events_scanned: usize,
    /// Number of tool completion events.
    pub tool_call_finished: usize,
    /// Number of legacy presentation events.
    pub presentations: usize,
    /// Whether this session needs manual review before migration.
    pub requires_review: bool,
}

/// Detailed audit issue for semantic migration readiness.
#[derive(Debug, Clone, Serialize)]
pub struct SemanticMigrationAuditIssue {
    /// Session id when known.
    pub session_id: Option<SessionId>,
    /// Session database path or candidate path.
    pub path: PathBuf,
    /// Event sequence when known.
    pub event_sequence: Option<u64>,
    /// Tool call id when known.
    pub tool_call_id: Option<String>,
    /// Issue kind.
    pub issue: SemanticMigrationAuditIssueKind,
    /// Human-readable detail.
    pub detail: String,
}

/// Semantic migration audit issue category.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticMigrationAuditIssueKind {
    /// Candidate session directory did not have a parseable session id.
    InvalidSessionDirectory,
    /// Session database could not be decoded strictly.
    DecodeFailed,
    /// Presentation event has no matching completion.
    OrphanPresentation,
    /// More than one presentation event exists for a tool call.
    DuplicatePresentation,
    /// Presentation data conflicts with existing semantic result data.
    ConflictingPresentation,
}

#[derive(Debug, Clone)]
struct CompletionAuditRecord {
    event_sequence: u64,
    semantic_result: Option<ToolInvocationResult>,
}

#[derive(Debug, Clone)]
struct PresentationAuditRecord {
    event_sequence: u64,
    presentation: ToolInvocationPresentation,
}

/// Audit all per-session databases under `root` without mutating them.
///
/// # Errors
///
/// Returns an error if the session root cannot be listed or an individual session database cannot
/// be opened. Strict per-session event decode failures are reported as audit issues instead of
/// failing the whole audit.
pub async fn audit_semantic_result_migration(
    root: impl AsRef<Path>,
) -> Result<SemanticMigrationAuditReport, SemanticMigrationAuditError> {
    let root = root.as_ref().to_path_buf();
    let mut report = SemanticMigrationAuditReport {
        root: root.clone(),
        ..SemanticMigrationAuditReport::default()
    };

    if !root.exists() {
        return Ok(report);
    }

    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            continue;
        }
        report.sessions_scanned += 1;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(session_id) = name.parse::<SessionId>() else {
            report.issues.push(SemanticMigrationAuditIssue {
                session_id: None,
                path,
                event_sequence: None,
                tool_call_id: None,
                issue: SemanticMigrationAuditIssueKind::InvalidSessionDirectory,
                detail: "session directory name is not a session id".to_string(),
            });
            continue;
        };
        let session_db_path = db::session_db_path(&root, session_id);
        if !session_db_path.exists() {
            continue;
        }
        let session_db = db::SessionDb::open_turso(session_id, &session_db_path).await?;
        let events = match session_db.all_events_strict().await {
            Ok(events) => events,
            Err(error) => {
                report.issues.push(SemanticMigrationAuditIssue {
                    session_id: Some(session_id),
                    path: session_db_path,
                    event_sequence: None,
                    tool_call_id: None,
                    issue: SemanticMigrationAuditIssueKind::DecodeFailed,
                    detail: error.to_string(),
                });
                continue;
            }
        };
        report.sessions_decoded += 1;
        audit_session_events(session_id, &session_db_path, &events, &mut report);
    }

    report.readiness.sessions_requiring_review = report
        .sessions
        .iter()
        .filter(|session| session.requires_review)
        .count();

    Ok(report)
}

fn audit_session_events(
    session_id: SessionId,
    path: &Path,
    events: &[SessionEvent],
    report: &mut SemanticMigrationAuditReport,
) {
    let mut session_report = collect_session_audit_records(session_id, path, events, report);
    let (completions, presentations) =
        collect_tool_audit_records(events, &mut session_report, report);
    let requires_review =
        audit_presentation_matches(session_id, path, &completions, &presentations, report);
    session_report.requires_review = requires_review;
    report.sessions.push(session_report);
}

fn collect_session_audit_records(
    session_id: SessionId,
    path: &Path,
    events: &[SessionEvent],
    report: &mut SemanticMigrationAuditReport,
) -> SessionSemanticMigrationAudit {
    report.events_scanned += events.len();
    SessionSemanticMigrationAudit {
        session_id,
        path: path.to_path_buf(),
        events_scanned: events.len(),
        ..SessionSemanticMigrationAudit::default()
    }
}

fn collect_tool_audit_records(
    events: &[SessionEvent],
    session_report: &mut SessionSemanticMigrationAudit,
    report: &mut SemanticMigrationAuditReport,
) -> (
    BTreeMap<String, CompletionAuditRecord>,
    BTreeMap<String, Vec<PresentationAuditRecord>>,
) {
    let mut completions = BTreeMap::<String, CompletionAuditRecord>::new();
    let mut presentations = BTreeMap::<String, Vec<PresentationAuditRecord>>::new();
    for event in events {
        collect_tool_audit_record(
            event,
            session_report,
            report,
            &mut completions,
            &mut presentations,
        );
    }
    (completions, presentations)
}

fn collect_tool_audit_record(
    event: &SessionEvent,
    session_report: &mut SessionSemanticMigrationAudit,
    report: &mut SemanticMigrationAuditReport,
    completions: &mut BTreeMap<String, CompletionAuditRecord>,
    presentations: &mut BTreeMap<String, Vec<PresentationAuditRecord>>,
) {
    match &event.kind {
        SessionEventKind::ToolCallFinished {
            tool_call_id,
            result,
            semantic_result,
            ..
        } => collect_completion_record(
            event.sequence,
            tool_call_id,
            result,
            semantic_result.as_ref(),
            session_report,
            report,
            completions,
        ),
        SessionEventKind::ToolInvocationPresentation {
            tool_call_id,
            presentation,
            ..
        } => collect_presentation_record(
            event.sequence,
            tool_call_id,
            presentation,
            session_report,
            report,
            presentations,
        ),
        _ => {}
    }
}

fn collect_completion_record(
    event_sequence: u64,
    tool_call_id: &str,
    result: &str,
    semantic_result: Option<&ToolInvocationResult>,
    session_report: &mut SessionSemanticMigrationAudit,
    report: &mut SemanticMigrationAuditReport,
    completions: &mut BTreeMap<String, CompletionAuditRecord>,
) {
    session_report.tool_call_finished += 1;
    report.tool_call_finished.total += 1;
    if semantic_result.is_some() {
        report.tool_call_finished.with_semantic_result += 1;
    } else {
        report.tool_call_finished.without_semantic_result += 1;
    }
    classify_legacy_result(result, &mut report.tool_call_finished);
    completions.insert(
        tool_call_id.to_string(),
        CompletionAuditRecord {
            event_sequence,
            semantic_result: semantic_result.cloned(),
        },
    );
}

fn collect_presentation_record(
    event_sequence: u64,
    tool_call_id: &str,
    presentation: &ToolInvocationPresentation,
    session_report: &mut SessionSemanticMigrationAudit,
    report: &mut SemanticMigrationAuditReport,
    presentations: &mut BTreeMap<String, Vec<PresentationAuditRecord>>,
) {
    session_report.presentations += 1;
    report.presentations.total += 1;
    match presentation {
        ToolInvocationPresentation::Terminal { .. } => {
            report.presentations.terminal += 1;
        }
        ToolInvocationPresentation::FileChange { .. } => {
            report.presentations.file_change += 1;
        }
    }
    presentations
        .entry(tool_call_id.to_string())
        .or_default()
        .push(PresentationAuditRecord {
            event_sequence,
            presentation: presentation.clone(),
        });
}

fn audit_presentation_matches(
    session_id: SessionId,
    path: &Path,
    completions: &BTreeMap<String, CompletionAuditRecord>,
    presentations: &BTreeMap<String, Vec<PresentationAuditRecord>>,
    report: &mut SemanticMigrationAuditReport,
) -> bool {
    let mut requires_review = false;
    for (tool_call_id, records) in presentations {
        requires_review |= audit_tool_call_presentations(
            session_id,
            path,
            tool_call_id,
            records,
            completions.get(tool_call_id),
            report,
        );
    }
    requires_review
}

fn audit_tool_call_presentations(
    session_id: SessionId,
    path: &Path,
    tool_call_id: &str,
    records: &[PresentationAuditRecord],
    completion: Option<&CompletionAuditRecord>,
    report: &mut SemanticMigrationAuditReport,
) -> bool {
    let Some(completion) = completion else {
        report_orphan_presentations(session_id, path, tool_call_id, records, report);
        return true;
    };
    report.presentations.matched_to_completion += records.len();
    if records.len() > 1 {
        report_duplicate_presentations(session_id, path, tool_call_id, records, report);
        return true;
    }
    audit_single_presentation(
        session_id,
        path,
        tool_call_id,
        &records[0],
        completion,
        report,
    )
}

fn report_orphan_presentations(
    session_id: SessionId,
    path: &Path,
    tool_call_id: &str,
    records: &[PresentationAuditRecord],
    report: &mut SemanticMigrationAuditReport,
) {
    report.presentations.orphan += records.len();
    for record in records {
        push_issue(
            report,
            session_id,
            path,
            Some(record.event_sequence),
            Some(tool_call_id),
            SemanticMigrationAuditIssueKind::OrphanPresentation,
            "presentation has no matching ToolCallFinished event",
        );
    }
}

fn report_duplicate_presentations(
    session_id: SessionId,
    path: &Path,
    tool_call_id: &str,
    records: &[PresentationAuditRecord],
    report: &mut SemanticMigrationAuditReport,
) {
    report.presentations.duplicate += records.len();
    for record in records {
        push_issue(
            report,
            session_id,
            path,
            Some(record.event_sequence),
            Some(tool_call_id),
            SemanticMigrationAuditIssueKind::DuplicatePresentation,
            "multiple presentation events exist for this tool call",
        );
    }
}

fn audit_single_presentation(
    session_id: SessionId,
    path: &Path,
    tool_call_id: &str,
    record: &PresentationAuditRecord,
    completion: &CompletionAuditRecord,
    report: &mut SemanticMigrationAuditReport,
) -> bool {
    let semantic = semantic_from_presentation(&record.presentation);
    match &completion.semantic_result {
        Some(existing) if existing != &semantic => {
            report.presentations.conflict += 1;
            push_issue(
                report,
                session_id,
                path,
                Some(record.event_sequence),
                Some(tool_call_id),
                SemanticMigrationAuditIssueKind::ConflictingPresentation,
                "presentation does not match existing semantic_result",
            );
            true
        }
        Some(_) => {
            report.readiness.removable_presentations += 1;
            false
        }
        None => {
            report.readiness.removable_presentations += 1;
            count_addable_semantic_result(&semantic, report);
            let _ = completion.event_sequence;
            false
        }
    }
}

const fn count_addable_semantic_result(
    semantic: &ToolInvocationResult,
    report: &mut SemanticMigrationAuditReport,
) {
    match semantic {
        ToolInvocationResult::ShellRun { .. } => {
            report.readiness.addable_terminal_results += 1;
        }
        ToolInvocationResult::FileChange { .. } => {
            report.readiness.addable_file_change_results += 1;
        }
        ToolInvocationResult::Text { .. } | ToolInvocationResult::Json { .. } => {}
    }
}

fn classify_legacy_result(result: &str, counts: &mut ToolCallFinishedAuditCounts) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(result) else {
        counts.plain_text += 1;
        return;
    };
    if value
        .get("mode")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|mode| mode == "terminal")
    {
        counts.legacy_terminal_json += 1;
    } else {
        counts.non_terminal_json += 1;
    }
}

fn semantic_from_presentation(presentation: &ToolInvocationPresentation) -> ToolInvocationResult {
    match presentation {
        ToolInvocationPresentation::Terminal {
            exit_code,
            timed_out,
            cancelled,
            output,
            output_truncated,
            output_bytes,
            retained_output_bytes,
            columns,
            rows,
        } => ToolInvocationResult::ShellRun {
            result: ShellRunResult::Terminal {
                exit_code: *exit_code,
                timed_out: *timed_out,
                cancelled: *cancelled,
                output_tail: output.clone(),
                output_truncated: *output_truncated,
                output_bytes: *output_bytes,
                retained_output_bytes: *retained_output_bytes,
                columns: *columns,
                rows: *rows,
            },
        },
        ToolInvocationPresentation::FileChange {
            tool_name,
            summary,
            path,
        } => ToolInvocationResult::FileChange {
            result: FileChangeResult {
                tool_name: tool_name.clone(),
                summary: summary.clone(),
                path: path.clone(),
            },
        },
    }
}

fn push_issue(
    report: &mut SemanticMigrationAuditReport,
    session_id: SessionId,
    path: &Path,
    event_sequence: Option<u64>,
    tool_call_id: Option<&str>,
    issue: SemanticMigrationAuditIssueKind,
    detail: &str,
) {
    report.issues.push(SemanticMigrationAuditIssue {
        session_id: Some(session_id),
        path: path.to_path_buf(),
        event_sequence,
        tool_call_id: tool_call_id.map(ToOwned::to_owned),
        issue,
        detail: detail.to_string(),
    });
}
