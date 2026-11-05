//! Canonical lifecycle evidence reduced once for every frontend. No dispatch authority lives here.
use bcode_harness_models::{
    HarnessExecutionEvent, HarnessNotificationState, HarnessObservationKind,
    HarnessTerminalDisposition, HarnessTrackingCoverage, HarnessWorkState,
    MAX_HARNESS_EXECUTION_ITEMS,
};
use bcode_session_models::{
    SessionEvent,
    external_backend::{ExternalBackendEnvelope, ExternalBackendRecord},
};
use bcode_session_view_models::execution::{
    ExecutionAttemptView, ExecutionCoverageView, ExecutionItemView, ExecutionLifecycleView,
    ExecutionNotificationView, ExecutionStateView,
};

fn attempt(value: &bcode_harness_models::HarnessAttemptIdentity) -> ExecutionAttemptView {
    ExecutionAttemptView {
        submission_id: value.submission_id.clone(),
        attempt_id: value.attempt_id.clone(),
    }
}
const fn state(value: HarnessWorkState) -> ExecutionStateView {
    match value {
        HarnessWorkState::Pending => ExecutionStateView::Pending,
        HarnessWorkState::Running => ExecutionStateView::Running,
        HarnessWorkState::Completed => ExecutionStateView::Completed,
        HarnessWorkState::Failed => ExecutionStateView::Failed,
        HarnessWorkState::Interrupted => ExecutionStateView::Interrupted,
        HarnessWorkState::Unknown => ExecutionStateView::Unknown,
    }
}
const fn notification(value: HarnessNotificationState) -> ExecutionNotificationView {
    match value {
        HarnessNotificationState::Pending => ExecutionNotificationView::Pending,
        HarnessNotificationState::Observed => ExecutionNotificationView::Observed,
        HarnessNotificationState::NotExpected => ExecutionNotificationView::NotExpected,
        HarnessNotificationState::Unknown => ExecutionNotificationView::Unknown,
    }
}
fn merge_optional<T: PartialEq>(old: &mut Option<T>, incoming: Option<T>) -> bool {
    match (old.as_ref(), incoming) {
        (Some(previous), Some(next)) if previous != &next => false,
        (None, Some(next)) => {
            *old = Some(next);
            true
        }
        _ => true,
    }
}
fn merge_item(items: &mut Vec<ExecutionItemView>, next: ExecutionItemView, overflow: bool) -> bool {
    if let Some(old) = items
        .iter_mut()
        .find(|old| old.generation == next.generation && old.id == next.id)
    {
        // Validate all enrichment before publishing any portion of a conflicting observation.
        let mut candidate = old.clone();
        if !merge_optional(&mut candidate.origin, next.origin)
            || !merge_optional(&mut candidate.native_id, next.native_id)
            || !merge_optional(&mut candidate.source_work_id, next.source_work_id)
        {
            return false;
        }
        if old.state.terminal()
            && next.state != old.state
            && next.state != ExecutionStateView::Unknown
        {
            return false;
        }
        candidate.state = if old.state.terminal()
            || (old.state == ExecutionStateView::Running
                && next.state == ExecutionStateView::Pending)
        {
            old.state
        } else {
            next.state
        };
        candidate.label = next.label;
        candidate.notification = match (old.notification, next.notification) {
            (
                ExecutionNotificationView::Observed | ExecutionNotificationView::NotExpected,
                ExecutionNotificationView::Pending,
            ) => old.notification,
            (_, next) => next,
        };
        *old = candidate;
        true
    } else if overflow {
        false
    } else {
        items.push(next);
        true
    }
}

fn reduce_submission_terminal(
    view: &mut ExecutionLifecycleView,
    generation: u64,
    origin: &bcode_harness_models::HarnessAttemptIdentity,
    native_turn_id: Option<String>,
    disposition: HarnessTerminalDisposition,
    overflow: bool,
) -> bool {
    let id = format!(
        "{}:{}{}",
        origin.submission_id.len(),
        origin.submission_id,
        origin.attempt_id
    );
    let state = match disposition {
        HarnessTerminalDisposition::Completed => ExecutionStateView::Completed,
        HarnessTerminalDisposition::Failed => ExecutionStateView::Failed,
        HarnessTerminalDisposition::Interrupted => ExecutionStateView::Interrupted,
    };
    merge_item(
        &mut view.submissions,
        ExecutionItemView {
            generation,
            id,
            native_id: native_turn_id,
            origin: Some(attempt(origin)),
            source_work_id: None,
            label: "Foreground reply".into(),
            state,
            notification: ExecutionNotificationView::NotExpected,
        },
        overflow,
    )
}

fn reduce_coverage(
    view: &mut ExecutionLifecycleView,
    next: ExecutionCoverageView,
    overflow: bool,
) -> bool {
    if let Some(old) = view
        .coverage
        .iter_mut()
        .find(|old| old.generation == next.generation && old.attempt == next.attempt)
    {
        *old = next;
        true
    } else if overflow {
        false
    } else {
        view.coverage.push(next);
        true
    }
}

fn reduce_execution(
    view: &mut ExecutionLifecycleView,
    generation: u64,
    execution: HarnessExecutionEvent,
    gap: bool,
) {
    let total = view.submissions.len()
        + view.background.len()
        + view.continuations.len()
        + view.coverage.len();
    let overflow = total >= MAX_HARNESS_EXECUTION_ITEMS;
    if matches!(
        &execution,
        HarnessExecutionEvent::BackgroundWork {
            state: HarnessWorkState::Unknown,
            ..
        } | HarnessExecutionEvent::Continuation {
            state: HarnessWorkState::Unknown,
            ..
        }
    ) {
        view.uncertain = true;
    }
    let accepted = match execution {
        HarnessExecutionEvent::SubmissionTerminal {
            attempt: origin,
            native_turn_id,
            native_frame_id: _,
            disposition,
            output: _,
        } => reduce_submission_terminal(
            view,
            generation,
            &origin,
            native_turn_id,
            disposition,
            overflow,
        ),
        HarnessExecutionEvent::BackgroundWork {
            work_id,
            native_task_id,
            origin,
            parent_work_id,
            work_kind: _,
            label,
            state: next,
            notification: delivery,
        } => merge_item(
            &mut view.background,
            ExecutionItemView {
                generation,
                id: work_id,
                native_id: native_task_id,
                origin: origin.as_ref().map(attempt),
                source_work_id: parent_work_id,
                label: label.chars().take(256).collect(),
                state: state(next),
                notification: notification(delivery),
            },
            overflow,
        ),
        HarnessExecutionEvent::Continuation {
            continuation_id,
            origin,
            source_work_id,
            native_turn_id,
            state: next,
            output: _,
        } => merge_item(
            &mut view.continuations,
            ExecutionItemView {
                generation,
                id: continuation_id,
                native_id: native_turn_id,
                origin: origin.as_ref().map(attempt),
                source_work_id,
                label: "Automatic native follow-up".into(),
                state: state(next),
                notification: ExecutionNotificationView::NotExpected,
            },
            overflow,
        ),
        HarnessExecutionEvent::Coverage {
            attempt: origin,
            background,
            continuations,
        } => reduce_coverage(
            view,
            ExecutionCoverageView {
                generation,
                attempt: attempt(&origin),
                background_complete: !gap && background == HarnessTrackingCoverage::Complete,
                continuations_complete: !gap && continuations == HarnessTrackingCoverage::Complete,
            },
            overflow,
        ),
    };
    if !accepted {
        view.uncertain = true;
        if overflow {
            view.truncated = true;
        }
    }
}

fn apply_observation_page(
    view: &mut ExecutionLifecycleView,
    event: &SessionEvent,
    operation: &bcode_session_models::ExternalHarnessOperation,
    page: &bcode_harness_models::HarnessObservationPage,
) -> bool {
    if operation.owner.session_id != event.session_id
        || view
            .session_id
            .as_deref()
            .is_some_and(|id| id != event.session_id.to_string())
        || view
            .generation
            .is_some_and(|generation| operation.owner.generation < generation)
    {
        return false;
    }
    let lost_coverage = page.completeness != bcode_harness_models::HarnessCompleteness::Complete
        || page.status.completeness != bcode_harness_models::HarnessCompleteness::Complete;
    let mut changed = lost_coverage;
    if lost_coverage {
        view.session_id = Some(event.session_id.to_string());
        view.generation = Some(operation.owner.generation);
        view.uncertain = true;
        for coverage in &mut view.coverage {
            coverage.background_complete = false;
            coverage.continuations_complete = false;
        }
    }
    for observation in &page.observations {
        let nested = SessionEvent {
            kind: bcode_session_models::SessionEventKind::ExternalObservationRecorded {
                version: 1,
                observation: Box::new(observation.clone()),
            },
            ..event.clone()
        };
        changed |= apply(view, &nested);
    }
    if lost_coverage {
        for coverage in &mut view.coverage {
            coverage.background_complete = false;
            coverage.continuations_complete = false;
        }
    }
    changed
}

fn apply_submission(
    view: &mut ExecutionLifecycleView,
    submission: &bcode_harness_models::HarnessSubmission,
) -> bool {
    let generation = submission.generation;
    let id = format!(
        "{}:{}{}",
        submission.submission_id.len(),
        submission.submission_id,
        submission.attempt_id
    );
    let overflow = view.submissions.len()
        + view.background.len()
        + view.continuations.len()
        + view.coverage.len()
        >= MAX_HARNESS_EXECUTION_ITEMS;
    if !view
        .submissions
        .iter()
        .any(|item| item.generation == generation && item.id == id)
    {
        if overflow {
            view.truncated = true;
            view.uncertain = true;
        } else {
            view.submissions.push(ExecutionItemView {
                generation,
                id,
                native_id: None,
                origin: Some(ExecutionAttemptView {
                    submission_id: submission.submission_id.clone(),
                    attempt_id: submission.attempt_id.clone(),
                }),
                source_work_id: None,
                label: "Foreground reply".into(),
                state: ExecutionStateView::Pending,
                notification: ExecutionNotificationView::NotExpected,
            });
        }
    }
    true
}

/// Apply one canonical observation to a bounded display projection. Invalid evidence leaves a fence.
/// Returns whether this event contains managed execution evidence or a tracking gap.
#[must_use]
pub fn apply(view: &mut ExecutionLifecycleView, event: &SessionEvent) -> bool {
    if let bcode_session_models::SessionEventKind::ExternalHarnessOperation { operation } =
        &event.kind
        && let bcode_session_models::ExternalHarnessOperationRecord::ObservationPage {
            page, ..
        } = &operation.record
    {
        return apply_observation_page(view, event, operation, page);
    }
    let envelope = match ExternalBackendEnvelope::from_event(event) {
        Ok(Some(value)) => value,
        Ok(None) => return false,
        Err(_) => {
            view.uncertain = true;
            return false;
        }
    };
    let record = envelope.record;
    if view
        .session_id
        .as_deref()
        .is_some_and(|id| id != record.session_id())
    {
        view.uncertain = true;
        return false;
    }
    let generation = record.generation();
    if view.generation.is_some_and(|current| generation < current) {
        return false;
    }
    if view.generation.is_some_and(|current| generation != current) {
        view.uncertain = true;
        for coverage in &mut view.coverage {
            coverage.background_complete = false;
            coverage.continuations_complete = false;
        }
    }
    view.session_id = Some(record.session_id().to_owned());
    view.generation = Some(generation);
    if let ExternalBackendRecord::Submission { submission } = &record {
        return apply_submission(view, submission);
    }
    let ExternalBackendRecord::Observation { observation } = record else {
        return false;
    };
    if view
        .session_id
        .as_deref()
        .is_some_and(|id| id != observation.session_id)
    {
        view.uncertain = true;
        return false;
    }
    if view
        .generation
        .is_some_and(|generation| observation.generation < generation)
    {
        return false;
    }
    if view
        .generation
        .is_some_and(|generation| generation != observation.generation)
    {
        view.uncertain = true;
        for coverage in &mut view.coverage {
            coverage.background_complete = false;
            coverage.continuations_complete = false;
        }
    }
    view.session_id = Some(observation.session_id.clone());
    view.generation = Some(observation.generation);
    let gap = matches!(
        observation.completeness,
        bcode_harness_models::HarnessCompleteness::Gap
            | bcode_harness_models::HarnessCompleteness::Unknown
    );
    if gap {
        view.uncertain = true;
        for coverage in &mut view.coverage {
            coverage.background_complete = false;
            coverage.continuations_complete = false;
        }
    }
    let HarnessObservationKind::Execution { event: execution } = observation.kind else {
        return gap;
    };
    if execution.validate().is_err() {
        view.uncertain = true;
        return true;
    }
    reduce_execution(view, observation.generation, execution, gap);
    true
}

/// Render current typed lifecycle alongside canonical text, not as raw vendor interpretation.
pub(crate) fn text(
    view: &mut ExecutionLifecycleView,
    event: &SessionEvent,
) -> Result<Option<String>, String> {
    let content = crate::external::canonical_external_text(event)?;
    if apply(view, event) {
        Ok(Some(format!(
            "{}\n{}",
            content.unwrap_or_default(),
            view.details()
        )))
    } else {
        Ok(content)
    }
}

pub(crate) fn event_label(event: &HarnessExecutionEvent) -> String {
    match event {
        HarnessExecutionEvent::SubmissionTerminal {
            attempt,
            disposition,
            output,
            ..
        } => format!(
            "Foreground reply {} · attempt {} · {disposition:?} (background and continuations independent){}",
            attempt.submission_id,
            attempt.attempt_id,
            output
                .as_ref()
                .map_or_else(String::new, |text| format!("\n{text}"))
        ),
        HarnessExecutionEvent::BackgroundWork {
            work_id,
            state,
            notification,
            label,
            ..
        } => format!(
            "Background task {work_id} · {label} · {state:?} · notification {notification:?}"
        ),
        HarnessExecutionEvent::Continuation {
            continuation_id,
            state,
            output,
            ..
        } => format!(
            "Automatic continuation {continuation_id} · {state:?} (does not reopen foreground reply){}",
            output
                .as_ref()
                .map_or_else(String::new, |text| format!("\n{text}"))
        ),
        HarnessExecutionEvent::Coverage {
            background,
            continuations,
            ..
        } => format!(
            "Tracking through observation: background {background:?} · continuations {continuations:?}; not future silence or workflow success"
        ),
    }
}

#[cfg(test)]
mod tests;
