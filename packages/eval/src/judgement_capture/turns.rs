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

pub(super) async fn execute(
    client: &BcodeClient,
    input: NormalTurnRequest,
    capture: &mut ConsumerCapture,
) -> Result<(), ()> {
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
    ) {
        return Err(());
    }
    if capture.request.feature == Feature::Recommendations {
        // Use the public service microbench for this feature until normal-turn final catalog
        // ordering has a bounded public observation. Do not spend on an uncapturable turn.
        return Err(());
    }
    capture.scope = CaptureScope::NormalTurn;
    capture.operation = "normal_turn.submit+live_contributions+terminal".into();
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
                            label = payload["category"].as_str().map(str::to_owned);
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
                        Feature::Recommendations => None,
                        _ => return Err(()),
                    };
                    capture.status = if capture.output.is_some() && capture.consumer_executed {
                        CaptureStatus::Captured
                    } else {
                        CaptureStatus::ConsumerUnavailable
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
