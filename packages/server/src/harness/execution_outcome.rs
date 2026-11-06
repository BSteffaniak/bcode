//! Workflow verdicts from bounded canonical execution evidence, never native dispatch.
use super::{HarnessWorkflowReceipt, complete_assignment_output, deferred, invalid};
use bcode_harness_models::{
    HarnessAttemptIdentity, HarnessCompleteness, HarnessCompletionRequirement,
    HarnessExecutionEvent as E, HarnessNotificationState as N, HarnessObservation,
    HarnessObservationKind, HarnessTerminalDisposition as D, HarnessTrackingCoverage as C,
    HarnessWorkState as W, MAX_HARNESS_EXECUTION_ITEMS,
};
use bcode_workflow_store::{AttemptObservation, WorkflowStoreError};
use std::collections::BTreeMap;

#[cfg(test)]
#[path = "execution_outcome_tests.rs"]
mod tests;

fn attempt(receipt: &HarnessWorkflowReceipt) -> Result<HarnessAttemptIdentity, WorkflowStoreError> {
    let bcode_harness_models::HarnessRequestKind::Submit { submission, .. } =
        &receipt.call.harness.kind
    else {
        return Err(invalid("missing managed submission"));
    };
    Ok(HarnessAttemptIdentity {
        submission_id: submission.submission_id.clone(),
        attempt_id: submission.attempt_id.clone(),
    })
}

// Persistence owns historical transition validation. These additional checks keep a
// conflicting bounded snapshot from becoming a workflow verdict.
fn update_state(prior: W, next: W) -> Option<W> {
    if prior.is_terminal() {
        if next == prior || next == W::Unknown {
            Some(prior)
        } else {
            None
        }
    } else if prior == W::Running && next == W::Pending {
        None
    } else {
        Some(next)
    }
}

#[derive(Default)]
struct Work {
    tasks: BTreeMap<String, (W, N)>,
    continuations: BTreeMap<String, W>,
    coverage: Option<(C, C)>,
    unknown_origin: bool,
    uncertain: std::collections::BTreeSet<String>,
    notified: std::collections::BTreeSet<String>,
    continued: std::collections::BTreeSet<String>,
}

impl Work {
    fn observe(&mut self, event: &E, exact: &HarnessAttemptIdentity) -> bool {
        match event {
            E::BackgroundWork {
                work_id,
                origin,
                state,
                notification,
                ..
            } if origin.as_ref().is_none_or(|origin| origin == exact) => {
                self.unknown_origin |= origin.is_none();
                if *state == W::Unknown || *notification == N::Unknown {
                    self.uncertain.insert(format!("work:{work_id}"));
                } else {
                    self.uncertain.remove(&format!("work:{work_id}"));
                }
                if *notification == N::Observed {
                    self.notified.insert(work_id.clone());
                }
                let state = if let Some((prior, _)) = self.tasks.get(work_id) {
                    let Some(next) = update_state(*prior, *state) else {
                        return false;
                    };
                    next
                } else {
                    *state
                };
                self.tasks.insert(work_id.clone(), (state, *notification));
            }
            E::Continuation {
                continuation_id,
                origin,
                source_work_id,
                state,
                ..
            } if origin.as_ref().is_none_or(|origin| origin == exact) => {
                self.unknown_origin |= origin.is_none();
                if *state == W::Unknown {
                    self.uncertain
                        .insert(format!("continuation:{continuation_id}"));
                } else {
                    self.uncertain
                        .remove(&format!("continuation:{continuation_id}"));
                }
                if let Some(source) = source_work_id {
                    self.continued.insert(source.clone());
                }
                let state = if let Some(prior) = self.continuations.get(continuation_id) {
                    let Some(next) = update_state(*prior, *state) else {
                        return false;
                    };
                    next
                } else {
                    *state
                };
                self.continuations.insert(continuation_id.clone(), state);
            }
            E::Coverage {
                attempt,
                background,
                continuations,
            } if attempt == exact => {
                self.coverage = Some((*background, *continuations));
            }
            _ => {}
        }
        self.tasks.len() + self.continuations.len() <= MAX_HARNESS_EXECUTION_ITEMS
    }

    fn verdict(&self) -> Option<AttemptObservation> {
        if self.unknown_origin
            || !self.uncertain.is_empty()
            || self.coverage != Some((C::Complete, C::Complete))
        {
            return Some(deferred("managed associated-work coverage unknown"));
        }
        if self.tasks.values().any(|(state, notification)| {
            !state.is_terminal() || !matches!(notification, N::NotExpected | N::Observed)
        }) || self
            .continuations
            .values()
            .any(|state| !state.is_terminal())
        {
            return Some(deferred(
                "managed work, notification or continuation pending",
            ));
        }
        // Observing a notification alone is not proof its automatic follow-up ended.
        if !self.notified.is_subset(&self.continued) {
            return Some(deferred("managed notification follow-up unaccounted"));
        }
        if self.tasks.values().any(|(state, _)| *state == W::Failed)
            || self.continuations.values().any(|state| *state == W::Failed)
        {
            return Some(AttemptObservation::Failed {
                message: "managed associated work failed".into(),
            });
        }
        if self
            .tasks
            .values()
            .any(|(state, _)| *state == W::Interrupted)
            || self
                .continuations
                .values()
                .any(|state| *state == W::Interrupted)
        {
            return Some(AttemptObservation::Cancelled);
        }
        None
    }
}

fn evidence_fence(
    observation: &HarnessObservation,
    session_id: bcode_session_models::SessionId,
    generation: u64,
    requirement: HarnessCompletionRequirement,
) -> Result<Option<AttemptObservation>, WorkflowStoreError> {
    observation
        .validate()
        .map_err(|_| invalid("invalid managed execution observation"))?;
    if observation.session_id != session_id.to_string()
        || observation.generation != generation
        || observation.completeness == HarnessCompleteness::Gap
        || (matches!(&observation.kind, HarnessObservationKind::Execution { .. })
            && observation.completeness != HarnessCompleteness::Complete)
    {
        return Ok(Some(deferred(
            "managed execution evidence incomplete or stale",
        )));
    }
    if matches!(&observation.kind, HarnessObservationKind::Status { status }
        if status.completeness == HarnessCompleteness::Gap)
        || (requirement == HarnessCompletionRequirement::AssociatedWork
            && matches!(
                &observation.kind,
                HarnessObservationKind::Interaction { .. }
            ))
    {
        return Ok(Some(deferred(
            "managed interaction or evidence gap unresolved",
        )));
    }
    Ok(None)
}

fn evaluate_adopted_foreground(
    observations: &[HarnessObservation],
    receipt: &HarnessWorkflowReceipt,
) -> Result<Option<AttemptObservation>, WorkflowStoreError> {
    let Some(proof) = &receipt.workflow_obligation_adoption else {
        return Ok(Some(deferred(
            "async handoff requires durable canonical adoption acknowledgment",
        )));
    };
    if proof.request.source.dispatch_identity != receipt.dispatch_identity
        || proof.request.run_id != receipt.run_id
    {
        return Ok(Some(deferred("async adoption source identity mismatch")));
    }
    let mut foreground: HarnessWorkflowReceipt =
        serde_json::from_value(serde_json::to_value(receipt)?)?;
    foreground.completion_requirement = HarnessCompletionRequirement::Foreground;
    evaluate(observations, &foreground)
}

/// None selects the strict legacy native-turn evidence evaluator.
pub(super) fn evaluate(
    observations: &[HarnessObservation],
    receipt: &HarnessWorkflowReceipt,
) -> Result<Option<AttemptObservation>, WorkflowStoreError> {
    if !observations
        .iter()
        .any(|o| matches!(&o.kind, HarnessObservationKind::Execution { .. }))
    {
        return Ok(None);
    }
    if observations.len() > bcode_harness_models::MAX_HARNESS_OBSERVATIONS {
        return Ok(Some(deferred(
            "managed execution snapshot exceeds wire bound",
        )));
    }
    if receipt.completion_requirement == HarnessCompletionRequirement::AsyncHandoff {
        return evaluate_adopted_foreground(observations, receipt);
    }
    if receipt.completion_requirement == HarnessCompletionRequirement::WorkspaceRelease {
        return Ok(Some(deferred(
            "workspace release requires separate host authorization",
        )));
    }
    let exact = attempt(receipt)?;
    match evaluate_assignment(
        observations,
        receipt.session_id,
        receipt.generation,
        &exact,
        receipt.completion_requirement,
    )? {
        None => Ok(None),
        Some(AssignmentVerdict::Pending(observation)) => Ok(Some(observation)),
        Some(AssignmentVerdict::Completed {
            native_turn_id,
            output,
        }) => complete_assignment_output(receipt, native_turn_id.as_deref(), &output).map(Some),
    }
}

/// Shared canonical verdict independent of block or prompt output representation.
pub(super) enum AssignmentVerdict {
    Pending(AttemptObservation),
    Completed {
        native_turn_id: Option<String>,
        output: String,
    },
}

/// Evaluate exact foreground and associated work without acquiring authority or replaying work.
pub(super) fn evaluate_assignment(
    observations: &[HarnessObservation],
    session_id: bcode_session_models::SessionId,
    generation: u64,
    exact: &HarnessAttemptIdentity,
    requirement: HarnessCompletionRequirement,
) -> Result<Option<AssignmentVerdict>, WorkflowStoreError> {
    if !observations
        .iter()
        .any(|o| matches!(&o.kind, HarnessObservationKind::Execution { .. }))
    {
        return Ok(None);
    }
    if observations.len() > bcode_harness_models::MAX_HARNESS_OBSERVATIONS
        || !matches!(
            requirement,
            HarnessCompletionRequirement::Foreground | HarnessCompletionRequirement::AssociatedWork
        )
    {
        return Ok(Some(AssignmentVerdict::Pending(deferred(
            "unsupported completion responsibility or excessive snapshot",
        ))));
    }
    let mut terminal = None;
    let mut work = Work::default();
    for observation in observations {
        if let Some(fence) = evidence_fence(observation, session_id, generation, requirement)? {
            return Ok(Some(AssignmentVerdict::Pending(fence)));
        }
        if let HarnessObservationKind::Execution { event } = &observation.kind {
            if let E::SubmissionTerminal {
                attempt,
                native_turn_id,
                disposition,
                output,
                ..
            } = event
                && attempt == exact
            {
                let next = (*disposition, native_turn_id.clone(), output.clone());
                if terminal.as_ref().is_some_and(|prior| prior != &next) {
                    return Ok(Some(AssignmentVerdict::Pending(deferred(
                        "conflicting managed foreground terminal",
                    ))));
                }
                terminal = Some(next);
            }
            if requirement == HarnessCompletionRequirement::AssociatedWork
                && !work.observe(event, exact)
            {
                return Ok(Some(AssignmentVerdict::Pending(deferred(
                    "conflicting or excessive managed execution evidence",
                ))));
            }
        }
    }
    let Some((disposition, native_turn_id, output)) = terminal else {
        return Ok(Some(AssignmentVerdict::Pending(deferred(
            "awaiting exact managed foreground terminal",
        ))));
    };
    if requirement == HarnessCompletionRequirement::AssociatedWork
        && let Some(verdict) = work.verdict()
    {
        return Ok(Some(AssignmentVerdict::Pending(verdict)));
    }
    Ok(Some(match disposition {
        D::Interrupted => AssignmentVerdict::Pending(AttemptObservation::Cancelled),
        D::Failed => AssignmentVerdict::Pending(AttemptObservation::Failed {
            message: "managed foreground failed".into(),
        }),
        D::Completed => {
            let Some(output) = output else {
                return Ok(Some(AssignmentVerdict::Pending(deferred(
                    "managed foreground output incomplete",
                ))));
            };
            AssignmentVerdict::Completed {
                native_turn_id,
                output,
            }
        }
    }))
}
