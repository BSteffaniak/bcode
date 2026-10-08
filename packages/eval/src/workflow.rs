//! Real product workflow execution. Never reads daemon databases or private sidecars.
use super::idle_timeout::{ExecutionClock, ProgressMark, TimeoutReason};
use super::{
    Command, EvalArtifactRef, EvalCase, EvalDiagnostic, EvalDiagnosticSeverity, EvalError,
    EvalMeasurementSet, EvalSuite, EvalVariant, ExecutionOutput, Instant, Path, fs,
    prepare_agent_daemon_isolation, process, rendered_prompt, session_telemetry, write_json,
    write_session_events_jsonl,
};
use bcode_eval_models::{EvalTimeoutMode, WorkflowEvalConfig};
use sha2::Digest;
use std::time::Duration;

#[path = "workflow_capture/mod.rs"]
mod workflow_capture;

fn execution_duration(start: Instant, observed_end: Instant) -> Duration {
    observed_end.saturating_duration_since(start)
}

// Adapter-local wire validation until the coordinator registers the public model
// dependency. Only this adapter consumes the independent session progress contract.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProgressSnapshot {
    version: u32,
    session_id: String,
    observation: ProgressObservation,
}

#[derive(serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum ProgressObservation {
    Available {
        epoch: String,
        counter: u64,
        last_progress_at_ms: Option<u64>,
    },
    Unavailable {
        reason: String,
    },
}

fn progress_mark(
    value: serde_json::Value,
    session: &str,
) -> Result<ProgressMark<String>, EvalError> {
    let snapshot: ProgressSnapshot = serde_json::from_value(value)?;
    if snapshot.version != 2 || snapshot.session_id != session {
        return Err(EvalError::Validation(
            "unsupported or mismatched session progress snapshot".into(),
        ));
    }
    match snapshot.observation {
        ProgressObservation::Available {
            epoch,
            counter,
            last_progress_at_ms,
        } if !epoch.is_empty() => {
            let _ = last_progress_at_ms;
            Ok(ProgressMark {
                epoch,
                count: counter,
            })
        }
        ProgressObservation::Unavailable { reason } => {
            // Do not persist untrusted reason text as a public diagnostic.
            let _ = reason;
            Err(EvalError::Validation(
                "session progress coverage unavailable; idle timeout cannot be established".into(),
            ))
        }
        ProgressObservation::Available { .. } => Err(EvalError::Validation(
            "session progress epoch missing".into(),
        )),
    }
}

fn clock_error(error: super::idle_timeout::ClockError) -> EvalError {
    EvalError::Validation(format!("execution activity observation failed: {error:?}"))
}

/// One independently bounded recursive scan through public session contracts.
/// No inspection timestamps, status polls or event-sequence hashes count as progress.
#[cfg(test)]
fn scan_progress(
    root: &str,
    clock: &mut ExecutionClock<String>,
    now: &impl Fn() -> Duration,
    remaining: &impl Fn() -> Duration,
    invoke: &impl Fn(&[String], &str, Duration) -> Result<serde_json::Value, EvalError>,
    max_sessions: std::num::NonZeroUsize,
) -> Result<(), EvalError> {
    use bcode_session_models::{
        SessionActivityCoverage, SessionActivityPage, SessionActivityState, SessionChildPage,
    };
    let mut pending = std::collections::VecDeque::from([root.to_owned()]);
    let mut visited = std::collections::BTreeSet::new();
    while let Some(session) = pending.pop_front() {
        if !visited.insert(session.clone()) {
            return Err(EvalError::Validation(
                "ambiguous or cyclic session subtree discovery".into(),
            ));
        }
        if visited.len() > max_sessions.get() {
            return Err(EvalError::Validation(
                "session progress scan budget exhausted; idle coverage incomplete".into(),
            ));
        }
        let read = |args: &[String], label: &str| {
            let allowance = remaining().min(Duration::from_secs(3));
            if allowance.is_zero() {
                return Err(EvalError::Validation(
                    "session progress scan allowance exhausted".into(),
                ));
            }
            invoke(args, label, allowance)
        };
        let activity: SessionActivityPage = serde_json::from_value(read(
            &[
                "session".into(),
                "activity".into(),
                session.clone(),
                "--json".into(),
            ],
            "idle-activity",
        )?)?;
        if activity.own.session_id.to_string() != session
            || activity.coverage != SessionActivityCoverage::Complete
            || !activity.issues.is_empty()
            || activity.next_after.is_some()
            || activity.own.states.is_empty()
            || activity.own.states.contains(&SessionActivityState::Unknown)
            || !activity
                .own
                .evidence
                .as_ref()
                .is_some_and(|evidence| evidence.counts_complete)
        {
            return Err(EvalError::Validation(
                "session activity coverage incomplete; idle timeout cannot be established".into(),
            ));
        }
        // A verified own-session idle state needs no live marker. This is not a
        // claim that descendants are settled: discovery always traverses them.
        let settled = activity.own.states.iter().all(|state| {
            matches!(
                state,
                SessionActivityState::Idle
                    | SessionActivityState::Failed
                    | SessionActivityState::Interrupted
            )
        }) && activity.own.evidence.as_ref().is_some_and(|evidence| {
            evidence.running == 0
                && evidence.queued == 0
                && evidence.permission_waits == 0
                && evidence.input_waits == 0
        });
        if settled {
            clock.retire(&session);
        } else {
            let value = read(
                &[
                    "session".into(),
                    "progress".into(),
                    session.clone(),
                    "--json".into(),
                ],
                "idle-progress",
            )?;
            let mark = progress_mark(value, &session)?;
            if clock.contains(&session) {
                clock.observe(now(), &session, mark).map_err(clock_error)?;
            } else {
                clock.baseline(session.clone(), mark).map_err(clock_error)?;
            }
        }
        let mut after = None::<String>;
        loop {
            let mut args = vec![
                "session".into(),
                "children".into(),
                session.clone(),
                "--limit".into(),
                "100".into(),
                "--json".into(),
            ];
            if let Some(cursor) = &after {
                args.extend(["--after".into(), cursor.clone()]);
            }
            let page: SessionChildPage = serde_json::from_value(read(&args, "idle-children")?)?;
            if !page.complete || !page.issues.is_empty() {
                return Err(EvalError::Validation(
                    "session child discovery degraded; idle timeout cannot be established".into(),
                ));
            }
            for child in page.children {
                if child.parent_id.map(|id| id.to_string()).as_deref() != Some(session.as_str()) {
                    return Err(EvalError::Validation(
                        "session child parent mismatch".into(),
                    ));
                }
                if visited.len() + pending.len() >= max_sessions.get() {
                    return Err(EvalError::Validation(
                        "session progress scan budget exhausted; idle coverage incomplete".into(),
                    ));
                }
                pending.push_back(child.id.to_string());
            }
            let Some(next) = page.next_after.map(|id| id.to_string()) else {
                break;
            };
            if after.as_ref().is_some_and(|previous| next <= *previous) {
                return Err(EvalError::Validation(
                    "session child cursor did not advance".into(),
                ));
            }
            after = Some(next);
        }
    }
    Ok(())
}

use bcode_workflow_view_models::{
    AttemptObservation, WORKFLOW_ACTIVITY_VERSION, WorkflowActivityPage, WorkflowActivitySection,
    WorkflowStageProgress,
};

type ActivityInvoke<'a> =
    dyn Fn(&[String], &str, Duration) -> Result<serde_json::Value, EvalError> + 'a;

struct ActivityScanGuard<'a> {
    id: Option<String>,
    invoke: &'a ActivityInvoke<'a>,
    ipc_allowance: Duration,
}

impl ActivityScanGuard<'_> {
    fn close(&mut self) -> Result<(), EvalError> {
        if let Some(id) = self.id.take() {
            (self.invoke)(
                &[
                    "workflow".into(),
                    "activity-close".into(),
                    "--scan-id".into(),
                    id,
                ],
                "activity-scan-close",
                self.ipc_allowance,
            )?;
        }
        Ok(())
    }
}

impl Drop for ActivityScanGuard<'_> {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn observe_stage(
    stage: &WorkflowStageProgress,
    dispatch: &str,
    clock: &mut ExecutionClock<String>,
    now: &impl Fn() -> Duration,
    active: &mut std::collections::BTreeSet<String>,
) -> Result<(), EvalError> {
    if stage.source_id != dispatch || stage.epoch.is_empty() || !(1..=3).contains(&stage.sequence) {
        return Err(EvalError::Validation(
            "workflow stage identity or transition invalid".into(),
        ));
    }
    let source = format!("workflow-stage:{}", stage.source_id);
    if !clock.contains(&source) {
        let host_now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| EvalError::Validation("activity host time unavailable".into()))?;
        let age = host_now
            .checked_sub(Duration::from_millis(stage.last_progress_at_ms))
            .ok_or_else(|| EvalError::Validation("activity timestamp is in the future".into()))?;
        let observed = now();
        if let Some(event_elapsed) = observed.checked_sub(age) {
            clock
                .substantive_at(observed, event_elapsed)
                .map_err(clock_error)?;
        }
    }
    let mark = ProgressMark {
        epoch: stage.epoch.clone(),
        count: stage.sequence,
    };
    active.insert(source.clone());
    if clock.contains(&source) {
        clock.observe(now(), &source, mark).map_err(clock_error)?;
    } else {
        clock.baseline(source, mark).map_err(clock_error)?;
    }
    Ok(())
}

/// Traverse one retained workflow inventory; ordinary session trees are not coverage.
#[cfg(test)]
fn scan_workflow_progress(
    root: &str,
    clock: &mut ExecutionClock<String>,
    now: &impl Fn() -> Duration,
    remaining: &impl Fn() -> Duration,
    invoke: &impl Fn(&[String], &str, Duration) -> Result<serde_json::Value, EvalError>,
) -> Result<Duration, EvalError> {
    scan_workflow_progress_with_ipc(root, clock, now, remaining, invoke, Duration::from_secs(15))
}

fn scan_workflow_progress_with_ipc(
    root: &str,
    clock: &mut ExecutionClock<String>,
    now: &impl Fn() -> Duration,
    remaining: &impl Fn() -> Duration,
    invoke: &impl Fn(&[String], &str, Duration) -> Result<serde_json::Value, EvalError>,
    ipc_allowance: Duration,
) -> Result<Duration, EvalError> {
    let scan_started = now();
    let mut guard = ActivityScanGuard {
        id: None,
        invoke,
        ipc_allowance: remaining().min(ipc_allowance),
    };
    let result = (|| {
        let mut pending = std::collections::VecDeque::from([root.to_owned()]);
        let mut visited = std::collections::BTreeSet::new();
        let mut active = std::collections::BTreeSet::new();
        let mut snapshot_time = None;
        let mut aggregate_complete = false;
        let mut readiness_pending = false;
        let mut required_children = std::collections::BTreeSet::<(String, String)>::new();
        let mut verified_children = std::collections::BTreeSet::<(String, String)>::new();
        while let Some(run) = pending.pop_front() {
            if !visited.insert(run.clone()) {
                return Err(EvalError::Validation(
                    "ambiguous workflow activity lineage".into(),
                ));
            }
            let mut revision = None::<u64>;
            for section in ["attempts", "children"] {
                let mut cursor = None::<String>;
                let mut cursors = std::collections::BTreeSet::new();
                loop {
                    let allowance = remaining().min(ipc_allowance);
                    if allowance.is_zero() {
                        return Err(EvalError::Validation(
                            "workflow activity scan allowance exhausted".into(),
                        ));
                    }
                    let mut args = vec![
                        "workflow".into(),
                        "activity-run".into(),
                        "--run-id".into(),
                        run.clone(),
                        "--root-run-id".into(),
                        root.into(),
                        "--section".into(),
                        section.into(),
                        "--limit".into(),
                        "100".into(),
                        "--json".into(),
                    ];
                    if let Some(id) = &guard.id {
                        args.extend(["--scan-id".into(), id.clone()]);
                    }
                    if let Some(value) = revision {
                        args.extend(["--revision".into(), value.to_string()]);
                    }
                    if let Some(value) = &cursor {
                        args.extend(["--after".into(), value.clone()]);
                    }
                    let value = invoke(&args, "workflow-activity", allowance)?;
                    // Retain the cleanup identity even if the remainder is malformed.
                    if guard.id.is_none() {
                        guard.id = value
                            .get("scan_id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned);
                    }
                    let page: WorkflowActivityPage = serde_json::from_value(value)?;
                    if page.version != WORKFLOW_ACTIVITY_VERSION || page.run_id != run || page.root_run_id != root
                        || guard.id.as_deref() != Some(page.scan_id.as_str()) || page.scan_id.is_empty()
                        || page.section != if section == "attempts" { WorkflowActivitySection::Attempts } else { WorkflowActivitySection::Children } || revision.is_some_and(|value| value != page.revision)
                        || page.scan_lifetime_ms == 0 || page.snapshot_age_ms >= page.scan_lifetime_ms
                        || snapshot_time.is_some_and(|value| value != page.snapshot_started_at_ms)
                        || page.inventory_exhausted != page.next_cursor.is_none()
                        || page.issues.iter().any(|issue| issue != "Complete page inventory is as-of the retained snapshot; live progress is observed later and current subtree idleness is not certified")
                        || (section == "attempts" && !page.children.is_empty())
                        || (section == "children" && !page.attempts.is_empty())
                    {
                        return Err(EvalError::Validation("workflow activity snapshot coverage inconsistent or unavailable".into()));
                    }
                    revision = Some(page.revision);
                    snapshot_time = Some(page.snapshot_started_at_ms);
                    // This is an as-of inventory, not an atomic present-time claim.
                    let _ = page.current_idle_eligible;
                    // Individual pages cannot certify recursive coverage. Exhaust every
                    // section and child within the same retained scan instead.
                    aggregate_complete = page.subtree_coverage_complete;
                    for attempt in page.attempts {
                        if attempt.dispatch_identity.is_empty()
                            || attempt.node_id.is_empty()
                            || attempt.activation_id.is_empty()
                        {
                            return Err(EvalError::Validation(
                                "workflow activity attempt identity invalid".into(),
                            ));
                        }
                        let _ = attempt.attempt;
                        let progress = match attempt.observation {
                            AttemptObservation::PendingReadiness { stage, .. } => {
                                observe_stage(
                                    &stage,
                                    &attempt.dispatch_identity,
                                    clock,
                                    now,
                                    &mut active,
                                )?;
                                readiness_pending = true;
                                continue;
                            }
                            AttemptObservation::RuntimeStage { progress } => {
                                observe_stage(
                                    &progress,
                                    &attempt.dispatch_identity,
                                    clock,
                                    now,
                                    &mut active,
                                )?;
                                continue;
                            }
                            AttemptObservation::ChildWorkflow {
                                child_run_id,
                                stage,
                            } => {
                                if child_run_id.is_empty() {
                                    return Err(EvalError::Validation(
                                        "workflow child observation identity invalid".into(),
                                    ));
                                }
                                required_children.insert((run.clone(), child_run_id));
                                observe_stage(
                                    &stage,
                                    &attempt.dispatch_identity,
                                    clock,
                                    now,
                                    &mut active,
                                )?;
                                continue;
                            }
                            AttemptObservation::Terminal { stage } => {
                                if !attempt.terminal {
                                    return Err(EvalError::Validation(
                                        "workflow terminal observation mismatch".into(),
                                    ));
                                }
                                observe_stage(
                                    &stage,
                                    &attempt.dispatch_identity,
                                    clock,
                                    now,
                                    &mut active,
                                )?;
                                continue;
                            }
                            AttemptObservation::Unavailable { .. } => {
                                return Err(EvalError::Validation(
                                    "workflow attempt source unavailable".into(),
                                ));
                            }
                            AttemptObservation::LinkedSession { progress, stage } => {
                                observe_stage(
                                    &stage,
                                    &attempt.dispatch_identity,
                                    clock,
                                    now,
                                    &mut active,
                                )?;
                                serde_json::to_value(progress)?
                            }
                        };
                        let session = attempt.session_id.ok_or_else(|| {
                            EvalError::Validation("active attempt session unavailable".into())
                        })?;
                        let timestamp = progress
                            .pointer("/observation/last_progress_at_ms")
                            .and_then(serde_json::Value::as_u64);
                        let mark = progress_mark(progress, &session)?;
                        if !clock.contains(&session) {
                            let event = timestamp.ok_or_else(|| {
                                EvalError::Validation(
                                    "first-seen activity freshness unavailable".into(),
                                )
                            })?;
                            let observed = now();
                            // Same-host Unix time is used only for event age; scheduling
                            // and expiration remain monotonic. Future host time fails closed.
                            let host_now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map_err(|_| {
                                    EvalError::Validation("activity host time unavailable".into())
                                })?;
                            let age = host_now
                                .checked_sub(Duration::from_millis(event))
                                .ok_or_else(|| {
                                    EvalError::Validation(
                                        "activity timestamp is in the future".into(),
                                    )
                                })?;
                            if let Some(event_elapsed) = observed.checked_sub(age) {
                                clock
                                    .substantive_at(observed, event_elapsed)
                                    .map_err(clock_error)?;
                            }
                        }
                        active.insert(session.clone());
                        if clock.contains(&session) {
                            clock.observe(now(), &session, mark).map_err(clock_error)?;
                        } else {
                            clock.baseline(session, mark).map_err(clock_error)?;
                        }
                    }
                    for child in page.children {
                        if child.parent_run_id != run || child.child_run_id.is_empty() {
                            return Err(EvalError::Validation(
                                "workflow activity child lineage mismatch".into(),
                            ));
                        }
                        verified_children.insert((run.clone(), child.child_run_id.clone()));
                        pending.push_back(child.child_run_id);
                    }
                    let Some(next) = page.next_cursor else {
                        break;
                    };
                    if next.is_empty() || !cursors.insert(next.clone()) {
                        return Err(EvalError::Validation(
                            "workflow activity cursor repeated".into(),
                        ));
                    }
                    cursor = Some(next);
                }
            }
        }
        if !required_children.is_subset(&verified_children) {
            return Err(EvalError::Validation(
                "workflow child observation lacks verified inventory link".into(),
            ));
        }
        if !aggregate_complete {
            return Err(EvalError::Validation(
                "workflow activity aggregate inventory coverage incomplete".into(),
            ));
        }
        clock.retain_active(&active);
        if readiness_pending {
            return Err(EvalError::Validation(
                "workflow_activity_pending_readiness".into(),
            ));
        }
        Ok(scan_started)
    })();
    guard.ipc_allowance = remaining().min(ipc_allowance);
    let closed = guard.close();
    match (result, closed) {
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(started), Ok(())) => Ok(started),
    }
}

pub fn activity_scan_allowance(
    metadata: &std::collections::BTreeMap<String, serde_json::Value>,
) -> Result<Duration, EvalError> {
    let milliseconds = match metadata.get("activity_scan_allowance_ms") {
        None => 15_000,
        Some(value) => value.as_u64().filter(|value| *value > 0).ok_or_else(|| {
            EvalError::Validation("activity_scan_allowance_ms must be a positive integer".into())
        })?,
    };
    Ok(Duration::from_millis(milliseconds))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CliReadDiagnostic {
    schema_version: u32,
    code: String,
    phase: String,
    message: String,
}

fn readonly_connection_timeout(stderr: &[u8]) -> bool {
    stderr.len() <= 4096
        && serde_json::from_slice::<CliReadDiagnostic>(stderr)
            .ok()
            .is_some_and(|error| {
                error.schema_version == 1
                    && error.code == "connection_timeout"
                    && error.phase == "connect"
                    && error.message == "Verified daemon connection timed out."
            })
}

fn readonly_operation(args: &[String]) -> Option<&'static str> {
    match (
        args.first().map(String::as_str),
        args.get(1).map(String::as_str),
    ) {
        (Some("workflow"), Some("inspect-run")) => Some("workflow_inspect"),
        (Some("workflow"), Some("activity-run")) => Some("workflow_activity"),
        (Some("workflow"), Some("activity-close")) => Some("workflow_activity_close"),
        (Some("session"), Some("activity")) => Some("session_activity"),
        (Some("session"), Some("progress")) => Some("session_progress"),
        (Some("session"), Some("children")) => Some("session_children"),
        (Some("session"), Some("history")) => Some("session_history"),
        _ => None,
    }
}

fn invocation_command(binary: &std::path::Path, args: &[String]) -> Command {
    let mut command = Command::new(binary);
    if readonly_operation(args).is_some() {
        command.arg("--json-errors");
    }
    command.args(args);
    command
}

fn operational_allowance(
    metadata: &std::collections::BTreeMap<String, serde_json::Value>,
    key: &str,
) -> Result<Duration, EvalError> {
    let milliseconds = match metadata.get(key) {
        None => 15_000,
        Some(value) => value
            .as_u64()
            .filter(|value| *value > 0)
            .ok_or_else(|| EvalError::Validation(format!("{key} must be a positive integer")))?,
    };
    Ok(Duration::from_millis(milliseconds))
}

fn inspection_allowance_with_ipc(
    idle_mode: bool,
    wall_remaining: Option<Duration>,
    ipc: Duration,
) -> Duration {
    if idle_mode {
        wall_remaining.unwrap_or(ipc).min(ipc)
    } else {
        wall_remaining.unwrap_or(ipc)
    }
}

#[cfg(test)]
fn inspection_allowance(idle_mode: bool, wall_remaining: Option<Duration>) -> Duration {
    inspection_allowance_with_ipc(idle_mode, wall_remaining, Duration::from_secs(15))
}

fn readiness_unavailable(error: &EvalError) -> bool {
    matches!(error, EvalError::Client(message) if message.contains("server returned error workflow_capability_unavailable: workflow readiness status is temporarily unavailable; retry the workflow request (no initialization progress has been verified)"))
}

fn observe(
    args: &[String],
    label: &str,
    remaining: impl Fn() -> Duration,
    invoke: &impl Fn(&[String], &str, Duration) -> Result<serde_json::Value, EvalError>,
    wait: impl Fn(Duration),
) -> Result<serde_json::Value, EvalError> {
    // Retry reads only. Admission, cancellation and other mutations never retry here.
    let mut retries = 0;
    loop {
        match invoke(args, label, remaining()) {
            Err(error)
                if readiness_unavailable(&error) && retries < 4 && !remaining().is_zero() =>
            {
                retries += 1;
                wait(Duration::from_millis(100).min(remaining()));
            }
            result => return result,
        }
    }
}

fn verify_cancellation(
    run_id: &str,
    invoke: &impl Fn(&[String], &str, Duration) -> Result<serde_json::Value, EvalError>,
) -> Option<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let args = [
            "workflow".into(),
            "inspect-run".into(),
            "--run-id".into(),
            run_id.into(),
            "--limit".into(),
            "1".into(),
        ];
        if let Ok(value) = invoke(
            &args,
            "cancel-verification",
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(3)),
        ) && terminal_status(&value)
        {
            return Some(value);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

struct IsolatedDaemonCleanup<'a> {
    binary: &'a Path,
    directory: &'a Path,
    enabled: bool,
    environment: std::collections::BTreeMap<String, String>,
    workspace: &'a Path,
}

impl Drop for IsolatedDaemonCleanup<'_> {
    fn drop(&mut self) {
        if !self.enabled {
            return;
        }
        let mut command = Command::new(self.binary);
        command
            .args(["server", "stop-if-idle"])
            .envs(&self.environment)
            .current_dir(self.workspace);
        let result = process::bounded_command(
            command,
            Duration::from_secs(10),
            self.directory,
            "daemon-cleanup",
        );
        let evidence = match result {
            Ok((status, timed_out, _, _)) => serde_json::json!({
                "requested": status == Some(0) && !timed_out,
                "timed_out": timed_out,
                "exit_code": status,
                "reason": if timed_out { "cleanup_deadline_elapsed" } else if status == Some(0) { "idle_stop_request_accepted" } else { "idle_stop_request_rejected_or_unavailable" },
                "scope": "exact isolated artifact/state; graceful idle-only stop; not proof of effect settlement",
            }),
            Err(_) => serde_json::json!({"requested":false,"reason":"cleanup_request_unavailable"}),
        };
        let _ = write_json(self.directory.join("daemon-cleanup.json"), &evidence);
    }
}

pub fn execute(
    suite: &EvalSuite,
    case: &EvalCase,
    variant: &EvalVariant,
    rep_dir: &Path,
    workspace: &Path,
) -> Result<ExecutionOutput, EvalError> {
    let mut config: WorkflowEvalConfig =
        serde_json::from_value(variant.metadata.get("workflow").cloned().ok_or_else(|| {
            EvalError::Validation("workflow executor requires metadata.workflow".into())
        })?)?;
    if let Some(oracle) = case.metadata.get("workflow_oracle") {
        if let Some(value) = oracle.get("expected_contains") {
            config.expected_contains = serde_json::from_value(value.clone())?;
        }
        if let Some(value) = oracle.get("answer_file") {
            config.answer_file = serde_json::from_value(value.clone())?;
        }
        if let Some(value) = oracle.get("expected_json") {
            config.expected_json = Some(value.clone());
        }
        if let Some(value) = oracle.get("oracle_command") {
            config.oracle_command = serde_json::from_value(value.clone())?;
        }
        if let Some(value) = oracle.get("expect_rejection") {
            config.expect_rejection = serde_json::from_value(value.clone())?;
        }
    }
    if let Some(command) = &mut config.oracle_command {
        let suite_dir = suite
            .metadata
            .get("suite_dir")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                EvalError::Validation("oracle requires resolved suite directory".into())
            })?;
        *command = command.replace("{{suite_dir}}", suite_dir);
    }
    if !config.binary.is_absolute()
        || !config.binary.is_file()
        || !["auto", "collaborate", "no-delegation"].contains(&config.strategy.as_str())
        || config.max_iterations == 0
        || config.concurrency == 0
    {
        return Err(EvalError::Validation(
            "invalid workflow executable, strategy or allowances".into(),
        ));
    }
    if let Some(profile) = &variant.profile {
        return Err(EvalError::Validation(format!(
            "workflow profile {profile} requires an explicit config_toml overlay; profile metadata is not applied by the external CLI"
        )));
    }
    let _environment = prepare_agent_daemon_isolation(variant, rep_dir)?;
    let _daemon_cleanup = IsolatedDaemonCleanup {
        binary: &config.binary,
        directory: rep_dir,
        enabled: super::agent_daemon_isolation_mode(variant) != "shared",
        environment: variant
            .env
            .iter()
            .chain(&case.env)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        workspace,
    };
    let preserved: Vec<_> = config
        .preserve_files
        .iter()
        .map(|path| {
            if path.is_absolute()
                || path
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                return Err(EvalError::Validation(
                    "preservation paths must be workspace-relative".into(),
                ));
            }
            Ok((path.clone(), fs::read(workspace.join(path))?))
        })
        .collect::<Result<_, EvalError>>()?;
    let mut hasher = sha2::Sha256::new();
    {
        use std::io::Read;
        let mut file = fs::File::open(&config.binary)?;
        let mut buffer = vec![0_u8; 65_536];
        loop {
            let length = file.read(&mut buffer)?;
            if length == 0 {
                break;
            }
            hasher.update(&buffer[..length]);
        }
    }
    let digest = hex::encode(hasher.finalize());
    write_json(
        rep_dir.join("subject.json"),
        &serde_json::json!({"schema_version":1,"binary":config.binary,"sha256":digest,"strategy":config.strategy,"judge":config.judge,"acceptance":config.stop_condition}),
    )?;
    let timeout_policy = suite.run.resolved_timeout(case);
    if timeout_policy.timeout_ms == 0 || timeout_policy.max_duration_ms == Some(0) {
        return Err(EvalError::Validation(
            "execution timeout durations must be positive".into(),
        ));
    }
    write_json(rep_dir.join("timeout-policy.json"), &timeout_policy)?;
    let idle_mode = timeout_policy.timeout_mode == EvalTimeoutMode::Idle;
    let session_budget = std::num::NonZeroUsize::new(
        suite
            .metadata
            .get("activity_scan_session_budget")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(4096),
    )
    .ok_or_else(|| EvalError::Validation("activity scan session budget must be positive".into()))?;
    let scan_allowance = activity_scan_allowance(&suite.metadata)?;
    let activity_ipc =
        operational_allowance(&suite.metadata, "workflow_activity_ipc_allowance_ms")?;
    let inspection_ipc =
        operational_allowance(&suite.metadata, "workflow_inspection_ipc_allowance_ms")?;
    write_json(
        rep_dir.join("workflow-operational-policy.json"),
        &serde_json::json!({
            "workflow_activity_ipc_allowance_ms": activity_ipc.as_millis(),
            "workflow_inspection_ipc_allowance_ms": inspection_ipc.as_millis(),
            "activity_scan_allowance_ms": scan_allowance.as_millis()
        }),
    )?;
    let scan_allowance_ms = scan_allowance.as_millis();
    let setup_start = Instant::now();
    // Idle interval is not a setup or admission wall limit.
    let setup_allowance = if idle_mode {
        Duration::from_mins(2)
    } else {
        Duration::from_millis(
            timeout_policy
                .timeout_ms
                .min(timeout_policy.max_duration_ms.unwrap_or(u64::MAX)),
        )
    };
    let deadline = setup_start + setup_allowance;
    let invoke =
        |args: &[String], label: &str, timeout: Duration| -> Result<serde_json::Value, EvalError> {
            let operation = readonly_operation(args);
            let started = Instant::now();
            let mut retries = 0_u32;
            loop {
                let remaining = timeout.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Err(EvalError::Client(format!(
                        "{label}: read operation allowance exhausted"
                    )));
                }
                let mut command = invocation_command(&config.binary, args);
                command
                    .current_dir(workspace)
                    .envs(&variant.env)
                    .envs(&case.env);
                let (code, expired, out, err) =
                    process::bounded_command(command, remaining, rep_dir, label)?;
                if !expired
                    && code != Some(0)
                    && operation.is_some()
                    && readonly_connection_timeout(&err)
                    && started.elapsed() < timeout
                {
                    retries = retries.saturating_add(1);
                    write_json(
                        rep_dir.join(format!("{label}-readonly-connect-retry-{retries}.json")),
                        &serde_json::json!({"schema_version":1,"operation":operation,
                        "code":"connection_timeout","phase":"connect","attempt":retries,
                        "elapsed_ms":started.elapsed().as_millis()}),
                    )?;
                    std::thread::sleep(
                        Duration::from_millis(100).min(timeout.saturating_sub(started.elapsed())),
                    );
                    continue;
                }
                if expired || code != Some(0) {
                    return Err(EvalError::Client(format!(
                        "{label} failed (exit={code:?}, timeout={expired}): {}",
                        String::from_utf8_lossy(&err)
                    )));
                }
                return Ok(serde_json::from_slice(&out)?);
            }
        };
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let session = invoke(
        &[
            "session".into(),
            "create".into(),
            "--cwd".into(),
            workspace.display().to_string(),
            "--json".into(),
            format!("eval:{}:{}", case.id, variant.id),
        ],
        "session",
        remaining(),
    )?;
    let session_id = session["id"]
        .as_str()
        .ok_or_else(|| EvalError::Validation("session response missing identity".into()))?;
    if let Some(model) = &variant.model {
        invoke(
            &[
                "session".into(),
                "set-model".into(),
                session_id.into(),
                model.clone(),
                "--json".into(),
            ],
            "model-selection",
            remaining(),
        )?;
    }
    let mut args = vec![
        "goal".into(),
        "--session".into(),
        session_id.into(),
        "--objective".into(),
        rendered_prompt(case, variant),
        "--stop-condition".into(),
        config.stop_condition.clone(),
        "--agents".into(),
        config.strategy.clone(),
        "--max-iterations".into(),
        config.max_iterations.to_string(),
        "--concurrency".into(),
        config.concurrency.to_string(),
        "--worker-attempts".into(),
        config.worker_attempts.to_string(),
        "--swarm-instructions".into(),
        config.guidance.clone(),
    ];
    if config.generated_entry {
        let position = args
            .iter()
            .position(|value| value == "--stop-condition")
            .expect("authored condition argument");
        args.drain(position..position + 2);
    }
    if let Some(judge) = &config.judge {
        args.extend([
            "--judge-provider".into(),
            judge.provider.clone(),
            "--judge-model".into(),
            judge.model.clone(),
            "--judge-account".into(),
            judge.account.clone(),
            "--judge-threshold".into(),
            judge.threshold.to_string(),
            "--judge-failure".into(),
            "pause".into(),
        ]);
    }
    let start = Instant::now();
    let admitted = invoke(&args, "admission", remaining())?;
    let run_id = admitted["run_id"]
        .as_str()
        .ok_or_else(|| EvalError::Validation("admission missing run identity".into()))?;
    write_json(rep_dir.join("admission.json"), &admitted)?;
    let execution_origin = if idle_mode {
        Instant::now()
    } else {
        setup_start
    };
    let interval = Duration::from_millis(timeout_policy.timeout_ms);
    let wall_limit = if idle_mode {
        timeout_policy.max_duration_ms.map(Duration::from_millis)
    } else {
        Some(Duration::from_millis(
            timeout_policy
                .timeout_ms
                .min(timeout_policy.max_duration_ms.unwrap_or(u64::MAX)),
        ))
    };
    let mut execution_clock = ExecutionClock::new(
        Duration::ZERO,
        idle_mode.then_some(interval),
        wall_limit,
        session_budget,
    );
    let now = || execution_origin.elapsed();
    let mut last_activity_as_of = None;
    let mut timeout_reason = None;
    let mut timed_out = false;
    let mut activity_observation_failed = false;
    let mut capture_failed = false;
    let mut last_observed = None;
    let mut inspection;
    loop {
        // Before a fresh subtree scan only an explicit wall limit can expire.
        // Reaching the previous idle boundary triggers observation, not a verdict.
        if wall_limit.is_some_and(|limit| now() >= limit) {
            timed_out = true;
            timeout_reason = Some(TimeoutReason::Wall);
            break;
        }
        let inspection_remaining = || {
            inspection_allowance_with_ipc(
                idle_mode,
                wall_limit.map(|limit| limit.saturating_sub(now())),
                inspection_ipc,
            )
        };
        let inspection_args = vec![
            "workflow".into(),
            "inspect-run".into(),
            "--run-id".into(),
            run_id.into(),
            "--limit".into(),
            "100".into(),
        ];
        inspection = match observe(
            &inspection_args,
            "inspection",
            inspection_remaining,
            &invoke,
            std::thread::sleep,
        ) {
            Ok(value) => value,
            Err(error) if wall_limit.is_some_and(|limit| now() >= limit) => {
                timed_out = true;
                timeout_reason = Some(TimeoutReason::Wall);
                write_json(rep_dir.join("observation-error.json"), &error.to_string())?;
                break;
            }
            Err(error) => {
                write_json(rep_dir.join("observation-error.json"), &error.to_string())?;
                let cancellation = invoke(
                    &[
                        "workflow".into(),
                        "cancel-run".into(),
                        "--run-id".into(),
                        run_id.into(),
                    ],
                    "observation-cancellation",
                    Duration::from_secs(15),
                );
                let terminal = verify_cancellation(run_id, &invoke);
                let verified = terminal.is_some();
                if terminal.is_some() {
                    last_observed = terminal;
                }
                write_json(
                    rep_dir.join("cancellation.json"),
                    &serde_json::json!({"requested":cancellation.is_ok(),"error":cancellation.err().map(|error|error.to_string()),"terminal_verified":verified,"reason":"observation_failed"}),
                )?;
                capture_failed = true;
                break;
            }
        };
        last_observed = Some(inspection.clone());
        write_json(rep_dir.join("inspection.json"), &inspection)?;
        let status = inspection
            .pointer("/run/status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if ["completed", "failed", "cancelled"].contains(&status) {
            break;
        }
        if status == "repair_required" {
            let cancellation = invoke(
                &[
                    "workflow".into(),
                    "cancel-run".into(),
                    "--run-id".into(),
                    run_id.into(),
                ],
                "repair-cancellation",
                Duration::from_secs(15),
            );
            let terminal = verify_cancellation(run_id, &invoke);
            let verified = terminal.is_some();
            if terminal.is_some() {
                last_observed = terminal;
            }
            write_json(
                rep_dir.join("cancellation.json"),
                &serde_json::json!({
                    "requested":cancellation.is_ok(), "terminal_verified":verified,
                    "reason":"repair_required", "error":cancellation.err().map(|error|error.to_string())
                }),
            )?;
            capture_failed = true;
            break;
        }
        let activity_result = if idle_mode {
            let readiness_allowance = match case.metadata.get("activity_readiness_allowance_ms") {
                None => Duration::from_secs(30),
                Some(value) => Duration::from_millis(
                    value.as_u64().filter(|value| *value > 0).ok_or_else(|| {
                        EvalError::Validation(
                            "activity_readiness_allowance_ms must be positive".into(),
                        )
                    })?,
                ),
            };
            let readiness_started = Instant::now();
            loop {
                let scan_deadline =
                    Instant::now().checked_add(scan_allowance).ok_or_else(|| {
                        EvalError::Validation(
                            "activity scan allowance exceeds monotonic clock range".into(),
                        )
                    })?;
                let result = scan_workflow_progress_with_ipc(
                    run_id,
                    &mut execution_clock,
                    &now,
                    &|| {
                        scan_deadline.saturating_duration_since(Instant::now()).min(
                            wall_limit.map_or(Duration::MAX, |limit| limit.saturating_sub(now())),
                        )
                    },
                    &invoke,
                    activity_ipc,
                );
                if matches!(&result, Err(EvalError::Validation(reason)) if reason == "workflow_activity_pending_readiness")
                    && readiness_started.elapsed() < readiness_allowance
                    && wall_limit.is_none_or(|limit| now() < limit)
                {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                break result;
            }
        } else {
            Ok(now())
        };
        let activity_as_of = activity_result.as_ref().ok().copied();
        last_activity_as_of = activity_as_of;
        if let Err(error) = activity_result {
            if wall_limit.is_some_and(|limit| now() >= limit) {
                timed_out = true;
                timeout_reason = Some(TimeoutReason::Wall);
            } else {
                activity_observation_failed = true;
                write_json(
                    rep_dir.join("activity-observation-error.json"),
                    &error.to_string(),
                )?;
                let cancellation = invoke(
                    &[
                        "workflow".into(),
                        "cancel-run".into(),
                        "--run-id".into(),
                        run_id.into(),
                    ],
                    "activity-observation-cancellation",
                    Duration::from_secs(15),
                );
                let terminal = verify_cancellation(run_id, &invoke);
                let verified = terminal.is_some();
                if terminal.is_some() {
                    last_observed = terminal;
                }
                write_json(
                    rep_dir.join("cancellation.json"),
                    &serde_json::json!({
                        "requested": cancellation.is_ok(), "terminal_verified": verified,
                        "reason": "activity_observation_failed", "error": cancellation.err().map(|error| error.to_string())
                    }),
                )?;
                capture_failed = true;
            }
            break;
        }
        let observed_now = now();
        if let Some(reason) = execution_clock
            .expired_as_of(observed_now, activity_as_of.unwrap_or(observed_now))
            .map_err(clock_error)?
        {
            timed_out = true;
            timeout_reason = Some(reason);
            break;
        }
        let sleep = Duration::from_millis(250);
        std::thread::sleep(sleep);
    }
    let execution_elapsed = execution_duration(start, Instant::now());
    if timed_out {
        let cancel = invoke(
            &[
                "workflow".into(),
                "cancel-run".into(),
                "--run-id".into(),
                run_id.into(),
            ],
            "cancellation",
            Duration::from_secs(15),
        );
        let requested = cancel.is_ok();
        let terminal = verify_cancellation(run_id, &invoke);
        let terminal_verified = terminal.is_some();
        if terminal.is_some() {
            last_observed = terminal;
        }
        write_json(
            rep_dir.join("cancellation.json"),
            &serde_json::json!({"requested":requested,"error":cancel.err().map(|e|e.to_string()),"terminal_verified":terminal_verified}),
        )?;
    }
    let timeout_reason_name = timeout_reason.map(|reason| match reason {
        TimeoutReason::Idle => "idle",
        TimeoutReason::Wall => "wall",
    });
    write_json(
        rep_dir.join("timeout-outcome.json"),
        &serde_json::json!({
            "policy": timeout_policy, "timed_out": timed_out, "reason": timeout_reason_name,
            "activity_scan_start_elapsed_ms": last_activity_as_of.map(|value| value.as_millis()),
            "activity_scan_age_ms": last_activity_as_of.map(|value| now().saturating_sub(value).as_millis()),
            "idle_observation_semantics": "as_of_scan_start_not_atomic_current",
            "activity_observation_failed": activity_observation_failed,
            "setup_allowance_ms": setup_allowance.as_millis(),
            "activity_scan_allowance_ms": scan_allowance_ms, "operation_allowance_ms": 3000,
            "activity_scan_session_budget": session_budget.get()
        }),
    )?;
    // Post-execution capture has a separate bounded allowance. Exhaustion degrades
    // evidence, never extends execution or changes an observed terminal outcome.
    let capture_allowance = Duration::from_millis(
        suite
            .metadata
            .get("capture_allowance_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(120_000),
    );
    let capture_deadline = Instant::now() + capture_allowance;
    let execution_invoke = &invoke;
    let invoke = |args: &[String], label: &str, timeout: Duration| {
        let allowance = capture_deadline.saturating_duration_since(Instant::now());
        if allowance.is_zero() {
            return Err(EvalError::Validation(
                "post-execution capture allowance exhausted".into(),
            ));
        }
        execution_invoke(args, label, timeout.min(allowance))
    };
    let outputs = match invoke(
        &[
            "workflow".into(),
            "run-output".into(),
            "--run-id".into(),
            run_id.into(),
            "--limit".into(),
            "100".into(),
        ],
        "outputs",
        Duration::from_secs(15),
    ) {
        Ok(value) => value,
        Err(error) => {
            capture_failed = true;
            write_json(
                rep_dir.join("outputs-capture-error.json"),
                &error.to_string(),
            )?;
            serde_json::json!([])
        }
    };
    write_json(rep_dir.join("workflow-output.json"), &outputs)?;
    let history = match invoke(
        &[
            "session".into(),
            "history".into(),
            session_id.into(),
            "--limit".into(),
            "100".into(),
            "--json".into(),
        ],
        "history",
        Duration::from_secs(15),
    ) {
        Ok(value) => value,
        Err(error) => {
            capture_failed = true;
            write_json(
                rep_dir.join("session-history-capture-error.json"),
                &error.to_string(),
            )?;
            serde_json::Value::Null
        }
    };
    write_json(rep_dir.join("session-history.json"), &history)?;
    let mut measurements = EvalMeasurementSet::new();
    measurements.insert(
        "workflow.wall_time_ms".into(),
        execution_elapsed.as_millis() as f64,
    );
    measurements.insert("workflow.timed_out".into(), f64::from(timed_out));
    measurements.insert(
        "workflow.timeout_idle".into(),
        f64::from(timeout_reason == Some(TimeoutReason::Idle)),
    );
    measurements.insert(
        "workflow.timeout_wall".into(),
        f64::from(timeout_reason == Some(TimeoutReason::Wall)),
    );
    measurements.insert(
        "workflow.timeout_interval_ms".into(),
        timeout_policy.timeout_ms as f64,
    );
    measurements.insert("workflow.timeout_mode_idle".into(), f64::from(idle_mode));
    if let Some(limit) = timeout_policy.max_duration_ms {
        measurements.insert("workflow.max_duration_ms".into(), limit as f64);
    }
    let final_inspection = match invoke(
        &[
            "workflow".into(),
            "inspect-run".into(),
            "--run-id".into(),
            run_id.into(),
            "--limit".into(),
            "100".into(),
        ],
        "final",
        Duration::from_secs(15),
    ) {
        Ok(value) => value,
        Err(error) => {
            capture_failed = true;
            write_json(
                rep_dir.join("final-inspection-error.json"),
                &error.to_string(),
            )?;
            last_observed.ok_or(error)?
        }
    };
    write_json(rep_dir.join("inspection.json"), &final_inspection)?;
    write_json(
        rep_dir.join("timing.json"),
        &super::timing::describe(&final_inspection),
    )?;
    if let Some(revision) = final_inspection
        .pointer("/graph/revision")
        .and_then(serde_json::Value::as_u64)
        && let Err(error) = super::capture::capture(run_id, revision, rep_dir, invoke)
    {
        capture_failed = true;
        write_json(rep_dir.join("graph-capture-error.json"), &error.to_string())?;
    }
    let capabilities: bcode_workflow::WorkflowRunInspection =
        serde_json::from_value(final_inspection.clone())?;
    if capabilities.run.run_id != run_id {
        return Err(EvalError::Validation(
            "capture inspection run identity mismatch".into(),
        ));
    }
    if let Err(error) = super::capture_workflow_history(&capabilities, rep_dir, invoke) {
        capture_failed = true;
        write_json(
            rep_dir.join("history-capture-error.json"),
            &error.to_string(),
        )?;
    }
    let mut diagnostic_receipts = Vec::new();
    for (index, attempt) in final_inspection["attempts"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let Some(identity) = attempt["dispatch_identity"].as_str() else {
            continue;
        };
        let result = super::workflow_diagnostics::capture(
            run_id,
            identity,
            &rep_dir.join(format!("diagnostics-{index:03}")),
            &invoke,
        );
        diagnostic_receipts.push(serde_json::json!({
            "dispatch_identity": identity,
            "status": if result.is_ok() { "retained_verified" } else { "unavailable_or_partial" },
            "error": result.err().map(|error| error.to_string()),
        }));
    }
    let mut event_sessions = vec![session_id.to_owned()];
    event_sessions.extend(
        final_inspection["child_sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|value| value["id"].as_str().map(str::to_owned)),
    );
    let mut capture_complete = !capture_failed
        && final_inspection["unavailable_collections"]
            .as_array()
            .is_none_or(Vec::is_empty)
        && final_inspection.pointer("/graph/nodes_complete")
            == Some(&serde_json::Value::Bool(true))
        && final_inspection.pointer("/graph/edges_complete")
            == Some(&serde_json::Value::Bool(true))
        && ["activations", "child_sessions", "events", "attempts"]
            .iter()
            .all(|key| {
                final_inspection[*key]
                    .as_array()
                    .is_some_and(|values| values.len() < 100)
            })
        && outputs.as_array().is_some_and(|values| values.len() < 100);
    let mut session_events = Vec::new();
    let mut correlation_events = Vec::new();
    let mut model_coverage = workflow_capture::ModelCoverage::default();
    let mut session_history_complete = true;
    for (index, identity) in event_sessions.iter().enumerate() {
        let mut after = 0_u64;
        let mut events = Vec::new();
        let mut history_complete = true;
        let mut first_page = (index == 0).then(|| history.clone());
        loop {
            let cached_prefix = first_page.is_some();
            let result = workflow_capture::first_or_fetch(&mut first_page, || {
                invoke(
                    &[
                        "session".into(),
                        "history".into(),
                        identity.clone(),
                        "--after".into(),
                        after.to_string(),
                        "--limit".into(),
                        "100".into(),
                        "--json".into(),
                    ],
                    &format!("history-{index}"),
                    Duration::from_secs(15),
                )
            });
            let page = match result {
                Ok(value) => value,
                Err(error) => {
                    capture_complete = false;
                    session_history_complete = false;
                    history_complete = false;
                    write_json(
                        rep_dir.join(format!("history-{index}-error.json")),
                        &error.to_string(),
                    )?;
                    break;
                }
            };
            let Some(page) = workflow_capture::history_page(&page, identity, after) else {
                capture_complete = false;
                session_history_complete = false;
                history_complete = false;
                break;
            };
            let next = workflow_capture::continuation(&page, cached_prefix);
            events.extend(page.events);
            let Some(next) = next else {
                break;
            };
            after = next;
            if events.len() >= 10000 {
                capture_complete = false;
                session_history_complete = false;
                history_complete = false;
                break;
            }
        }
        write_session_events_jsonl(&rep_dir.join(format!("transcript-{index}.jsonl")), &events)?;
        correlation_events.extend(events.iter().cloned());
        session_events.push((index, events, history_complete));
    }
    let export_sessions = session_events
        .iter()
        .map(|(index, events, _)| {
            (
                rep_dir.join(format!("transcript-{index}.jsonl")),
                events.clone(),
            )
        })
        .collect::<Vec<_>>();
    if let Err(error) = super::trace_exports::export_cli_batch(&export_sessions, &invoke) {
        capture_complete = false;
        write_json(rep_dir.join("trace-export-error.json"), &error.to_string())?;
    }
    for (index, events, history_complete) in session_events {
        let receipts: serde_json::Value =
            fs::read(rep_dir.join(format!("transcript-{index}.captures.json")))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_else(|| serde_json::json!([]));
        let capture_summary = super::introspection::summary_with_exports(&events, &receipts);
        model_coverage.observe(&events, &capture_summary, index != 0, history_complete);
        write_session_events_jsonl(&rep_dir.join(format!("transcript-{index}.jsonl")), &events)?;
    }
    if !model_coverage.complete() {
        capture_complete = false;
    }
    let telemetry = session_telemetry(&correlation_events).measurements;
    write_json(
        rep_dir.join("workflow-session-correlations.json"),
        &super::workflow_correlations::project(&final_inspection["attempts"], &correlation_events),
    )?;
    let manifest_path = rep_dir.join("capture-completeness.json");
    let mut manifest: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    let observation_pages = std::fs::read_dir(rep_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                name.to_string_lossy().starts_with("workflow-observations-")
                    && !name.to_string_lossy().ends_with("error.json")
            })
        })
        .collect::<Vec<_>>();
    manifest["boundaries"]["transactional_observations"] = serde_json::json!({
        "status": if rep_dir.join("workflow-observations-error.json").exists() || observation_pages.is_empty() {"partial_or_unavailable"} else {"verified_observed"},
        "scope":"finite declared source transitions; not semantic execution history",
        "pages": observation_pages.len(),
    });
    manifest["boundaries"]["model_exports"] = serde_json::json!({
        "status":if model_coverage.complete() {"verified_observed"} else {"incomplete_or_not_observed"},
        "scope":"retained request-build observations; child execution identity without model facts remains unknown; parent excluded only after complete fresh-tail capture; not provider wire or judgement",
        "sessions_observed":model_coverage.applicable_sessions,
        "sessions_complete":model_coverage.complete_sessions,
        "non_model_sessions":model_coverage.non_model_sessions,
        "sessions_unknown_applicability":model_coverage.unknown_sessions,
        "sessions_missing_request_boundaries":model_coverage.missing_boundaries
    });
    let judgement_status = if diagnostic_receipts.is_empty() {
        "not_observed"
    } else if diagnostic_receipts
        .iter()
        .all(|receipt| receipt["status"] == "retained_verified")
    {
        "verified_observed"
    } else {
        "unavailable_or_partial"
    };
    manifest["boundaries"]["judgement"] = serde_json::json!({
        "status": judgement_status, "scope": "bounded inspected dispatches; not full run coverage",
        "dispatches": diagnostic_receipts,
    });
    manifest["boundaries"]["session_history"] = serde_json::json!({"status":if session_history_complete {"complete"} else {"partial_or_unavailable"}});
    write_json(manifest_path, &manifest)?;
    measurements.extend(telemetry);
    measurements.insert(
        "workflow.capture_complete".into(),
        f64::from(capture_complete),
    );
    let activations = final_inspection["activations"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let worker_count = activations
        .iter()
        .filter(|value| {
            value["status"] == "completed"
                && value["node_id"]
                    .as_str()
                    .is_some_and(|id| !id.starts_with("loop.") && !id.starts_with("delegation."))
        })
        .count();
    measurements.insert("workflow.worker_activations".into(), worker_count as f64);
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for value in &activations {
        if let Some(id) = value["node_id"].as_str() {
            *counts.entry(id.into()).or_default() += 1;
        }
    }
    let repeated = counts
        .iter()
        .filter(|(id, _)| !id.starts_with("loop.") && !id.starts_with("delegation."))
        .map(|(_, count)| count.saturating_sub(1))
        .sum::<usize>();
    measurements.insert(
        "workflow.repeated_worker_activations".into(),
        repeated as f64,
    );
    let judge_calls = activations
        .iter()
        .filter(|value| value["node_id"] == "loop.judgement.evaluate")
        .count();
    measurements.insert("workflow.judge_calls".into(), judge_calls as f64);
    let status = final_inspection
        .pointer("/run/status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let completed = status == "completed"
        && final_inspection.pointer("/terminal_output/value/condition_met")
            == Some(&serde_json::Value::Bool(true));
    measurements.insert("workflow.completion_claimed".into(), f64::from(completed));
    let completed_value = final_inspection.pointer("/terminal_output/value");
    let output_text = completed_value.map_or_else(String::new, |value| {
        serde_json::to_string(
            &serde_json::json!({"summary":value["summary"],"evidence":value["evidence"]}),
        )
        .unwrap_or_default()
    });
    let artifact_observed = completed_value.is_some()
        || config.answer_file.is_some()
        || (config.oracle_command.is_some()
            && config.expected_contains.is_empty()
            && config.expected_json.is_none());
    measurements.insert(
        "workflow.artifact_observed".into(),
        f64::from(artifact_observed),
    );
    let artifact_correct = if config.expected_contains.is_empty()
        && config.expected_json.is_none()
        && config.oracle_command.is_none()
    {
        false
    } else {
        super::oracle::grade(&config, workspace, rep_dir, &output_text)?
    };
    let preservation = preserved
        .iter()
        .all(|(path, bytes)| fs::read(workspace.join(path)).is_ok_and(|current| current == *bytes));
    let oracle_passed = preservation
        && if config.expect_rejection {
            !completed && status == "failed"
        } else {
            artifact_correct && completed
        };
    let false_completion =
        completed && (!artifact_correct || !preservation || config.expect_rejection);
    let completion_failure = !config.expect_rejection && artifact_correct && !completed;
    for (key, value) in [
        ("oracle_passed", oracle_passed),
        ("artifact_correct", artifact_correct),
        ("preservation_passed", preservation),
        ("false_completion", false_completion),
        ("completion_failure", completion_failure),
    ] {
        measurements.insert(format!("workflow.{key}"), f64::from(value));
    }
    write_json(
        rep_dir.join("oracle.json"),
        &serde_json::json!({"schema_version":1,"passed":oracle_passed,"artifact_correct":artifact_correct,"artifact_observed":artifact_observed,"preservation_passed":preservation,"completion_claimed":completed,"false_completion":false_completion,"completion_failure":completion_failure,"status":status,"timed_out":timed_out,"capture_complete":capture_complete}),
    )?;
    let oracle_value = serde_json::json!({"completion_claimed":completed,"status":status,"capture_complete":capture_complete,"timed_out":timed_out,"artifact_correct":artifact_correct,"artifact_observed":artifact_observed,"false_completion":false_completion,"preservation_passed":preservation,"completion_failure":completion_failure});
    let mut diagnosis =
        super::diagnosis::classify(&oracle_value, &outputs, &final_inspection, &measurements);
    if let Some(optional) = super::diagnosis::optional_request(&oracle_value, &diagnosis) {
        // Only allowlisted measured facts enter this transient command argument. No request file,
        // prompt, raw workflow output, credentials, or provider diagnostics are persisted here.
        if let Ok(request_json) = serde_json::to_string(&optional) {
            let args = [
                "judgement".to_owned(),
                "optional-judge".to_owned(),
                "--request-json".to_owned(),
                request_json,
            ];
            if let Ok(value) = invoke(
                &args,
                "optional-eval-categorization",
                Duration::from_millis(case.timeout_ms.unwrap_or(suite.run.timeout_ms)),
            ) && let Ok(result) = serde_json::from_value(value)
            {
                super::diagnosis::attach_advisory(&mut diagnosis, &optional.request, result);
            }
        }
    }
    write_json(rep_dir.join("diagnosis.json"), &diagnosis)?;
    measurements.insert(
        "workflow.capture_allowance_ms".into(),
        capture_allowance.as_millis() as f64,
    );
    measurements.insert(
        "workflow.capture_time_ms".into(),
        capture_allowance
            .saturating_sub(capture_deadline.saturating_duration_since(Instant::now()))
            .as_millis() as f64,
    );
    measurements.insert(
        "workflow.capture_allowance_exhausted".into(),
        f64::from(Instant::now() >= capture_deadline),
    );
    let diagnostics = if timed_out {
        vec![EvalDiagnostic {
            severity: EvalDiagnosticSeverity::Error,
            message: format!(
                "workflow {} timeout exceeded; cancellation requested, terminal state requires verification",
                timeout_reason_name.unwrap_or("unknown")
            ),
        }]
    } else {
        Vec::new()
    };
    Ok(ExecutionOutput {
        exit_code: Some(if timed_out {
            124
        } else {
            i32::from(!oracle_passed)
        }),
        measurements,
        diagnostics,
        artifacts: vec![
            EvalArtifactRef {
                kind: "workflow_outputs".into(),
                path: "workflow-output.json".into(),
            },
            EvalArtifactRef {
                kind: "workflow_inspection".into(),
                path: "inspection.json".into(),
            },
        ],
    })
}

pub fn terminal_status(value: &serde_json::Value) -> bool {
    value
        .pointer("/run/status")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|status| ["completed", "failed", "cancelled"].contains(&status))
}

#[cfg(test)]
mod observation_tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn public_subtree_scan_compares_nested_progress_not_poll_metadata() {
        let root = "00000000-0000-0000-0000-000000000001";
        let child = "00000000-0000-0000-0000-000000000002";
        let grandchild = "00000000-0000-0000-0000-000000000003";
        let counter = Cell::new(1_u64);
        let timestamp = Cell::new(0_u64);
        let invoke = |args: &[String], _: &str, allowance: Duration| {
            assert!(allowance <= Duration::from_secs(3));
            let id = &args[2];
            Ok(match args[1].as_str() {
                "activity" => serde_json::json!({
                    "own": {"session_id": id, "parent_id": null, "states": ["running"],
                        "evidence": {"running":1,"queued":0,"permission_waits":0,"input_waits":0,
                            "counts_complete":true,"observed_event_sequence": timestamp.get(),"current_operation":null}},
                    "descendants":[],"coverage":"complete","next_after":null,
                    "observed_at_ms":timestamp.get(),"issues":[]
                }),
                "progress" => serde_json::json!({"version":2,"session_id":id,
                    "observation":{"status":"available","epoch":"epoch",
                        "counter": if id == grandchild {counter.get()} else {1}}}),
                "children" => {
                    let next = if id == root {
                        Some(child)
                    } else if id == child {
                        Some(grandchild)
                    } else {
                        None
                    };
                    let children = next.map(|next| serde_json::json!({
                        "id":next,"parent_id":id,"name":null,"client_count":0,"created_at_ms":0,"updated_at_ms":0
                    })).into_iter().collect::<Vec<_>>();
                    serde_json::json!({"children":children,"complete":true,"issues":[],"next_after":null})
                }
                _ => panic!("unexpected public read"),
            })
        };
        let budget = std::num::NonZeroUsize::new(8).unwrap();
        let mut clock =
            ExecutionClock::new(Duration::ZERO, Some(Duration::from_mins(3)), None, budget);
        scan_progress(
            root,
            &mut clock,
            &|| Duration::ZERO,
            &|| Duration::from_secs(15),
            &invoke,
            budget,
        )
        .unwrap();
        timestamp.set(9999);
        scan_progress(
            root,
            &mut clock,
            &|| Duration::from_secs(100),
            &|| Duration::from_secs(15),
            &invoke,
            budget,
        )
        .unwrap();
        assert_eq!(
            clock.remaining(Duration::from_secs(100), true).unwrap(),
            Some(Duration::from_secs(80))
        );
        counter.set(2);
        scan_progress(
            root,
            &mut clock,
            &|| Duration::from_secs(170),
            &|| Duration::from_secs(15),
            &invoke,
            budget,
        )
        .unwrap();
        assert_eq!(clock.expired(Duration::from_secs(250), true).unwrap(), None);
        assert_eq!(
            clock.expired(Duration::from_secs(350), true).unwrap(),
            Some(TimeoutReason::Idle)
        );
    }

    #[test]
    fn settled_child_retires_marker_before_unavailable_progress_lookup() {
        let root = "00000000-0000-0000-0000-000000000001";
        let child = "00000000-0000-0000-0000-000000000002";
        let grandchild = "00000000-0000-0000-0000-000000000003";
        let settled = Cell::new(false);
        let counter = Cell::new(1_u64);
        let invoke = |args: &[String], _: &str, _: Duration| {
            let id = &args[2];
            Ok(match args[1].as_str() {
                "activity" => {
                    let inactive = id == root || (id == child && settled.get());
                    serde_json::json!({
                        "own":{"session_id":id,"parent_id":null,
                            "states":if inactive {vec!["idle"]} else {vec!["running"]},
                            "evidence":{"running":u32::from(!inactive),"queued":0,"permission_waits":0,"input_waits":0,
                                "counts_complete":true,"observed_event_sequence":null,"current_operation":null}},
                        "descendants":[],"coverage":"complete","next_after":null,"observed_at_ms":0,"issues":[]
                    })
                }
                "progress" => {
                    assert_ne!(
                        id, root,
                        "nonexecuting parent must not require live progress"
                    );
                    // Released owners return unavailable, but the adapter must not ask.
                    assert!(
                        !(id == child && settled.get()),
                        "settled child progress would be unavailable after ownership release"
                    );
                    serde_json::json!({"version":2,"session_id":id,"observation":{
                        "status":"available","epoch":"e","counter":counter.get()}})
                }
                "children" => {
                    let next = if id == root {
                        Some(child)
                    } else if id == child {
                        Some(grandchild)
                    } else {
                        None
                    };
                    let children = next
                        .map(|next| {
                            serde_json::json!({"id":next,"parent_id":id,
                        "name":null,"client_count":0,"created_at_ms":0,"updated_at_ms":0})
                        })
                        .into_iter()
                        .collect::<Vec<_>>();
                    serde_json::json!({"children":children,"complete":true,"next_after":null,"issues":[]})
                }
                _ => panic!("unexpected public read"),
            })
        };
        let budget = std::num::NonZeroUsize::new(4).unwrap();
        let mut clock =
            ExecutionClock::new(Duration::ZERO, Some(Duration::from_mins(3)), None, budget);
        scan_progress(
            root,
            &mut clock,
            &|| Duration::ZERO,
            &|| Duration::from_secs(15),
            &invoke,
            budget,
        )
        .unwrap();
        assert!(clock.contains(child));
        settled.set(true);
        counter.set(2);
        scan_progress(
            root,
            &mut clock,
            &|| Duration::from_secs(170),
            &|| Duration::from_secs(15),
            &invoke,
            budget,
        )
        .unwrap();
        assert!(!clock.contains(child));
        assert!(clock.contains(grandchild));
        assert_eq!(clock.expired(Duration::from_secs(250), true).unwrap(), None);
    }

    #[test]
    fn public_progress_rejects_unavailable_future_and_mismatched_snapshots() {
        assert!(
            progress_mark(
                serde_json::json!({"version":1,"session_id":"s",
            "observation":{"status":"unavailable","reason":"coverage_unavailable"}}),
                "s"
            )
            .is_err()
        );
        for (version, id) in [(3, "s"), (2, "other")] {
            assert!(
                progress_mark(
                    serde_json::json!({"version":version,"session_id":id,
                "observation":{"status":"available","epoch":"e","counter":1}}),
                    "s"
                )
                .is_err()
            );
        }
    }

    #[test]
    fn public_scan_rejects_incomplete_discovery_and_cursor_replay() {
        let root = "00000000-0000-0000-0000-000000000001";
        let budget = std::num::NonZeroUsize::new(8).unwrap();
        for incomplete in [true, false] {
            let mut clock =
                ExecutionClock::new(Duration::ZERO, Some(Duration::from_mins(3)), None, budget);
            let invoke = |args: &[String], _: &str, _: Duration| {
                Ok(if args[1] == "activity" {
                    serde_json::json!({"own":{"session_id":root,"parent_id":null,"states":["idle"],
                        "evidence":{"running":0,"queued":0,"permission_waits":0,"input_waits":0,
                            "counts_complete":true,"observed_event_sequence":null,"current_operation":null}},
                        "descendants":[],"next_after":null,"coverage":"complete","observed_at_ms":0,"issues":[]})
                } else {
                    serde_json::json!({"children":[],"next_after":root,"complete":!incomplete,"issues":[]})
                })
            };
            assert!(
                scan_progress(
                    root,
                    &mut clock,
                    &|| Duration::ZERO,
                    &|| Duration::from_secs(15),
                    &invoke,
                    budget
                )
                .is_err()
            );
        }
    }

    #[test]
    fn active_session_with_unavailable_progress_fails_observation_not_idle() {
        let root = "00000000-0000-0000-0000-000000000001";
        let budget = std::num::NonZeroUsize::new(8).unwrap();
        let mut clock =
            ExecutionClock::new(Duration::ZERO, Some(Duration::from_mins(3)), None, budget);
        let invoke = |args: &[String], _: &str, _: Duration| {
            Ok(if args[1] == "activity" {
                serde_json::json!({"own":{"session_id":root,"parent_id":null,"states":["running"],
                    "evidence":{"running":1,"queued":0,"permission_waits":0,"input_waits":0,
                        "counts_complete":true,"observed_event_sequence":null,"current_operation":null}},
                    "descendants":[],"next_after":null,"coverage":"complete","observed_at_ms":0,"issues":[]})
            } else {
                serde_json::json!({"version":1,"session_id":root,"observation":{
                    "status":"unavailable","reason":"not_observed_or_evicted"}})
            })
        };
        assert!(
            scan_progress(
                root,
                &mut clock,
                &|| Duration::from_secs(200),
                &|| Duration::from_secs(15),
                &invoke,
                budget
            )
            .is_err()
        );
        assert!(clock.expired(Duration::from_secs(200), false).is_err());
    }

    #[test]
    fn scan_allowance_is_configurable_without_a_fixed_upper_ceiling() {
        let mut metadata = std::collections::BTreeMap::new();
        assert_eq!(
            activity_scan_allowance(&metadata).unwrap(),
            Duration::from_secs(15)
        );
        metadata.insert(
            "activity_scan_allowance_ms".into(),
            serde_json::json!(3_600_000),
        );
        assert_eq!(
            activity_scan_allowance(&metadata).unwrap(),
            Duration::from_hours(1)
        );
        for value in [
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!("15000"),
        ] {
            metadata.insert("activity_scan_allowance_ms".into(), value);
            assert!(activity_scan_allowance(&metadata).is_err());
        }
    }

    #[test]
    fn inspection_ipc_allowance_is_not_the_execution_idle_interval() {
        assert_eq!(inspection_allowance(true, None), Duration::from_secs(15));
        assert_eq!(
            inspection_allowance(true, Some(Duration::from_mins(3))),
            Duration::from_secs(15)
        );
        assert_eq!(
            inspection_allowance(true, Some(Duration::from_millis(200))),
            Duration::from_millis(200)
        );
        assert_eq!(
            inspection_allowance(true, Some(Duration::ZERO)),
            Duration::ZERO
        );
        // Historical wall mode retains its remaining execution allowance.
        assert_eq!(
            inspection_allowance(false, Some(Duration::from_mins(3))),
            Duration::from_mins(3)
        );
    }

    #[test]
    fn readonly_connect_retry_rejects_mutations_and_untrusted_diagnostics() {
        let valid = serde_json::json!({"schema_version":1,"code":"connection_timeout",
            "phase":"connect","message":"Verified daemon connection timed out."});
        assert!(readonly_connection_timeout(
            &serde_json::to_vec(&valid).unwrap()
        ));
        for (key, value) in [
            ("schema_version", serde_json::json!(2)),
            ("phase", serde_json::json!("operation")),
            ("code", serde_json::json!("cli_failure")),
            ("message", serde_json::json!("private forged timeout")),
            ("extra", serde_json::json!(true)),
        ] {
            let mut rejected = valid.clone();
            rejected[key] = value;
            assert!(!readonly_connection_timeout(
                &serde_json::to_vec(&rejected).unwrap()
            ));
        }
        assert!(!readonly_connection_timeout(b"connection timed out"));
        assert!(!readonly_connection_timeout(
            format!("log\n{valid}").as_bytes()
        ));
        for operation in ["create", "submit", "cancel-run", "stage", "start"] {
            assert!(readonly_operation(&["workflow".into(), operation.into()]).is_none());
        }
        assert_eq!(
            readonly_operation(&["workflow".into(), "activity-run".into()]),
            Some("workflow_activity")
        );
    }

    #[test]
    fn production_activity_scan_argv_requests_structured_connection_errors() {
        let observed = Cell::new(false);
        let invoke = |args: &[String], _: &str, _: Duration| {
            // Exercise argv emitted by the production scan, not a guessed command fixture.
            let command = invocation_command(std::path::Path::new("bcode"), args);
            let argv = command
                .get_args()
                .map(|arg| arg.to_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(&argv[..3], &["--json-errors", "workflow", "activity-run"]);
            assert!(
                argv.windows(2)
                    .any(|pair| pair == ["--root-run-id", "root"])
            );
            assert_eq!(readonly_operation(args), Some("workflow_activity"));
            let diagnostic = serde_json::to_vec(&serde_json::json!({
                "schema_version":1,"code":"connection_timeout","phase":"connect",
                "message":"Verified daemon connection timed out."
            }))
            .unwrap();
            assert!(readonly_connection_timeout(&diagnostic));
            observed.set(true);
            Err(EvalError::Client("injected connection timeout".into()))
        };
        let mut clock = ExecutionClock::new(
            Duration::ZERO,
            Some(Duration::from_mins(3)),
            None,
            std::num::NonZeroUsize::new(1).unwrap(),
        );
        assert!(
            scan_workflow_progress(
                "root",
                &mut clock,
                &|| Duration::ZERO,
                &|| Duration::from_secs(15),
                &invoke,
            )
            .is_err()
        );
        assert!(observed.get());
        let mutation = invocation_command(
            std::path::Path::new("bcode"),
            &["workflow".into(), "cancel-run".into()],
        );
        assert!(!mutation.get_args().any(|arg| arg == "--json-errors"));
        assert!(readonly_operation(&["workflow".into(), "activity".into()]).is_none());
    }

    #[test]
    fn operational_ipc_policy_accepts_five_second_observation_without_changing_wall_mode() {
        let mut metadata = std::collections::BTreeMap::new();
        for key in [
            "workflow_activity_ipc_allowance_ms",
            "workflow_inspection_ipc_allowance_ms",
        ] {
            assert_eq!(
                operational_allowance(&metadata, key).unwrap(),
                Duration::from_secs(15)
            );
            metadata.insert(key.into(), serde_json::json!(15000));
            let policy = operational_allowance(&metadata, key).unwrap();
            let fake_response = |allowance: Duration| assert!(allowance >= Duration::from_secs(5));
            fake_response(inspection_allowance_with_ipc(true, None, policy));
            fake_response(Duration::from_secs(20).min(policy));
            assert_eq!(
                inspection_allowance_with_ipc(false, Some(Duration::from_mins(3)), policy),
                Duration::from_mins(3)
            );
            assert_eq!(
                inspection_allowance_with_ipc(true, Some(Duration::from_secs(2)), policy),
                Duration::from_secs(2)
            );
            metadata.insert(key.into(), serde_json::json!(0));
            assert!(operational_allowance(&metadata, key).is_err());
        }
    }

    #[test]
    fn workflow_pages_pin_snapshot_revision_and_close_on_all_outcomes() {
        use std::cell::RefCell;
        for revised in [false, true] {
            let calls = RefCell::new(Vec::<Vec<String>>::new());
            let invoke = |args: &[String], _: &str, _: Duration| {
                calls.borrow_mut().push(args.to_vec());
                if args[1] == "activity-close" {
                    return Ok(serde_json::json!({}));
                }
                let field = |name: &str| {
                    args.iter()
                        .position(|arg| arg == name)
                        .map(|index| args[index + 1].clone())
                };
                let run = field("--run-id").unwrap();
                let section = field("--section").unwrap();
                let continuation = field("--after").is_some();
                let revision = if revised && continuation {
                    8
                } else if run == "root" {
                    7
                } else {
                    3
                };
                Ok(
                    serde_json::json!({"version":2,"scan_id":"scan","root_run_id":"root",
                    "snapshot_started_at_ms":10,"snapshot_age_ms":0,"scan_lifetime_ms":15000,"current_idle_eligible":true,"run_id":run,
                    "revision":revision,"section":section,
                    "attempts":if section == "attempts" {vec![serde_json::json!({
                        "dispatch_identity":"d","node_id":"n","activation_id":"a","attempt":1,
                        "observation":{"kind":"terminal","stage":{"source_id":"d","epoch":"a","sequence":3,"last_progress_at_ms":10}},
                        "terminal":true,"session_id":null,"progress":null})]} else {vec![]},
                    "children":if run == "root" && section == "children" {vec![serde_json::json!({
                        "child_run_id":"child","parent_run_id":"root"})]} else {vec![]},
                    "next_cursor":if run == "root" && section == "attempts" && !continuation {Some("next")} else {None},
                    "inventory_exhausted": !(run == "root" && section == "attempts" && !continuation),
                    "subtree_coverage_complete":run == "child" && section == "children","issues":[] }),
                )
            };
            let mut clock = ExecutionClock::new(
                Duration::ZERO,
                Some(Duration::from_mins(3)),
                None,
                std::num::NonZeroUsize::new(1).unwrap(),
            );
            let result = scan_workflow_progress(
                "root",
                &mut clock,
                &|| Duration::ZERO,
                &|| Duration::from_secs(15),
                &invoke,
            );
            assert_eq!(result.is_err(), revised);
            let calls = calls.borrow();
            assert_eq!(calls.last().unwrap()[1], "activity-close");
            assert_eq!(
                calls
                    .iter()
                    .filter(|args| args[1] == "activity-close")
                    .count(),
                1
            );
            assert!(calls[1].windows(2).any(|pair| pair == ["--revision", "7"]));
            assert!(
                calls[1]
                    .windows(2)
                    .any(|pair| pair == ["--scan-id", "scan"])
            );
            if !revised {
                assert!(
                    calls
                        .iter()
                        .any(|args| args.windows(2).any(|pair| pair == ["--run-id", "child"]))
                );
            }
        }
    }

    #[test]
    fn workflow_unavailable_marker_fails_but_asof_inventory_does_not_claim_current_idle() {
        for active in [false, true] {
            let closed = Cell::new(false);
            let invoke = |args: &[String], _: &str, _: Duration| {
                if args[1] == "activity-close" {
                    closed.set(true);
                    return Ok(serde_json::json!({}));
                }
                let section =
                    args[args.iter().position(|arg| arg == "--section").unwrap() + 1].clone();
                Ok(
                    serde_json::json!({"version":2,"scan_id":"scan","root_run_id":"root",
                    "snapshot_started_at_ms":10,"snapshot_age_ms":0,"scan_lifetime_ms":15000,"current_idle_eligible":false,"run_id":"root",
                    "revision":7,"section":section,"children":[],"next_cursor":null,
                    "inventory_exhausted":true,"subtree_coverage_complete":section == "children","issues":[],
                    "attempts":if active && section == "attempts" {vec![serde_json::json!({
                        "dispatch_identity":"d","node_id":"n","activation_id":"a","attempt":1,
                        "observation":{"kind":"unavailable","reason":"unknown_dispatch_source"},
                        "terminal":false,"session_id":"s","progress":{"version":2,"session_id":"s",
                            "observation":{"status":"unavailable","reason":"coverage_unavailable"}}})]} else {vec![]} }),
                )
            };
            let mut clock = ExecutionClock::new(
                Duration::ZERO,
                Some(Duration::from_mins(3)),
                None,
                std::num::NonZeroUsize::new(1).unwrap(),
            );
            assert_eq!(
                scan_workflow_progress(
                    "root",
                    &mut clock,
                    &|| Duration::from_secs(200),
                    &|| Duration::from_secs(15),
                    &invoke
                )
                .is_err(),
                active
            );
            assert!(closed.get());
            assert!(clock.expired(Duration::from_secs(200), false).is_err());
        }
    }

    fn readiness_error() -> EvalError {
        EvalError::Client("server returned error workflow_capability_unavailable: workflow readiness status is temporarily unavailable; retry the workflow request (no initialization progress has been verified)".into())
    }

    #[test]
    fn explicit_readiness_retries_reads_but_other_errors_do_not() {
        let calls = Cell::new(0);
        let result = observe(
            &[],
            "inspection",
            || Duration::from_secs(1),
            &|_, _, _| {
                calls.set(calls.get() + 1);
                if calls.get() < 3 {
                    Err(readiness_error())
                } else {
                    Ok(serde_json::json!({}))
                }
            },
            |_| {},
        );
        assert!(result.is_ok());
        assert_eq!(calls.get(), 3);
        calls.set(0);
        let result = observe(
            &[],
            "inspection",
            || Duration::from_secs(1),
            &|_, _, _| {
                calls.set(calls.get() + 1);
                Err(EvalError::Client(
                    "workflow_capability_unavailable: incompatible state".into(),
                ))
            },
            |_| {},
        );
        assert!(result.is_err());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn cancellation_verification_retains_exact_terminal_snapshot() {
        let snapshot = serde_json::json!({"run":{"status":"failed"},"attempts":[{"id":"exact"}]});
        assert_eq!(
            verify_cancellation("run", &|_, _, _| Ok(snapshot.clone())),
            Some(snapshot)
        );
    }

    #[test]
    fn execution_timing_excludes_post_terminal_export_overhead() {
        let start = Instant::now();
        let terminal = start + Duration::from_millis(20);
        let export_end = terminal + Duration::from_secs(5);
        assert_eq!(
            execution_duration(start, terminal),
            Duration::from_millis(20)
        );
        assert_ne!(
            execution_duration(start, terminal),
            execution_duration(start, export_end)
        );
    }

    #[test]
    fn readiness_retry_count_and_deadline_are_bounded() {
        let calls = Cell::new(0);
        let result = observe(
            &[],
            "inspection",
            || Duration::from_secs(1),
            &|_, _, _| {
                calls.set(calls.get() + 1);
                Err(readiness_error())
            },
            |_| {},
        );
        assert!(result.is_err());
        assert_eq!(calls.get(), 5);
        calls.set(0);
        let result = observe(
            &[],
            "inspection",
            || Duration::ZERO,
            &|_, _, _| {
                calls.set(calls.get() + 1);
                Err(readiness_error())
            },
            |_| panic!("expired observation must not wait"),
        );
        assert!(result.is_err());
        assert_eq!(calls.get(), 1);
    }
}
