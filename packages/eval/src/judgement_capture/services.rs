//! Public service microbench adapters. Wire inputs remain producer-owned versioned schemas.
//! No provider answers are injected; prepare/central access/apply is the actual plugin algorithm.
use super::{CaptureStatus, ConsumerCapture, MAX_CAPTURE_BYTES};
use crate::judgement_benchmark::{ConsumerOutput, TerminalOutcome};
use bcode_client::BcodeClient;
use bcode_model::judgement::{Feature, OptionalRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Real producer service algorithms, distinct from normal automatic turn triggers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "service", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceConsumer {
    /// Version-one `TriageRequest` at the loop service boundary.
    ToolResultTriage { input: Value },
    /// Version-one `ProgressRequest`: bounded scope/objective/settled observations.
    ProgressMonitoring { input: Value },
    /// Ordered service observations replayed as bounded prefixes, not a static full-window answer.
    ProgressTrajectory {
        input: Value,
        observation_times_ms: Vec<u64>,
    },
    /// Bounded original-evidence selection through producer prepare/central/apply.
    ContextSelection { input: Value },
    /// Producer `SelectionRequest`: purpose recommendations, original eligible candidates.
    Recommendations { input: Value },
    /// Portable `CategorizationRequest` and one independently preregistered output dimension.
    SessionCategorization {
        input: Value,
        dimension: CategorizationDimension,
    },
}

/// Classification oracle dimension; do not combine unrelated label vocabularies.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CategorizationDimension {
    Task,
    Domain,
}

impl ServiceConsumer {
    /// Feature actually exercised, independent of caller schedule claims.
    #[must_use]
    pub const fn feature(&self) -> Feature {
        match self {
            Self::ToolResultTriage { .. } => Feature::ToolResultTriage,
            Self::ProgressMonitoring { .. } | Self::ProgressTrajectory { .. } => {
                Feature::ProgressMonitoring
            }
            Self::ContextSelection { .. } => Feature::ContextReranking,
            Self::Recommendations { .. } => Feature::Recommendations,
            Self::SessionCategorization { .. } => Feature::SessionCategorization,
        }
    }
}

async fn service(
    client: &BcodeClient,
    interface: &str,
    operation: &str,
    input: &Value,
) -> Result<Value, ()> {
    let payload = serde_json::to_vec(input).map_err(|_| ())?;
    if payload.len() > 16_384 {
        return Err(());
    }
    // Unique public routing fails closed when no producer or multiple producers are registered.
    let response = client
        .call_plugin_service(interface.into(), operation.into(), payload)
        .await
        .map_err(|_| ())?;
    if response.error.is_some() || response.payload.len() > MAX_CAPTURE_BYTES {
        return Err(());
    }
    serde_json::from_slice(&response.payload).map_err(|_| ())
}

pub(super) async fn execute(
    client: &BcodeClient,
    input: ServiceConsumer,
    capture: &mut ConsumerCapture,
) -> Result<(), ()> {
    capture.scope = super::CaptureScope::ServiceMicrobenchmark;
    let output = match input {
        ServiceConsumer::ToolResultTriage { input } => {
            capture.operation = "bcode.loop.tool-result-triage/v1:triage".into();
            let response =
                service(client, "bcode.loop.tool-result-triage/v1", "triage", &input).await?;
            capture.telemetry.retain_service_response(&response)?;
            let advisory = response.get("advisory").ok_or(())?;
            let label = if advisory.is_null() {
                None
            } else {
                Some(advisory["category"].as_str().ok_or(())?.to_owned())
            };
            ConsumerOutput::Classification { label }
        }
        ServiceConsumer::ProgressMonitoring { input } => {
            capture.operation = "bcode.loop.progress-monitoring/v1:observe".into();
            let response = service(
                client,
                "bcode.loop.progress-monitoring/v1",
                "observe",
                &input,
            )
            .await?;
            capture.telemetry.retain_service_response(&response)?;
            if response["version"] != 1 {
                return Err(());
            }
            let advice = response.get("advice").ok_or(())?;
            let label = if advice.is_null() {
                None
            } else {
                Some(
                    match advice["kind"].as_str().ok_or(())? {
                        "repeated_failure" => "stall",
                        "drift" => "drift",
                        _ => return Err(()),
                    }
                    .to_owned(),
                )
            };
            // A microbench invocation has no normal-turn detection timeline. Delay stays unknown.
            ConsumerOutput::Progress {
                label,
                detected_ms: None,
            }
        }
        ServiceConsumer::ProgressTrajectory {
            input,
            observation_times_ms,
        } => {
            capture.operation =
                "bcode.loop.progress-monitoring/v1:observe_prefix_trajectory".into();
            let observations = input["observations"].as_array().ok_or(())?;
            if observations.len() != observation_times_ms.len()
                || observations.len() > 8
                || observation_times_ms
                    .windows(2)
                    .any(|pair| pair[0] >= pair[1])
            {
                return Err(());
            }
            let mut label = None;
            let mut detected_ms = None;
            // Replay supplied timeline prefixes serially. Detection is in corpus timeline units,
            // not wall time or a normal automatic trigger. No future observations reach the service.
            for (index, time) in observation_times_ms.iter().enumerate() {
                let mut prefix = input.clone();
                prefix["observations"] = Value::Array(observations[..=index].to_vec());
                let response = service(
                    client,
                    "bcode.loop.progress-monitoring/v1",
                    "observe",
                    &prefix,
                )
                .await?;
                capture.telemetry.retain_service_response(&response)?;
                if response["version"] != 1 {
                    return Err(());
                }
                let advice = response.get("advice").ok_or(())?;
                if !advice.is_null() && label.is_none() {
                    label = Some(
                        match advice["kind"].as_str().ok_or(())? {
                            "repeated_failure" => "stall",
                            "drift" => "drift",
                            _ => return Err(()),
                        }
                        .to_owned(),
                    );
                    detected_ms = Some(*time);
                }
            }
            ConsumerOutput::Progress { label, detected_ms }
        }
        ServiceConsumer::ContextSelection { input } => {
            capture.operation = "bcode.context-selection/v1:prepare+optional_judge+apply".into();
            let prepared = service(client, "bcode.context-selection/v1", "prepare", &input).await?;
            let prepared = prepared.get("judgement").ok_or(())?;
            let result = if prepared.is_null() {
                json!({"response":null})
            } else {
                let query: OptionalRequest =
                    serde_json::from_value(prepared.clone()).map_err(|_| ())?;
                if query.feature != Feature::ContextReranking {
                    return Err(());
                }
                let response = client.optional_judge(&query).await.map_err(|_| ())?;
                capture.telemetry.retain_optional_response(&response);
                serde_json::to_value(response).map_err(|_| ())?
            };
            let response = service(
                client,
                "bcode.context-selection/v1",
                "apply",
                &json!({"selection":input,"judgement":result}),
            )
            .await?;
            ConsumerOutput::Ranking {
                order: serde_json::from_value(response.get("ordered_ids").ok_or(())?.clone())
                    .map_err(|_| ())?,
                admitted: serde_json::from_value(response.get("included_ids").ok_or(())?.clone())
                    .map_err(|_| ())?,
            }
        }
        ServiceConsumer::Recommendations { input } => {
            if input["purpose"] != "recommendations" {
                return Err(());
            }
            capture.operation = "bcode.skills-selection/v1:prepare+optional_judge+apply".into();
            let prepared = service(client, "bcode.skills-selection/v1", "prepare", &input).await?;
            let prepared = prepared.get("judgement").ok_or(())?;
            let result = if prepared.is_null() {
                json!({"response":null})
            } else {
                let query: OptionalRequest =
                    serde_json::from_value(prepared.clone()).map_err(|_| ())?;
                if query.feature != Feature::Recommendations {
                    return Err(());
                }
                let response = client.optional_judge(&query).await.map_err(|_| ())?;
                capture.telemetry.retain_optional_response(&response);
                serde_json::to_value(response).map_err(|_| ())?
            };
            let response = service(
                client,
                "bcode.skills-selection/v1",
                "apply",
                &json!({"selection":input,"judgement":result}),
            )
            .await?;
            let order: Vec<String> =
                serde_json::from_value(response.get("ordered_ids").ok_or(())?.clone())
                    .map_err(|_| ())?;
            let original: Vec<&str> = input["candidates"]
                .as_array()
                .ok_or(())?
                .iter()
                .map(|candidate| candidate["id"].as_str().ok_or(()))
                .collect::<Result<_, _>>()?;
            let actual: std::collections::BTreeSet<&str> =
                order.iter().map(String::as_str).collect();
            let expected: std::collections::BTreeSet<&str> = original.iter().copied().collect();
            if order.len() != original.len()
                || actual != expected
                || actual.len() != order.len()
                || input["candidates"]
                    .as_array()
                    .ok_or(())?
                    .iter()
                    .zip(&order)
                    .any(|(candidate, id)| candidate["protected"] == true && candidate["id"] != *id)
            {
                return Err(());
            }
            ConsumerOutput::Ranking {
                admitted: order.clone(),
                order,
            }
        }
        ServiceConsumer::SessionCategorization { input, dimension } => {
            capture.operation = "bcode.session-categorization/v1:categorize".into();
            let response = service(
                client,
                "bcode.session-categorization/v1",
                "categorize",
                &input,
            )
            .await?;
            capture.telemetry.retain_service_response(&response)?;
            let labels = response.get("categorization").ok_or(())?;
            let label = if labels.is_null() {
                None
            } else {
                if labels["source_sequence"] != input["source_sequence"] {
                    return Err(());
                }
                Some(
                    labels[match dimension {
                        CategorizationDimension::Task => "task",
                        CategorizationDimension::Domain => "domain",
                    }]
                    .as_str()
                    .ok_or(())?
                    .to_owned(),
                )
            };
            ConsumerOutput::Classification { label }
        }
    };
    capture.status = match &output {
        ConsumerOutput::Classification { label: None }
        | ConsumerOutput::Progress { label: None, .. } => CaptureStatus::FallbackReasonUnknown,
        _ => CaptureStatus::Captured,
    };
    capture.consumer_executed = true;
    capture.outcome = TerminalOutcome::Succeeded;
    capture.output = Some(output);
    Ok(())
}
