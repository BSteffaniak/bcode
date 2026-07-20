//! CLI surface and progress rendering for the one-time legacy stream cleanup.

use crate::CliError;
use bcode_session::legacy_stream_cleanup::{
    CleanupMode, CleanupOutcome, CleanupPhase, CleanupProgress, SessionCleanupReport,
    cleanup_session, discover_session_ids,
};
use bcode_session_models::SessionId;
use serde::Serialize;
use std::io::{IsTerminal as _, Write as _};

#[derive(Debug, Serialize)]
struct CleanupRunReport {
    mode: &'static str,
    sessions_total: usize,
    sessions_handled: usize,
    cleaned: usize,
    would_clean: usize,
    unchanged: usize,
    skipped: usize,
    failed: usize,
    reports: Vec<SessionCleanupReport>,
    failures: Vec<CleanupFailure>,
}

#[derive(Debug, Serialize)]
struct CleanupFailure {
    session_id: SessionId,
    error: String,
}

struct ProgressRenderer {
    enabled: bool,
    total_sessions: usize,
    handled_sessions: usize,
    cleaned: usize,
    unchanged: usize,
    skipped: usize,
    failed: usize,
    current_session: Option<SessionId>,
    phase: CleanupPhase,
    processed_events: usize,
    total_events: usize,
    drawn: bool,
}

impl ProgressRenderer {
    const fn new(enabled: bool, total_sessions: usize) -> Self {
        Self {
            enabled,
            total_sessions,
            handled_sessions: 0,
            cleaned: 0,
            unchanged: 0,
            skipped: 0,
            failed: 0,
            current_session: None,
            phase: CleanupPhase::Scanning,
            processed_events: 0,
            total_events: 0,
            drawn: false,
        }
    }

    fn start_session(&mut self, session_id: SessionId) {
        self.current_session = Some(session_id);
        self.phase = CleanupPhase::Scanning;
        self.processed_events = 0;
        self.total_events = 0;
        self.draw();
    }

    fn update(&mut self, progress: &CleanupProgress) {
        match progress {
            CleanupProgress::PhaseChanged { phase } => {
                self.phase = *phase;
            }
            CleanupProgress::EventsProcessed { processed, total } => {
                self.processed_events = *processed;
                self.total_events = *total;
            }
        }
        self.draw();
    }

    fn finish(&mut self, outcome: Option<CleanupOutcome>) {
        self.handled_sessions += 1;
        match outcome {
            Some(CleanupOutcome::Cleaned | CleanupOutcome::WouldClean) => self.cleaned += 1,
            Some(CleanupOutcome::Unchanged) => self.unchanged += 1,
            Some(CleanupOutcome::Skipped) => self.skipped += 1,
            None => self.failed += 1,
        }
        self.processed_events = self.total_events;
        self.draw();
        self.current_session = None;
    }

    fn draw(&mut self) {
        if !self.enabled {
            return;
        }
        let mut stderr = std::io::stderr().lock();
        if self.drawn {
            let _ = write!(stderr, "\x1b[3A");
        }
        let overall_percent = percent(self.handled_sessions, self.total_sessions);
        let current_percent = percent(self.processed_events, self.total_events);
        let session = self.current_session.map_or_else(
            || "-".to_owned(),
            |id| id.to_string().chars().take(12).collect(),
        );
        let _ = writeln!(
            stderr,
            "\x1b[2KOverall [{}] {:>3}% {}/{} handled",
            bar(overall_percent),
            overall_percent,
            self.handled_sessions,
            self.total_sessions
        );
        let _ = writeln!(
            stderr,
            "\x1b[2KCurrent [{}] {:>3}% {}/{} events",
            bar(current_percent),
            current_percent,
            self.processed_events,
            self.total_events
        );
        let _ = writeln!(
            stderr,
            "\x1b[2K        {session} {} — {} cleaned, {} unchanged, {} skipped, {} failed",
            phase_label(self.phase),
            self.cleaned,
            self.unchanged,
            self.skipped,
            self.failed
        );
        let _ = stderr.flush();
        self.drawn = true;
    }
}

#[derive(Debug, Clone, Copy)]
pub enum RunTarget {
    All,
    Session(SessionId),
}

#[derive(Debug, Clone, Copy)]
pub enum RunMode {
    DryRun,
    Apply,
}

#[derive(Debug, Clone, Copy)]
pub struct RunOptions {
    pub target: RunTarget,
    pub mode: RunMode,
    pub json: bool,
}

/// Run the isolated one-time cleanup command.
pub async fn run(options: RunOptions) -> Result<(), CliError> {
    let root = bcode_config::default_session_store_dir();
    let ids = match options.target {
        RunTarget::All => discover_session_ids(&root)?,
        RunTarget::Session(session_id) => vec![session_id],
    };
    let (mode, apply) = match options.mode {
        RunMode::DryRun => (CleanupMode::DryRun, false),
        RunMode::Apply => (CleanupMode::Apply, true),
    };
    execute(root, ids, mode, apply, options.json).await
}

async fn execute(
    root: std::path::PathBuf,
    ids: Vec<SessionId>,
    mode: CleanupMode,
    apply: bool,
    json: bool,
) -> Result<(), CliError> {
    let progress_enabled = !json && std::io::stderr().is_terminal();
    let mut renderer = ProgressRenderer::new(progress_enabled, ids.len());
    let mut reports = Vec::new();
    let mut failures = Vec::new();

    for id in ids.iter().copied() {
        renderer.start_session(id);
        match cleanup_session(&root, id, mode, |event| renderer.update(&event)).await {
            Ok(report) => {
                if !progress_enabled && !json {
                    eprintln!(
                        "{}: {:?}, {}/{} events pruned",
                        report.session_id,
                        report.outcome,
                        report.events_pruned,
                        report.events_scanned
                    );
                }
                renderer.finish(Some(report.outcome));
                reports.push(report);
            }
            Err(error) => {
                renderer.finish(None);
                eprintln!("{id}: failed: {error}");
                failures.push(CleanupFailure {
                    session_id: id,
                    error: error.to_string(),
                });
            }
        }
    }

    let run_report = CleanupRunReport {
        mode: if apply { "apply" } else { "dry_run" },
        sessions_total: ids.len(),
        sessions_handled: ids.len(),
        cleaned: reports
            .iter()
            .filter(|report| report.outcome == CleanupOutcome::Cleaned)
            .count(),
        would_clean: reports
            .iter()
            .filter(|report| report.outcome == CleanupOutcome::WouldClean)
            .count(),
        unchanged: reports
            .iter()
            .filter(|report| report.outcome == CleanupOutcome::Unchanged)
            .count(),
        skipped: reports
            .iter()
            .filter(|report| report.outcome == CleanupOutcome::Skipped)
            .count(),
        failed: failures.len(),
        reports,
        failures,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&run_report)?);
    } else {
        println!(
            "Handled {}/{} sessions: {} cleaned, {} would clean, {} unchanged, {} skipped, {} failed",
            run_report.sessions_handled,
            run_report.sessions_total,
            run_report.cleaned,
            run_report.would_clean,
            run_report.unchanged,
            run_report.skipped,
            run_report.failed
        );
        for report in &run_report.reports {
            let removable = report
                .payload_bytes_before
                .saturating_sub(report.payload_bytes_after);
            println!(
                "{}: {:?}; {}/{} events; {} payload bytes removable; backup {}",
                report.session_id,
                report.outcome,
                report.events_pruned,
                report.events_scanned,
                removable,
                report
                    .backup_path
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), |path| path.display().to_string())
            );
        }
        for failure in &run_report.failures {
            println!("{}: FAILED — {}", failure.session_id, failure.error);
        }
    }
    Ok(())
}

fn percent(value: usize, total: usize) -> usize {
    value.saturating_mul(100).checked_div(total).unwrap_or(0)
}

fn bar(percent: usize) -> String {
    let filled = percent.min(100) / 5;
    format!("{}{}", "#".repeat(filled), "-".repeat(20 - filled))
}

const fn phase_label(phase: CleanupPhase) -> &'static str {
    match phase {
        CleanupPhase::Scanning => "scanning events",
        CleanupPhase::CreatingBackup => "creating backup",
        CleanupPhase::ReplacingEvents => "replacing transient events",
        CleanupPhase::Validating => "validating",
        CleanupPhase::Compacting => "compacting database",
    }
}
