//! Codex lifecycle evidence, separate from transport mutation authority.
//! Native item completion and turn completion are different boundaries. In particular,
//! commandExecution.processId identifies a native PTY, not a Bcode child or tool grant.
use crate::transport::CodexError;
use bcode_harness_models::{
    HarnessAttemptIdentity, HarnessExecutionEvent, HarnessNotificationState, HarnessSubmission,
    HarnessTerminalDisposition, HarnessTrackingCoverage, HarnessWorkKind, HarnessWorkState,
    MAX_HARNESS_EXECUTION_ITEMS, MAX_HARNESS_TEXT_BYTES,
};
use serde_json::Value;
use std::collections::{BTreeMap, btree_map::Entry};

#[cfg(test)]
#[path = "execution_tests.rs"]
mod tests;

struct Turn {
    attempt: Option<HarnessAttemptIdentity>,
    steered_attempts: Vec<HarnessAttemptIdentity>,
    continuation: String,
    output: Option<String>,
    /// Last unphased (legacy) assistant message; output only if no final answer arrives.
    unphased_output: Option<String>,
    terminal: bool,
    foreground: bool,
}
struct Work {
    id: String,
    origin: Option<HarnessAttemptIdentity>,
    native_task: String,
    kind: HarnessWorkKind,
    state: HarnessWorkState,
    notification: Option<HarnessNotificationState>,
}
#[derive(Default)]
pub struct Execution {
    turns: BTreeMap<String, Turn>,
    work: BTreeMap<String, Work>,
    retired_contexts: bool,
    uncorrelated_continuation: Option<String>,
}
impl Execution {
    #[must_use]
    fn retained_count(&self) -> usize {
        self.work.len()
            + self.turns.len()
            + usize::from(self.uncorrelated_continuation.is_some())
            + self
                .turns
                .values()
                .map(|turn| turn.steered_attempts.len())
                .sum::<usize>()
    }

    fn retire_completed_foreground(&mut self) {
        // Canonical observations already own emitted outcomes. Retain only the live
        // correlation contexts needed by outstanding work/continuations; never evict
        // responsibility merely to admit another turn.
        let before = self.turns.len();
        let turns = &self.turns;
        let work = &self.work;
        let retained = turns
            .iter()
            .filter_map(|(id, turn)| {
                let responsible = turn.attempt.as_ref().is_some_and(|attempt| {
                    work.values()
                        .any(|work| work.origin.as_ref() == Some(attempt))
                        || turns.values().any(|other| {
                            !other.foreground
                                && !other.terminal
                                && other.attempt.as_ref() == Some(attempt)
                        })
                }) || turn.steered_attempts.iter().any(|attempt| {
                    work.values()
                        .any(|work| work.origin.as_ref() == Some(attempt))
                        || turns.values().any(|other| {
                            !other.foreground
                                && !other.terminal
                                && other.attempt.as_ref() == Some(attempt)
                        })
                });
                (!turn.terminal || responsible).then(|| id.clone())
            })
            .collect::<std::collections::BTreeSet<_>>();
        self.turns.retain(|id, _| retained.contains(id));
        self.retired_contexts |= before != self.turns.len();
    }

    pub(super) fn retire_acknowledged(
        &mut self,
        observations: &[bcode_harness_models::HarnessObservation],
    ) {
        for observation in observations {
            let bcode_harness_models::HarnessObservationKind::Execution { event } =
                &observation.kind
            else {
                continue;
            };
            match event {
                HarnessExecutionEvent::BackgroundWork {
                    work_id,
                    state,
                    notification,
                    ..
                } => {
                    // Responsibility remains in canonical storage, including nonterminal
                    // work, pending notification and failure. Evict only the exact version.
                    self.work.retain(|_, work| {
                        work.id != *work_id
                            || work.state != *state
                            || work_notification(work) != *notification
                    });
                }
                HarnessExecutionEvent::SubmissionTerminal {
                    native_turn_id: Some(id),
                    ..
                } => {
                    if self.turns.get(id).is_some_and(|turn| turn.terminal) {
                        self.turns.remove(id);
                        self.retired_contexts = true;
                    }
                }
                HarnessExecutionEvent::Continuation {
                    continuation_id,
                    native_turn_id: Some(id),
                    state,
                    output,
                    ..
                } if self.turns.get(id).is_some_and(|turn| {
                    !turn.foreground
                        && turn.continuation == *continuation_id
                        && turn.terminal == state.is_terminal()
                        && turn.output == *output
                }) =>
                {
                    self.turns.remove(id);
                    self.retired_contexts = true;
                }
                _ => {}
            }
        }
    }

    pub(super) fn hydrate(
        &mut self,
        observations: &[bcode_harness_models::HarnessObservation],
    ) -> Result<(), CodexError> {
        use bcode_harness_models::HarnessObservationKind;
        for observation in observations {
            match &observation.kind {
                HarnessObservationKind::SubmissionAccepted {
                    submission_id,
                    attempt_id,
                    native_turn_id,
                } => {
                    if !self.turns.contains_key(native_turn_id) {
                        capacity(self.retained_count())?;
                        self.turns.insert(
                            native_turn_id.clone(),
                            Turn {
                                attempt: Some(HarnessAttemptIdentity {
                                    submission_id: submission_id.clone(),
                                    attempt_id: attempt_id.clone(),
                                }),
                                steered_attempts: Vec::new(),
                                continuation: uuid::Uuid::new_v4().to_string(),
                                output: None,
                                unphased_output: None,
                                terminal: false,
                                foreground: true,
                            },
                        );
                    }
                }
                HarnessObservationKind::Execution { event } => self.hydrate_execution(event)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn hydrate_execution(&mut self, event: &HarnessExecutionEvent) -> Result<(), CodexError> {
        match event {
            HarnessExecutionEvent::BackgroundWork {
                work_id,
                native_task_id: Some(native),
                origin,
                work_kind,
                state,
                notification,
                ..
            } => {
                if let Some(work) = self.work.values().find(|work| work.id == *work_id) {
                    if work.state.is_terminal() && work.state != *state {
                        return Err(CodexError::Stale);
                    }
                    return Ok(());
                }
                capacity(self.retained_count())?;
                self.work.insert(
                    format!("canonical:{work_id}"),
                    Work {
                        id: work_id.clone(),
                        origin: origin.clone(),
                        native_task: native.clone(),
                        kind: *work_kind,
                        state: *state,
                        notification: Some(*notification),
                    },
                );
            }
            HarnessExecutionEvent::SubmissionTerminal {
                native_turn_id: Some(id),
                output,
                ..
            } => {
                if let Some(turn) = self.turns.get_mut(id)
                    && !turn.terminal
                {
                    turn.terminal = true;
                    turn.output.clone_from(output);
                }
            }
            HarnessExecutionEvent::Continuation {
                continuation_id,
                origin,
                native_turn_id: Some(id),
                state,
                output,
                ..
            } => {
                if let Some(turn) = self.turns.get(id) {
                    if turn.terminal {
                        return Ok(());
                    }
                    if turn.continuation != *continuation_id {
                        return Err(CodexError::Stale);
                    }
                }
                if !self.turns.contains_key(id) {
                    capacity(self.retained_count())?;
                }
                self.turns.insert(
                    id.clone(),
                    Turn {
                        attempt: origin.clone(),
                        steered_attempts: Vec::new(),
                        continuation: continuation_id.clone(),
                        output: output.clone(),
                        unphased_output: None,
                        terminal: state.is_terminal(),
                        foreground: false,
                    },
                );
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn bind_reference(
        &mut self,
        reference: &bcode_harness_models::HarnessNativeReference,
        observations: &[bcode_harness_models::HarnessObservation],
    ) {
        if reference.kind != bcode_harness_models::HarnessNativeReferenceKind::Item {
            return;
        }
        let Some(turn) = &reference.native_turn_id else {
            return;
        };
        let id = observations
            .iter()
            .rev()
            .find_map(|observation| match &observation.kind {
                bcode_harness_models::HarnessObservationKind::Execution {
                    event: HarnessExecutionEvent::BackgroundWork { work_id, .. },
                } => Some(work_id),
                _ => None,
            });
        if let Some(id) = id {
            let key = self
                .work
                .iter()
                .find_map(|(key, work)| (work.id == *id).then(|| key.clone()));
            if let Some(key) = key
                && let Some(work) = self.work.remove(&key)
            {
                self.work
                    .insert(format!("{turn}:{}", reference.native_id), work);
            }
        }
    }

    #[must_use]
    pub(super) fn has_native_work(&self, native: &str) -> bool {
        self.work.values().any(|work| work.native_task == native)
    }

    pub(super) fn accept(
        &mut self,
        turn: &str,
        submission: &HarnessSubmission,
    ) -> Result<(), CodexError> {
        // Do not retire this exact context to turn a stale same-turn attempt into a
        // fresh acceptance. Mutation admission and retained native ACK remain required.
        if self.terminal_turn(turn) {
            return Err(CodexError::Stale);
        }
        self.retire_completed_foreground();
        capacity(self.retained_count())?;
        if let Some(context) = self.turns.get_mut(turn) {
            if context.terminal {
                return Err(CodexError::Stale);
            }
            context.output = None;
            capacity(context.steered_attempts.len())?;
            context.steered_attempts.push(HarnessAttemptIdentity {
                submission_id: submission.submission_id.clone(),
                attempt_id: submission.attempt_id.clone(),
            });
            return Ok(());
        }
        capacity(self.turns.len())?;
        self.turns.insert(
            turn.to_owned(),
            Turn {
                attempt: Some(HarnessAttemptIdentity {
                    submission_id: submission.submission_id.clone(),
                    attempt_id: submission.attempt_id.clone(),
                }),
                steered_attempts: Vec::new(),
                continuation: uuid::Uuid::new_v4().to_string(),
                output: None,
                unphased_output: None,
                terminal: false,
                foreground: true,
            },
        );
        Ok(())
    }

    #[must_use]
    pub(super) fn tracks_turn(&self, id: &str) -> bool {
        self.turns.contains_key(id)
    }

    #[must_use]
    pub(super) fn terminal_turn(&self, id: &str) -> bool {
        self.turns.get(id).is_some_and(|turn| turn.terminal)
    }

    pub(super) fn inventory(
        &mut self,
        response: &Value,
    ) -> Result<Vec<HarnessExecutionEvent>, CodexError> {
        let data = response["data"].as_array().ok_or(CodexError::Protocol)?;
        if data.len() > MAX_HARNESS_EXECUTION_ITEMS {
            return Err(CodexError::Bounds);
        }
        let mut events = Vec::new();
        for terminal in data {
            let process = native_id(&terminal["processId"])?;
            let item = native_id(&terminal["itemId"])?;
            let candidates = self
                .work
                .iter()
                .filter(|(key, work)| {
                    work.kind == HarnessWorkKind::Command
                        && (work.native_task == process || key.ends_with(&format!(":{item}")))
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            let existing = if candidates.len() == 1 {
                candidates.first().cloned()
            } else {
                None
            };
            let key = existing.unwrap_or_else(|| format!("inventory:{process}"));
            let work_count = self.retained_count();
            if let Entry::Vacant(entry) = self.work.entry(key.clone()) {
                capacity(work_count)?;
                entry.insert(Work {
                    id: uuid::Uuid::new_v4().to_string(),
                    origin: None,
                    native_task: process.to_owned(),
                    kind: HarnessWorkKind::Command,
                    state: HarnessWorkState::Running,
                    notification: None,
                });
            }
            let work = self.work.get_mut(&key).ok_or(CodexError::Protocol)?;
            if !work.state.is_terminal() {
                work.state = HarnessWorkState::Running;
                events.push(work_event(work));
            }
        }
        // Each RPC page is a fresh native inventory, not an atomic snapshot. Absence
        // cannot terminalize a retained process or discharge its notification.
        for event in &events {
            event.validate().map_err(|_| CodexError::Protocol)?;
        }
        Ok(events)
    }

    pub(super) fn observe(
        &mut self,
        method: &str,
        params: &Value,
    ) -> Result<Vec<HarnessExecutionEvent>, CodexError> {
        let mut events = Vec::new();
        match method {
            "turn/started" => self.started(params, &mut events)?,
            "turn/completed" => self.completed(params, &mut events)?,
            "item/started" | "item/completed" => self.item(method, params, &mut events)?,
            _ => {}
        }
        for event in &events {
            event.validate().map_err(|_| CodexError::Protocol)?;
        }
        Ok(events)
    }

    fn started(
        &mut self,
        params: &Value,
        events: &mut Vec<HarnessExecutionEvent>,
    ) -> Result<(), CodexError> {
        let id = native_id(&params["turn"]["id"])?;
        if self.retired_contexts
            && !self.turns.contains_key(id)
            && !params["turn"]["rootTurnId"]
                .as_str()
                .is_some_and(|root| self.turns.contains_key(root))
        {
            // With bounded historical correlation, an unlinked start may be a stale
            // replay. Keep a session-wide Unknown obligation, not a reopened native
            // turn or a fabricated association to the most recent submission.
            if self.uncorrelated_continuation.is_none() {
                capacity(self.retained_count())?;
                let continuation_id = uuid::Uuid::new_v4().to_string();
                events.push(HarnessExecutionEvent::Continuation {
                    continuation_id: continuation_id.clone(),
                    origin: None,
                    source_work_id: None,
                    native_turn_id: None,
                    state: HarnessWorkState::Unknown,
                    output: None,
                });
                self.uncorrelated_continuation = Some(continuation_id);
            }
            return Ok(());
        }
        let turn_count = self.retained_count();
        let origin = params["turn"]["rootTurnId"]
            .as_str()
            .and_then(|root| self.turns.get(root))
            .and_then(|turn| {
                if turn.steered_attempts.is_empty() {
                    turn.attempt.clone()
                } else {
                    None
                }
            });
        if let Entry::Vacant(entry) = self.turns.entry(id.to_owned()) {
            capacity(turn_count)?;
            let turn = entry.insert(Turn {
                attempt: origin,
                steered_attempts: Vec::new(),
                continuation: uuid::Uuid::new_v4().to_string(),
                output: None,
                unphased_output: None,
                terminal: false,
                foreground: false,
            });
            // Root lineage can establish an attempt, not a particular source work item.
            events.push(continuation(id, turn, HarnessWorkState::Running));
        }
        if let Some(attempt) = &self.turns[id].attempt {
            events.push(coverage(attempt));
        }
        Ok(())
    }

    fn completed(
        &mut self,
        params: &Value,
        events: &mut Vec<HarnessExecutionEvent>,
    ) -> Result<(), CodexError> {
        let id = native_id(&params["turn"]["id"])?;
        // Reconciliation must not adopt an unobserved native turn from terminal text.
        let Some(turn) = self.turns.get_mut(id) else {
            return Ok(());
        };
        if turn.terminal {
            return Ok(());
        }
        let disposition = match params["turn"]["status"].as_str() {
            Some("completed") if params["turn"]["error"].is_null() => {
                HarnessTerminalDisposition::Completed
            }
            Some("failed") => HarnessTerminalDisposition::Failed,
            Some("interrupted") => HarnessTerminalDisposition::Interrupted,
            _ => return Err(CodexError::Protocol),
        };
        turn.terminal = true;
        if disposition == HarnessTerminalDisposition::Completed && turn.output.is_none() {
            turn.output = turn.unphased_output.take();
        }
        if let Some(attempt) = &turn.attempt {
            // Successful foreground output must be a complete final-answer item.
            // Failed/interrupted outcomes remain reportable without such output.
            if turn.foreground
                && (disposition != HarnessTerminalDisposition::Completed || turn.output.is_some())
            {
                events.push(HarnessExecutionEvent::SubmissionTerminal {
                    attempt: attempt.clone(),
                    native_turn_id: Some(id.to_owned()),
                    native_frame_id: None,
                    disposition,
                    output: turn.output.clone(),
                });
            }
            events.push(coverage(attempt));
        }
        if turn.foreground {
            for attempt in &turn.steered_attempts {
                if disposition != HarnessTerminalDisposition::Completed || turn.output.is_some() {
                    events.push(HarnessExecutionEvent::SubmissionTerminal {
                        attempt: attempt.clone(),
                        native_turn_id: Some(id.to_owned()),
                        native_frame_id: None,
                        disposition,
                        output: turn.output.clone(),
                    });
                }
                events.push(coverage(attempt));
            }
        }
        if !turn.foreground {
            let state = match disposition {
                HarnessTerminalDisposition::Completed => HarnessWorkState::Completed,
                HarnessTerminalDisposition::Failed => HarnessWorkState::Failed,
                HarnessTerminalDisposition::Interrupted => HarnessWorkState::Interrupted,
            };
            events.push(continuation(id, turn, state));
        }
        Ok(())
    }

    fn agents(
        &mut self,
        params: &Value,
        events: &mut Vec<HarnessExecutionEvent>,
    ) -> Result<(), CodexError> {
        let item = &params["item"];
        let origin = params["turnId"]
            .as_str()
            .and_then(|id| self.turns.get(id))
            .and_then(|turn| turn.attempt.clone());
        let states = item["agentsStates"].as_object();
        let receivers = item["receiverThreadIds"]
            .as_array()
            .ok_or(CodexError::Protocol)?;
        for receiver in receivers {
            let id = native_id(receiver)?;
            let key = self
                .work
                .iter()
                .find_map(|(key, work)| {
                    (work.kind == HarnessWorkKind::Subagent && work.native_task == id)
                        .then(|| key.clone())
                })
                .unwrap_or_else(|| format!("agent:{id}"));
            let work_count = self.retained_count();
            if let Entry::Vacant(entry) = self.work.entry(key.clone()) {
                capacity(work_count)?;
                entry.insert(Work {
                    id: uuid::Uuid::new_v4().to_string(),
                    origin: origin.clone(),
                    native_task: id.to_owned(),
                    kind: HarnessWorkKind::Subagent,
                    state: HarnessWorkState::Pending,
                    notification: None,
                });
            }
            let work = self.work.get_mut(&key).ok_or(CodexError::Protocol)?;
            if work.state.is_terminal() {
                continue;
            }
            let state = states.and_then(|states| states.get(id));
            work.state = match state.and_then(|state| state["status"].as_str()) {
                Some("pendingInit") => HarnessWorkState::Pending,
                Some("running") => HarnessWorkState::Running,
                Some("completed") => HarnessWorkState::Completed,
                Some("errored") => HarnessWorkState::Failed,
                Some("interrupted") => HarnessWorkState::Interrupted,
                _ => HarnessWorkState::Unknown,
            };
            events.push(work_event(work));
        }
        Ok(())
    }

    fn item(
        &mut self,
        method: &str,
        params: &Value,
        events: &mut Vec<HarnessExecutionEvent>,
    ) -> Result<(), CodexError> {
        let item = &params["item"];
        let turn_id = params["turnId"].as_str();
        if item["type"] == "agentMessage" {
            let phase = item["phase"].as_str();
            if method == "item/completed"
                && matches!(phase, Some("final_answer") | None)
                && let Some(turn) = turn_id.and_then(|id| self.turns.get_mut(id))
                && !turn.terminal
            {
                let output = item["text"].as_str().ok_or(CodexError::Protocol)?;
                if output.len() > MAX_HARNESS_TEXT_BYTES {
                    return Err(CodexError::Bounds);
                }
                if phase.is_some() {
                    turn.output = Some(output.to_owned());
                } else {
                    // Models/providers without message phases never label a final answer;
                    // the last unphased message is the answer only if no final answer
                    // arrives before successful native completion. Commentary never is.
                    turn.unphased_output = Some(output.to_owned());
                }
            }
            return Ok(());
        }
        let kind = match item["type"].as_str() {
            Some("commandExecution") => HarnessWorkKind::Command,
            Some("collabAgentToolCall") => {
                return self.agents(params, events);
            }
            Some("mcpToolCall" | "dynamicToolCall") => HarnessWorkKind::Tool,
            _ => return Ok(()),
        };
        let id = native_id(&item["id"])?;
        let key = self
            .work
            .iter()
            .find_map(|(key, work)| {
                let process = item["processId"].as_str();
                (work.kind == kind
                    && (work.native_task == id || process.is_some_and(|p| work.native_task == p)))
                .then(|| key.clone())
            })
            .unwrap_or_else(|| format!("codex-work:{}:{id}", turn_id.unwrap_or("unknown")));
        let work_count = self.retained_count();
        if let Entry::Vacant(entry) = self.work.entry(key.clone()) {
            capacity(work_count)?;
            let origin = turn_id.and_then(|id| self.turns.get(id)).and_then(|turn| {
                if turn.steered_attempts.is_empty() {
                    turn.attempt.clone()
                } else {
                    None
                }
            });
            // Deterministic work identity: acknowledged work is evicted from this bounded
            // tracker, and a later item update must continue the same canonical work rather
            // than open a second, never-terminated record.
            entry.insert(Work {
                id: if key.len() <= bcode_harness_models::MAX_HARNESS_ID_BYTES {
                    key.clone()
                } else {
                    uuid::Uuid::new_v4().to_string()
                },
                origin,
                // The native item id is stable for the item's whole lifecycle; the PTY
                // process id appears only once the command runs.
                native_task: id.to_owned(),
                kind,
                state: HarnessWorkState::Pending,
                notification: None,
            });
        }
        let work = self.work.get_mut(&key).ok_or(CodexError::Protocol)?;
        if work.state.is_terminal() {
            return Ok(());
        }
        work.state = match item["status"].as_str() {
            Some("inProgress") => HarnessWorkState::Running,
            Some("completed") => HarnessWorkState::Completed,
            Some("failed" | "declined") => HarnessWorkState::Failed,
            _ => HarnessWorkState::Unknown,
        };
        // Turn root lineage may prove an attempt, but not a particular source task.
        events.push(work_event(work));
        Ok(())
    }
}

fn native_id(value: &Value) -> Result<&str, CodexError> {
    value
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or(CodexError::Protocol)
}
const fn capacity(len: usize) -> Result<(), CodexError> {
    if len >= MAX_HARNESS_EXECUTION_ITEMS {
        Err(CodexError::Bounds)
    } else {
        Ok(())
    }
}
fn coverage(attempt: &HarnessAttemptIdentity) -> HarnessExecutionEvent {
    // Item streams do not cover every native detached process, agent wakeup or
    // notification queue. Complete terminal inventory is not comprehensive coverage.
    HarnessExecutionEvent::Coverage {
        attempt: attempt.clone(),
        background: HarnessTrackingCoverage::Unknown,
        continuations: HarnessTrackingCoverage::Unknown,
    }
}
fn work_notification(work: &Work) -> HarnessNotificationState {
    work.notification.unwrap_or(
        if work.kind == HarnessWorkKind::Command || work.kind == HarnessWorkKind::Subagent {
            HarnessNotificationState::Pending
        } else {
            HarnessNotificationState::Unknown
        },
    )
}
fn work_event(work: &Work) -> HarnessExecutionEvent {
    HarnessExecutionEvent::BackgroundWork {
        work_id: work.id.clone(),
        native_task_id: Some(work.native_task.clone()),
        origin: work.origin.clone(),
        parent_work_id: None,
        work_kind: work.kind,
        label: "Codex native activity".into(),
        state: work.state,
        notification: work_notification(work),
    }
}
fn continuation(id: &str, turn: &Turn, state: HarnessWorkState) -> HarnessExecutionEvent {
    HarnessExecutionEvent::Continuation {
        continuation_id: turn.continuation.clone(),
        origin: turn.attempt.clone(),
        source_work_id: None,
        native_turn_id: Some(id.to_owned()),
        state,
        output: turn.output.clone(),
    }
}
