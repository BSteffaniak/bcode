//! Canonical cache hydration. Acknowledgment moves storage, never discharges debt.
use super::{
    BTreeSet, CacheDebt, Event, Execution, Followup, HarnessContractError,
    MAX_HARNESS_EXECUTION_ITEMS, NativeMessage, Notification, Result, Tracker, Value, Work,
};
use bcode_harness_models::{
    HarnessHydrationReply, HarnessHydrationResolution, HarnessNativeReference,
    HarnessNativeReferenceKind, HarnessObservationKind,
};

fn matching_hydration_work(
    reply: &HarnessHydrationReply,
    observations: &[bcode_harness_models::HarnessObservation],
) -> BTreeSet<String> {
    let matching_work: BTreeSet<_> = observations
        .iter()
        .filter_map(|observation| match &observation.kind {
            HarnessObservationKind::Execution {
                event:
                    Execution::BackgroundWork {
                        work_id,
                        native_task_id: Some(_),
                        ..
                    },
            } if reply.reference.matches_observation(observation) => Some(work_id.clone()),
            _ => None,
        })
        .collect();
    matching_work
}

impl Tracker {
    pub(crate) fn acknowledge_record(
        &mut self,
        observation: &bcode_harness_models::HarnessObservation,
    ) {
        let HarnessObservationKind::Execution { event } = &observation.kind else {
            return;
        };
        match event {
            Execution::SubmissionTerminal { attempt, .. } => {
                self.acknowledged_terminals.insert(attempt.clone());
            }
            Execution::BackgroundWork { work_id, .. } => {
                // Only the exact latest revision may leave memory. Its terminal,
                // failure, pending notification and origin remain canonical.
                let key = self
                    .work
                    .iter()
                    .find(|(_, work)| work.id == *work_id && work.event() == *event)
                    .map(|(key, _)| key.clone());
                if let Some(key) = key {
                    if self.work.get(&key).is_some_and(|work| {
                        !work.state.is_terminal() || work.notification != Notification::NotExpected
                    }) {
                        // The canonical root, not this adapter cache, owns the debt.
                        // Without an explicit discharged-chain reply do not retire
                        // the attempt merely because its child left memory.
                        self.cache_debt = CacheDebt::CanonicalOutstanding;
                    }
                    if let Some(tool) = self.work.remove(&key).and_then(|work| work.tool) {
                        self.tools.remove(&tool);
                        self.pending_background_tools.remove(&tool);
                    }
                }
            }
            Execution::Continuation {
                continuation_id, ..
            } if self
                .followups
                .get(continuation_id)
                .is_some_and(|item| item.event() == *event) =>
            {
                if let Some(item) = self.followups.remove(continuation_id) {
                    if let Some(frame) = item.result_frame {
                        self.seen_results.remove(&frame);
                    }
                    if let Some(source) = item.work
                        && !self
                            .pending_publication
                            .contains_key(&format!("work:{source}"))
                    {
                        self.work.retain(|_, work| work.id != source);
                    }
                }
                if self.active_followup.as_ref() == Some(continuation_id) {
                    self.active_followup = None;
                }
            }
            _ => {}
        }
        let retired: Vec<_> = self
            .hydrated_origins
            .iter()
            .filter(|origin| {
                !self
                    .work
                    .values()
                    .any(|work| work.origin.as_ref() == Some(*origin))
                    && !self
                        .followups
                        .values()
                        .any(|item| item.origin.as_ref() == Some(*origin))
            })
            .cloned()
            .collect();
        for origin in retired {
            self.hydrated_origins.remove(&origin);
            self.attempts.remove(&origin);
        }
    }
    pub(crate) fn native_reference(&self, event: &Event) -> Option<HarnessNativeReference> {
        let value = match event {
            Event::Message(NativeMessage::System(value)) => value,
            Event::Message(
                NativeMessage::Assistant(value)
                | NativeMessage::Result(value)
                | NativeMessage::User(value),
            ) => {
                if value.get("type").and_then(Value::as_str) == Some("user")
                    && let Some(task) = value
                        .pointer("/tool_use_result/backgroundTaskId")
                        .or_else(|| value.pointer("/tool_use_result/background_task_id"))
                        .and_then(Value::as_str)
                    && !self.work.values().any(|work| work.task == task)
                {
                    let tools: Vec<_> = value
                        .pointer("/message/content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|block| {
                            block.get("type").and_then(Value::as_str) == Some("tool_result")
                        })
                        .filter_map(|block| block.get("tool_use_id").and_then(Value::as_str))
                        .collect();
                    if tools.len() == 1 {
                        return Some(HarnessNativeReference {
                            kind: HarnessNativeReferenceKind::Item,
                            native_id: tools[0].into(),
                            native_turn_id: None,
                        });
                    }
                    return Some(HarnessNativeReference {
                        kind: HarnessNativeReferenceKind::Process,
                        native_id: task.into(),
                        native_turn_id: None,
                    });
                }
                if let Some(run) = value.pointer("/origin/runId").and_then(Value::as_str)
                    && !self
                        .work
                        .values()
                        .any(|work| work.run.as_deref() == Some(run))
                {
                    return Some(HarnessNativeReference {
                        kind: HarnessNativeReferenceKind::Process,
                        native_id: run.into(),
                        native_turn_id: None,
                    });
                }
                return None;
            }
            _ => return None,
        };
        if !matches!(
            value.get("subtype").and_then(Value::as_str),
            Some("task_started" | "task_updated" | "task_notification")
        ) {
            return None;
        }
        let task = value.get("task_id")?.as_str()?;
        let key = Self::work_key(value).ok()?;
        if self.work.contains_key(&key) {
            return None;
        }
        Some(HarnessNativeReference {
            kind: if value
                .get("task_type")
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "local_agent" | "local_workflow"))
            {
                HarnessNativeReferenceKind::Agent
            } else {
                HarnessNativeReferenceKind::Process
            },
            native_id: task.into(),
            native_turn_id: None,
        })
    }
    pub(crate) fn hydrate(&mut self, reply: &HarnessHydrationReply) -> Result<()> {
        let mut candidate = self.clone();
        candidate.hydrate_checked(reply)?;
        *self = candidate;
        Ok(())
    }
    fn hydrate_checked(&mut self, reply: &HarnessHydrationReply) -> Result<()> {
        match &reply.resolution {
            HarnessHydrationResolution::Absent => Ok(()),
            HarnessHydrationResolution::Unavailable => {
                self.lose_coverage();
                Ok(())
            }
            HarnessHydrationResolution::Found { observations } => {
                if !matches!(
                    reply.reference.kind,
                    HarnessNativeReferenceKind::Process
                        | HarnessNativeReferenceKind::Agent
                        | HarnessNativeReferenceKind::Item
                ) {
                    return Err(HarnessContractError::InvalidShape);
                }
                let matching_work = matching_hydration_work(reply, observations);
                let mut found = false;
                for observation in observations {
                    let HarnessObservationKind::Execution { event } = &observation.kind else {
                        continue;
                    };
                    match event {
                        Execution::BackgroundWork {
                            work_id,
                            native_task_id: Some(task),
                            origin,
                            parent_work_id,
                            work_kind,
                            state,
                            notification,
                            ..
                        } if matching_work.contains(work_id) => {
                            // Native run metadata is retained separately, never in a native-turn field.
                            let run =
                                observation
                                    .native_correlations
                                    .iter()
                                    .find_map(|correlation| {
                                        correlation.strip_prefix("claude-run:").map(str::to_owned)
                                    });
                            let key = (task.clone(), run.clone());
                            let tool = observation.native_correlations.iter().find_map(|entry| {
                                entry.strip_prefix("claude-tool:").map(str::to_owned)
                            });
                            if let Some(tool) = &tool {
                                self.tools.insert(tool.clone(), origin.clone());
                            }
                            self.hydrate_origin(origin.as_ref(), observations)?;
                            if self.work.len() >= MAX_HARNESS_EXECUTION_ITEMS
                                && !self.work.contains_key(&key)
                            {
                                return Err(HarnessContractError::BoundsExceeded);
                            }
                            if let Some(existing) = self.work.get(&key)
                                && (existing.id != *work_id
                                    || (existing.state.is_terminal() && existing.state != *state)
                                    || existing
                                        .origin
                                        .as_ref()
                                        .is_some_and(|old| origin.as_ref() != Some(old)))
                            {
                                return Err(HarnessContractError::InvalidShape);
                            }
                            self.work.insert(
                                key,
                                Work {
                                    id: work_id.clone(),
                                    task: task.clone(),
                                    tool,
                                    run,
                                    origin: origin.clone(),
                                    parent: parent_work_id.clone(),
                                    kind: *work_kind,
                                    state: *state,
                                    notification: *notification,
                                },
                            );
                            found = true;
                        }
                        Execution::Continuation { source_work_id, .. }
                            if source_work_id
                                .as_ref()
                                .is_some_and(|id| matching_work.contains(id)) =>
                        {
                            self.hydrate_continuation(observation, event)?;
                        }
                        _ => {}
                    }
                }
                if found {
                    Ok(())
                } else {
                    self.lose_coverage();
                    Err(HarnessContractError::InvalidShape)
                }
            }
        }
    }
    fn hydrate_continuation(
        &mut self,
        observation: &bcode_harness_models::HarnessObservation,
        event: &Execution,
    ) -> Result<()> {
        let Execution::Continuation {
            continuation_id,
            origin,
            source_work_id,
            state,
            output,
            ..
        } = event
        else {
            return Err(HarnessContractError::InvalidShape);
        };
        if self.followups.len() >= MAX_HARNESS_EXECUTION_ITEMS
            && !self.followups.contains_key(continuation_id)
        {
            return Err(HarnessContractError::BoundsExceeded);
        }
        if self.followups.get(continuation_id).is_some_and(|old| {
            old.work != *source_work_id
                || (old.state.is_terminal() && (old.state != *state || old.output != *output))
        }) {
            return Err(HarnessContractError::InvalidShape);
        }
        self.followups.insert(
            continuation_id.clone(),
            Followup {
                id: continuation_id.clone(),
                origin: origin.clone(),
                work: source_work_id.clone(),
                state: *state,
                result_frame: observation
                    .native_correlations
                    .iter()
                    .find_map(|entry| entry.strip_prefix("claude-result:").map(str::to_owned)),
                output: output.clone(),
            },
        );
        Ok(())
    }

    fn hydrate_origin(
        &mut self,
        origin: Option<&bcode_harness_models::HarnessAttemptIdentity>,
        observations: &[bcode_harness_models::HarnessObservation],
    ) -> Result<()> {
        let Some(origin) = origin else {
            return Ok(());
        };
        let proven = self.attempts.contains(origin)
            || observations.iter().any(|item| match &item.kind {
                HarnessObservationKind::SubmissionAccepted {
                    submission_id,
                    attempt_id,
                    ..
                }
                | HarnessObservationKind::SubmissionConsumed {
                    submission_id,
                    attempt_id,
                    ..
                }
                | HarnessObservationKind::SubmissionResultObserved {
                    submission_id,
                    attempt_id,
                    ..
                } => submission_id == &origin.submission_id && attempt_id == &origin.attempt_id,
                HarnessObservationKind::Execution {
                    event: Execution::SubmissionTerminal { attempt, .. },
                } => attempt == origin,
                _ => false,
            });
        if !proven {
            return Err(HarnessContractError::InvalidShape);
        }
        if !self.attempts.contains(origin) {
            self.hydrated_origins.insert(origin.clone());
        }
        if self.attempts.len() >= MAX_HARNESS_EXECUTION_ITEMS && !self.attempts.contains(origin) {
            return Err(HarnessContractError::BoundsExceeded);
        }
        self.attempts.insert(origin.clone());
        Ok(())
    }

    pub(crate) fn native_correlations(&self, event: &Execution) -> Vec<String> {
        match event {
            Execution::BackgroundWork { work_id, .. } => self
                .work
                .values()
                .find(|work| &work.id == work_id)
                .map(|work| {
                    let mut references = Vec::new();
                    if let Some(run) = &work.run {
                        references.push(format!("claude-run:{run}"));
                    }
                    if let Some(tool) = &work.tool {
                        references.push(tool.clone());
                        references.push(format!("claude-tool:{tool}"));
                    }
                    references
                })
                .unwrap_or_default(),
            Execution::Continuation {
                continuation_id, ..
            } => self
                .followups
                .get(continuation_id)
                .and_then(|item| {
                    item.result_frame
                        .as_ref()
                        .map(|frame| format!("claude-result:{frame}"))
                })
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }
}
