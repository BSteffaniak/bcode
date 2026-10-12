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
        || !observation
            .covered_features
            .contains(&Feature::ContextReranking)
        || observation.omitted != 0
    {
        return Err(());
    }
    let operations: Vec<_> = observation
        .operations
        .iter()
        .filter(|operation| operation.feature == Feature::ContextReranking)
        .collect();
    let aggregates: Vec<_> = observation
        .passive_aggregates
        .iter()
        .filter(|op| op.feature == Feature::ContextReranking)
        .collect();
    if let Some(operation) = operations.first() {
        if aggregates.iter().any(|other| {
            other.trigger != operation.trigger
                || Some(other.application) != operation.application
                || operation.selection.is_some()
        }) {
            return Err(());
        }
    } else if !aggregates.is_empty() {
        return if aggregates
            .iter()
            .all(|op| op.trigger == Trigger::NotApplicable)
        {
            Ok(None)
        } else {
            Err(())
        };
    }
    let operation = operations.first().ok_or(())?;
    // The legacy output has one selection, not a trajectory. Multiple rounds
    // qualify only if every consumer decision agrees; never pick a favorable round.
    if operations.iter().skip(1).any(|other| {
        other.trigger != operation.trigger
            || other.application != operation.application
            || other.selection != operation.selection
    }) {
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

fn recommendation_output(
    observation: &bcode_model::judgement::TurnJudgementObservation,
    arm: crate::judgement_benchmark::Arm,
) -> Result<Option<ConsumerOutput>, ()> {
    use bcode_model::judgement::{
        ApplicationDisposition as Application, RecommendationDomain, TriggerDisposition as Trigger,
    };
    observation.validate().map_err(|_| ())?;
    if observation.terminal == Some(bcode_model::judgement::ObservationTerminal::Cancelled) {
        return Err(());
    }
    if observation
        .passive_aggregates
        .iter()
        .any(|op| op.feature == Feature::Recommendations && op.trigger != Trigger::NotApplicable)
    {
        // Detail-free disabled facts cannot stand in for a recommendation catalog.
        return Err(());
    }
    let mut selected = None;
    let mut disposition = None;
    for operation in observation
        .operations
        .iter()
        .filter(|operation| operation.feature == Feature::Recommendations)
    {
        if operation.trigger == Trigger::NotApplicable && operation.recommendation.is_none() {
            continue;
        }
        let recommendation = operation.recommendation.as_ref().ok_or(())?;
        if recommendation.domain != RecommendationDomain::Tools {
            continue;
        }
        if operation.selection.as_ref() != Some(&recommendation.selection)
            || !matches!(
                operation.application,
                Some(Application::Applied | Application::BaselineRetained)
            )
            || (arm == crate::judgement_benchmark::Arm::Off
                && (operation.trigger != Trigger::Disabled
                    || !operation.inference.is_empty()
                    || operation.application != Some(Application::BaselineRetained)))
            || (arm == crate::judgement_benchmark::Arm::On
                && operation.trigger != Trigger::Triggered)
        {
            return Err(());
        }
        if let Some(previous) = selected {
            // A single ranking cannot represent changed multi-round catalogs/decisions.
            // Keep those trajectories unknown rather than choosing a favorable round.
            if previous != recommendation
                || disposition != Some((operation.trigger, operation.application))
            {
                return Err(());
            }
        } else {
            selected = Some(recommendation);
            disposition = Some((operation.trigger, operation.application));
        }
    }
    Ok(selected.map(|recommendation| ConsumerOutput::Ranking {
        order: recommendation
            .selection
            .ordered
            .iter()
            .map(|ordinal| recommendation.candidate_ids[*ordinal].clone())
            .collect(),
        admitted: recommendation
            .selection
            .admitted
            .iter()
            .map(|ordinal| recommendation.candidate_ids[*ordinal].clone())
            .collect(),
    }))
}

fn disabled_baseline_observed(
    observation: &bcode_model::judgement::TurnJudgementObservation,
    feature: Feature,
) -> bool {
    let mut found = false;
    for operation in observation
        .operations
        .iter()
        .filter(|operation| operation.feature == feature)
    {
        found = true;
        if operation.trigger != bcode_model::judgement::TriggerDisposition::Disabled
            || operation.application
                != Some(bcode_model::judgement::ApplicationDisposition::BaselineRetained)
            || !operation.inference.is_empty()
        {
            return false;
        }
    }
    for aggregate in observation
        .passive_aggregates
        .iter()
        .filter(|op| op.feature == feature)
    {
        found = true;
        if aggregate.trigger != bcode_model::judgement::TriggerDisposition::Disabled
            || aggregate.application
                != bcode_model::judgement::ApplicationDisposition::BaselineRetained
        {
            return false;
        }
    }
    found
}

/// Secret-safe diagnostics for failures after authoritative turn terminalization.
#[derive(Debug, Clone, Copy)]
enum ObservationFailureStage {
    PublicRead,
    PublicContract,
    Validation,
    Coverage,
    Telemetry,
}

impl ObservationFailureStage {
    const fn operation(self) -> &'static str {
        match self {
            Self::PublicRead => "normal_turn.observation_public_read_failed",
            Self::PublicContract => "normal_turn.observation_public_contract_failed",
            Self::Validation => "normal_turn.observation_validation_failed",
            Self::Coverage => "normal_turn.observation_coverage_failed",
            Self::Telemetry => "normal_turn.observation_telemetry_failed",
        }
    }

    fn retain(self, capture: &mut ConsumerCapture) {
        capture.operation = self.operation().into();
    }
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
                                .or_else(|| {
                                    payload
                                        .pointer("/advisory/category")
                                        .and_then(serde_json::Value::as_str)
                                })
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
                            .map_err(|error| {
                                let stage = if matches!(
                                    error,
                                    bcode_client::ClientError::UnexpectedResponse
                                ) {
                                    ObservationFailureStage::PublicContract
                                } else {
                                    ObservationFailureStage::PublicRead
                                };
                                stage.retain(capture);
                            })?;
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
                        observation.validate().map_err(|_| {
                            ObservationFailureStage::Validation.retain(capture);
                        })?;
                        let expected_terminal = match capture.outcome {
                            TerminalOutcome::Succeeded => {
                                bcode_model::judgement::ObservationTerminal::Completed
                            }
                            TerminalOutcome::Cancelled => {
                                bcode_model::judgement::ObservationTerminal::Cancelled
                            }
                            _ => bcode_model::judgement::ObservationTerminal::Failed,
                        };
                        if observation.session_id != input.session_id.to_string()
                            || observation.turn_id != receipt.turn_id.0
                            || observation.scope
                                != bcode_model::judgement::ObservationScope::NormalTurn
                            || observation.terminal != Some(expected_terminal)
                            || !observation.complete
                            || observation.omitted != 0
                            || !observation
                                .covered_features
                                .contains(&capture.request.feature)
                        {
                            ObservationFailureStage::Coverage.retain(capture);
                            return Err(());
                        }
                        if capture.request.feature == Feature::Recommendations {
                            capture.output =
                                recommendation_output(&observation, capture.request.arm)?;
                            capture.consumer_executed = capture.output.is_some();
                        }
                        capture
                            .telemetry
                            .retain_turn_observation(&observation)
                            .map_err(|()| {
                                ObservationFailureStage::Telemetry.retain(capture);
                            })?;
                        // A closed disabled baseline application is observable control execution,
                        // not inference or evidence of on-arm benefit.
                        if capture.request.arm == crate::judgement_benchmark::Arm::Off {
                            capture.consumer_executed |=
                                disabled_baseline_observed(&observation, capture.request.feature);
                        }
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
            schema_version: 1,
            accounting: None,
            passive_aggregates: vec![],
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
                    recommendation: None,
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
    fn v2_passive_baseline_preserves_counts_and_legacy_semantics() {
        let legacy = snapshot();
        assert!(legacy.validate().is_ok());
        let mut current = legacy.clone();
        current.schema_version = bcode_model::judgement::TURN_OBSERVATION_VERSION;
        current.accounting = Some(bcode_model::judgement::OperationAccounting {
            begun: 200 + current.operations.len() as u64 - 1,
            omitted_operations: 0,
        });
        let removed = current.operations.remove(0);
        // Ordinals follow producer chronology; detailed records remain distinct.
        current
            .passive_aggregates
            .push(bcode_model::judgement::PassiveOperationAggregate {
                feature: removed.feature,
                trigger: Trigger::Disabled,
                application: Application::BaselineRetained,
                count: 200,
                first_operation_id: 0,
                last_operation_id: 203,
            });
        assert!(current.validate().is_ok());
        assert!(disabled_baseline_observed(
            &current,
            Feature::ToolResultTriage
        ));
        let mut telemetry = crate::judgement_capture::CaptureTelemetry::default();
        telemetry.retain_turn_observation(&current).unwrap();
        assert_eq!(telemetry.inference_calls, Some(0));
        assert_eq!(
            telemetry
                .aggregated_turn_observation
                .as_ref()
                .unwrap()
                .accounting
                .as_ref()
                .unwrap()
                .begun,
            204
        );
        current.passive_aggregates[0].trigger = Trigger::Triggered;
        assert!(current.validate().is_err());
        let mut future = legacy.clone();
        future.schema_version = bcode_model::judgement::TURN_OBSERVATION_VERSION + 1;
        assert!(future.validate().is_err());
        let mut old_gap = legacy;
        old_gap.omitted = 1;
        old_gap.complete = false;
        assert!(old_gap.validate().is_ok());
        old_gap.complete = true;
        assert!(old_gap.validate().is_err());
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
    fn history_selection_requires_feature_coverage_and_unique_canonical_sources() {
        let mut observation = snapshot();
        observation
            .covered_features
            .retain(|feature| *feature != Feature::ContextReranking);
        assert!(convert(&observation, Arm::Off).is_err());
        assert!(
            context_output(
                &snapshot(),
                "session",
                "turn",
                TerminalOutcome::Succeeded,
                Arm::Off,
                &["history:session:2".into(), "history:session:2".into()]
            )
            .is_err()
        );
    }

    #[test]
    fn recommendation_uses_actual_tool_ids_and_rejects_changed_rounds() {
        use bcode_model::judgement::{RecommendationDomain, RecommendationSelectionObservation};
        let mut observation = snapshot();
        let selection = SelectionObservation {
            candidate_count: 2,
            ordered: vec![1, 0],
            admitted: vec![1, 0],
        };
        let operation = &mut observation.operations[2];
        operation.selection = Some(selection.clone());
        operation.recommendation = Some(RecommendationSelectionObservation {
            domain: RecommendationDomain::Tools,
            candidate_ids: vec!["filesystem.read".into(), "filesystem.write".into()],
            selection,
        });
        let Some(ConsumerOutput::Ranking { order, .. }) =
            recommendation_output(&observation, Arm::Off).unwrap()
        else {
            panic!("actual tool catalog baseline required");
        };
        assert_eq!(order, ["filesystem.write", "filesystem.read"]);
        let mut repeated = observation.operations[2].clone();
        repeated.operation_id = observation.operations.len() as u64;
        observation.operations.push(repeated);
        assert!(
            recommendation_output(&observation, Arm::Off)
                .unwrap()
                .is_some()
        );
        observation
            .operations
            .last_mut()
            .unwrap()
            .recommendation
            .as_mut()
            .unwrap()
            .candidate_ids[0] = "filesystem.edit".into();
        assert!(recommendation_output(&observation, Arm::Off).is_err());
        observation.operations.pop();
        observation.operations[2]
            .recommendation
            .as_mut()
            .unwrap()
            .domain = RecommendationDomain::Skills;
        assert!(
            recommendation_output(&observation, Arm::Off)
                .unwrap()
                .is_none()
        );
        observation.operations[2].recommendation = None;
        assert!(recommendation_output(&observation, Arm::Off).is_err());
    }

    #[test]
    fn disabled_control_requires_application_and_zero_inference_not_silence() {
        let mut observation = snapshot();
        assert!(disabled_baseline_observed(
            &observation,
            Feature::ToolResultTriage
        ));
        observation.operations[0].application = None;
        assert!(!disabled_baseline_observed(
            &observation,
            Feature::ToolResultTriage
        ));
        observation.operations[0].application = Some(Application::BaselineRetained);
        observation.operations[0].trigger = Trigger::NotApplicable;
        assert!(!disabled_baseline_observed(
            &observation,
            Feature::ToolResultTriage
        ));
        observation.operations[0].trigger = Trigger::Disabled;
        observation.operations[0]
            .inference
            .push(bcode_model::judgement::OptionalOperationObservation::default());
        assert!(!disabled_baseline_observed(
            &observation,
            Feature::ToolResultTriage
        ));
        observation.operations.remove(0);
        assert!(!disabled_baseline_observed(
            &observation,
            Feature::ToolResultTriage
        ));
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
    fn repeated_rounds_require_identical_decisions_not_a_favorable_final_pick() {
        let mut value = snapshot();
        let mut repeated = value.operations[1].clone();
        repeated.operation_id = value.operations.len() as u64;
        value.operations.push(repeated);
        let Some(ConsumerOutput::Ranking { admitted, .. }) = convert(&value, Arm::Off).unwrap()
        else {
            panic!("identical baseline rounds must remain capturable");
        };
        assert_eq!(admitted, ["history:session:2"]);
        value
            .operations
            .last_mut()
            .unwrap()
            .selection
            .as_mut()
            .unwrap()
            .admitted = vec![1];
        assert!(convert(&value, Arm::Off).is_err());
        value.operations.last_mut().unwrap().selection = value.operations[1].selection.clone();
        value.operations.last_mut().unwrap().application = Some(Application::Unavailable);
        value.complete = false;
        assert!(convert(&value, Arm::Off).is_err());
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
