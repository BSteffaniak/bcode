//! Normal-turn downstream capture, separate from producer service microbenchmarks.
use super::{CaptureScope, CaptureStatus, ConsumerCapture};
use crate::judgement_benchmark::{ConsumerOutput, TerminalOutcome};
use bcode_client::BcodeClient;
use bcode_ipc::Event;
use bcode_model::judgement::Feature;
use bcode_session_models::{
    ModelTurnOutcome, SessionEventKind, SessionId, SessionLiveEventKind,
    ToolContributionPersistence, TurnAdmission, TurnAdmissionMetadata,
};
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// Caller supplies an isolated, fresh harness-owned session; no session history is replayed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalTurnRequest {
    /// Existing isolated session in the selected subject/state context.
    pub session_id: SessionId,
    /// Synthetic task input, never copied to capture output; normal user-message persistence applies.
    pub prompt: String,
    /// Immutable execution options, including ordinary model identity/allowlist/turn budget.
    pub admission: TurnAdmissionMetadata,
    /// Preregistered maximum events; excess is explicit infrastructure failure, not silent truncation.
    pub max_events: usize,
    /// Session classification dimension, when that feature is selected.
    pub categorization_dimension: super::CategorizationDimension,
}

/// Convert only a closed exact-turn product snapshot, not a fixture ranking or oracle.
///
/// Source IDs are the admitted turn's original history references in producer order.
/// If mandatory-context deduplication changes that snapshot, count mismatch rejects
/// rather than guessing ordinals. Ranking is admission evidence, not task correctness.
fn context_output(
    observation: &bcode_model::judgement::TurnJudgementObservation,
    session_id: &str,
    turn_id: &str,
    outcome: TerminalOutcome,
    arm: crate::judgement_benchmark::Arm,
    source_ids: &[String],
) -> Result<Option<ConsumerOutput>, ()> {
    use bcode_model::judgement::{
        ApplicationDisposition as Application, ObservationScope, ObservationTerminal,
        TriggerDisposition as Trigger,
    };
    observation.validate().map_err(|_| ())?;
    let terminal = match outcome {
        TerminalOutcome::Succeeded => ObservationTerminal::Completed,
        TerminalOutcome::Failed => ObservationTerminal::Failed,
        TerminalOutcome::Cancelled => ObservationTerminal::Cancelled,
        _ => return Err(()),
    };
    if observation.scope != ObservationScope::NormalTurn
        || observation.session_id != session_id
        || observation.turn_id != turn_id
        || observation.terminal != Some(terminal)
        || !observation.complete
        || observation.omitted != 0
    {
        return Err(());
    }
    let mut operations = observation
        .operations
        .iter()
        .filter(|operation| operation.feature == Feature::ContextReranking);
    let operation = operations.next().ok_or(())?;
    if operations.next().is_some() {
        return Err(());
    }
    if operation.trigger == Trigger::NotApplicable {
        return if operation.selection.is_none() {
            Ok(None)
        } else {
            Err(())
        };
    }
    if !matches!(
        operation.application,
        Some(Application::Applied | Application::BaselineRetained)
    ) || terminal == ObservationTerminal::Cancelled
    {
        return Err(());
    }
    if arm == crate::judgement_benchmark::Arm::Off
        && (operation.trigger != Trigger::Disabled
            || operation.application != Some(Application::BaselineRetained)
            || !operation.inference.is_empty())
    {
        return Err(());
    }
    if arm == crate::judgement_benchmark::Arm::On && operation.trigger != Trigger::Triggered {
        return Err(());
    }
    let selection = operation.selection.as_ref().ok_or(())?;
    if selection.candidate_count == 0
        || selection.candidate_count != source_ids.len()
        || source_ids
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != source_ids.len()
    {
        return Err(());
    }
    Ok(Some(ConsumerOutput::Ranking {
        order: selection
            .ordered
            .iter()
            .map(|ordinal| source_ids[*ordinal].clone())
            .collect(),
        admitted: selection
            .admitted
            .iter()
            .map(|ordinal| source_ids[*ordinal].clone())
            .collect(),
    }))
}

pub(super) async fn execute(
    client: &BcodeClient,
    input: NormalTurnRequest,
    capture: &mut ConsumerCapture,
) -> Result<(), ()> {
    capture.scope = CaptureScope::NormalTurn;
    capture.operation = "normal_turn.submit+live_contributions+terminal".into();
    if input.prompt.is_empty()
        || input.prompt.len() > 8192
        || input.max_events == 0
        || input.max_events > 4096
    {
        return Err(());
    }
    if !matches!(
        capture.request.feature,
        Feature::ToolResultTriage
            | Feature::ProgressMonitoring
            | Feature::SessionCategorization
            | Feature::Recommendations
            | Feature::ContextReranking
    ) {
        return Err(());
    }
    if capture.request.feature == Feature::Recommendations {
        // Use the public service microbench for this feature until normal-turn final catalog
        // ordering has a bounded public observation. Do not spend on an uncapturable turn.
        return Err(());
    }
    let context_ids: Vec<_> = input
        .admission
        .execution
        .historical_context
        .iter()
        .map(|reference| format!("history:{}:{}", reference.session_id, reference.sequence))
        .collect();
    // Subscribe before admission: transient contributions cannot be recovered by canonical replay.
    let mut connection = client.connect("judgement-capture").await.map_err(|_| ())?;
    connection
        .attach_session_recent(input.session_id, 1)
        .await
        .map_err(|_| ())?;
    let started = Instant::now();
    let TurnAdmission::Accepted(receipt) = client
        .submit_turn(input.session_id, input.prompt, input.admission)
        .await
        .map_err(|_| ())?
    else {
        // Reused/deferred work cannot be treated as this trial's freshly observed execution.
        return Err(());
    };
    let mut label = None;
    let mut detected_ms = None;
    for _ in 0..input.max_events {
        match connection
            .recv_event_without_reconnect()
            .await
            .map_err(|_| ())?
        {
            Event::SessionLive(event) if event.session_id == input.session_id => {
                if let SessionLiveEventKind::ToolContributionPlaced { envelope } = event.kind {
                    if envelope.schema_version != 1
                        || envelope.contribution.schema_version != 1
                        || envelope.contribution.persistence
                            != ToolContributionPersistence::Transient
                    {
                        continue;
                    }
                    let contribution = envelope.contribution;
                    let payload = contribution.payload;
                    match capture.request.feature {
                        Feature::ToolResultTriage
                            if contribution.schema == "bcode.loop.diagnostic-advisory/v1" =>
                        {
                            // Only fixed plugin categories are portable evidence. Never retain
                            // arbitrary strings supplied by an untrusted contribution producer.
                            label = payload["category"]
                                .as_str()
                                .filter(|value| {
                                    matches!(
                                        *value,
                                        "code"
                                            | "environment"
                                            | "auth"
                                            | "transient"
                                            | "partial"
                                            | "inconclusive"
                                    )
                                })
                                .map(str::to_owned);
                        }
                        Feature::ProgressMonitoring
                            if contribution.schema == "bcode.loop.progress-monitoring/v1"
                                && label.is_none() =>
                        {
                            label = match payload["kind"].as_str() {
                                Some("repeated_failure") => Some("stall".into()),
                                Some("drift") => Some("drift".into()),
                                _ => None,
                            };
                            if label.is_some() {
                                detected_ms = u64::try_from(started.elapsed().as_millis()).ok();
                            }
                        }
                        _ => {}
                    }
                }
            }
            Event::Session(event) if event.session_id == input.session_id => {
                if let SessionEventKind::ModelTurnFinished {
                    turn_id, outcome, ..
                } = event.kind
                {
                    if turn_id != receipt.turn_id.0 {
                        continue;
                    }
                    // Tool counts prove eligible activity, not that an optional consumer ran.
                    // Until the stream exposes activation-bound decision facts, only an observed
                    // contribution establishes execution; silence (including disabled) is unknown.
                    capture.consumer_executed = match capture.request.feature {
                        Feature::ToolResultTriage | Feature::ProgressMonitoring => label.is_some(),
                        _ => false,
                    };
                    capture.trial_latency_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
                    capture.outcome = match outcome {
                        ModelTurnOutcome::Completed => TerminalOutcome::Succeeded,
                        ModelTurnOutcome::Cancelled => TerminalOutcome::Cancelled,
                        _ => TerminalOutcome::Failed,
                    };
                    capture.output = match capture.request.feature {
                        Feature::ToolResultTriage => Some(ConsumerOutput::Classification { label }),
                        Feature::ProgressMonitoring => {
                            Some(ConsumerOutput::Progress { label, detected_ms })
                        }
                        Feature::SessionCategorization => {
                            let labels = client
                                .session_categorization(input.session_id)
                                .await
                                .map_err(|_| ())?;
                            // The advisory result must belong to this activation, not a stale disposable cache entry.
                            let label = labels
                                .filter(|value| {
                                    value.source_sequence == receipt.accepted_event_sequence
                                })
                                .map(|value| match input.categorization_dimension {
                                    super::CategorizationDimension::Task => value.task,
                                    super::CategorizationDimension::Domain => value.domain,
                                });
                            capture.consumer_executed = label.is_some();
                            Some(ConsumerOutput::Classification { label })
                        }
                        // Current public stream lacks the final available-tool permutation. The turn ran,
                        // but raw provider requests must not be exposed or inferred from model prose.
                        Feature::Recommendations | Feature::ContextReranking => None,
                        Feature::EvalCategorization => return Err(()),
                    };
                    let mut product_observation = None;
                    // A terminal event may be published just before final producer closure.
                    // Poll only this exact identity; absence remains unknown, never current-turn fallback.
                    for _ in 0..5 {
                        product_observation = client
                            .turn_judgement_observation(input.session_id, &receipt.turn_id.0)
                            .await
                            .map_err(|_| ())?;
                        if product_observation
                            .as_ref()
                            .is_some_and(|value| value.terminal.is_some())
                        {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                    if let Some(observation) = product_observation {
                        if capture.request.feature == Feature::ContextReranking {
                            capture.output = context_output(
                                &observation,
                                &input.session_id.to_string(),
                                &receipt.turn_id.0,
                                capture.outcome,
                                capture.request.arm,
                                &context_ids,
                            )?;
                            capture.consumer_executed = capture.output.is_some();
                        }
                        capture.telemetry.retain_turn_observation(&observation)?;
                        // NotApplicable and Disabled are not proof of consumer execution.
                        capture.consumer_executed |= capture.request.feature
                            != Feature::ContextReranking
                            && observation.operations.iter().any(|operation| {
                                operation.feature == capture.request.feature
                                    && operation.trigger
                                        == bcode_model::judgement::TriggerDisposition::Triggered
                                    && operation.application.is_some()
                            });
                    }
                    capture.status = if capture.output.is_some() && capture.consumer_executed {
                        CaptureStatus::Captured
                    } else {
                        // Silence cannot distinguish baseline retention, disabled treatment,
                        // an ineligible trigger, or a producer with missing observations.
                        CaptureStatus::FallbackReasonUnknown
                    };
                    return Ok(());
                }
            }
            // A reconnect/resync loses transient coverage; never claim absence from a partial stream.
            Event::SessionViewResyncRequired { .. } => return Err(()),
            _ => {}
        }
    }
    Err(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judgement_benchmark::Arm;
    use bcode_model::judgement::{
        ApplicationDisposition as Application, ConsumerOperationObservation, ObservationScope,
        ObservationTerminal, SelectionObservation, TriggerDisposition as Trigger,
        TurnJudgementObservation,
    };

    fn snapshot() -> TurnJudgementObservation {
        let features = [
            Feature::ToolResultTriage,
            Feature::ContextReranking,
            Feature::Recommendations,
            Feature::ProgressMonitoring,
            Feature::SessionCategorization,
        ];
        TurnJudgementObservation {
            schema_version: bcode_model::judgement::TURN_OBSERVATION_VERSION,
            scope: ObservationScope::NormalTurn,
            session_id: "session".into(),
            turn_id: "turn".into(),
            operations: features
                .into_iter()
                .enumerate()
                .map(|(index, feature)| ConsumerOperationObservation {
                    operation_id: index as u64,
                    feature,
                    trigger: Trigger::Disabled,
                    inference: Vec::new(),
                    inference_complete: true,
                    application: Some(Application::BaselineRetained),
                    selection: (feature == Feature::ContextReranking).then_some(
                        SelectionObservation {
                            candidate_count: 2,
                            ordered: vec![0, 1],
                            admitted: vec![0],
                        },
                    ),
                })
                .collect(),
            covered_features: features.into_iter().collect(),
            terminal: Some(ObservationTerminal::Completed),
            omitted: 0,
            complete: true,
        }
    }
    fn convert(
        snapshot: &TurnJudgementObservation,
        arm: Arm,
    ) -> Result<Option<ConsumerOutput>, ()> {
        context_output(
            snapshot,
            "session",
            "turn",
            TerminalOutcome::Succeeded,
            arm,
            &["history:session:2".into(), "history:session:7".into()],
        )
    }

    #[test]
    fn disabled_baseline_and_on_final_selection_use_original_source_ordinals() {
        let mut snapshot = snapshot();
        let Some(ConsumerOutput::Ranking { order, admitted }) =
            convert(&snapshot, Arm::Off).unwrap()
        else {
            panic!("missing baseline selection");
        };
        assert_eq!(order, ["history:session:2", "history:session:7"]);
        assert_eq!(admitted, ["history:session:2"]);
        let operation = &mut snapshot.operations[1];
        operation.trigger = Trigger::Triggered;
        operation.application = Some(Application::Applied);
        operation
            .inference
            .push(bcode_model::judgement::OptionalOperationObservation {
                inference_invocations: 1,
                ..Default::default()
            });
        operation.selection = Some(SelectionObservation {
            candidate_count: 2,
            ordered: vec![1, 0],
            admitted: vec![1],
        });
        let Some(ConsumerOutput::Ranking { order, admitted }) =
            convert(&snapshot, Arm::On).unwrap()
        else {
            panic!("missing installed selection");
        };
        assert_eq!(order, ["history:session:7", "history:session:2"]);
        assert_eq!(admitted, ["history:session:7"]);
        assert!(convert(&snapshot, Arm::Off).is_err());
    }

    #[test]
    fn no_prerequisite_is_not_execution_and_unavailable_is_not_success() {
        let mut snapshot = snapshot();
        snapshot.operations[1].trigger = Trigger::NotApplicable;
        snapshot.operations[1].selection = None;
        assert!(convert(&snapshot, Arm::On).unwrap().is_none());
        snapshot.operations[1].trigger = Trigger::Disabled;
        assert!(convert(&snapshot, Arm::Off).is_err());
        snapshot.operations[1].application = Some(Application::Unavailable);
        assert!(convert(&snapshot, Arm::Off).is_err());
    }

    #[test]
    fn partial_wrong_turn_future_and_cancelled_observations_fail_closed() {
        let mutations: [fn(&mut TurnJudgementObservation); 9] = [
            |value| value.complete = false,
            |value| value.omitted = 1,
            |value| value.session_id = "other-session".into(),
            |value| value.turn_id = "other-turn".into(),
            |value| value.schema_version += 1,
            |value| value.terminal = None,
            |value| {
                value.operations[1]
                    .selection
                    .as_mut()
                    .unwrap()
                    .candidate_count = 3;
            },
            |value| value.operations[1].selection.as_mut().unwrap().ordered = vec![0, 0],
            |value| value.operations[1].application = Some(Application::Cancelled),
        ];
        for mutate in mutations {
            let mut value = snapshot();
            mutate(&mut value);
            assert!(convert(&value, Arm::Off).is_err());
        }
        let mut value = snapshot();
        value.terminal = Some(ObservationTerminal::Cancelled);
        assert!(
            context_output(
                &value,
                "session",
                "turn",
                TerminalOutcome::Cancelled,
                Arm::Off,
                &["history:session:2".into(), "history:session:7".into()]
            )
            .is_err()
        );
        let mut serialized = serde_json::to_value(snapshot()).unwrap();
        serialized["operations"][1]["trigger"] = serde_json::json!("future_trigger");
        assert!(serde_json::from_value::<TurnJudgementObservation>(serialized).is_err());
    }
}
