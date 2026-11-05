//! Borrowed capacity admission for typed external events, before any source clone.

use super::{Inspector, ObservationRetentionExceeded};
use crate::SessionEventKind;
use bcode_harness_models::{HarnessInteraction, HarnessObservationKind, HarnessResponseValue};

type Result = std::result::Result<(), ObservationRetentionExceeded>;

fn interaction(i: &mut Inspector, value: &HarnessInteraction, depth: usize) -> Result {
    i.nodes(3, depth)?;
    for text in [
        &value.request_id,
        &value.session_id,
        &value.prompt,
        &value.scope,
    ] {
        i.text(text, depth + 1)?;
    }
    i.collection(value.choices.capacity(), depth + 1)?;
    for choice in &value.choices {
        i.text(&choice.choice_id, depth + 2)?;
        i.text(&choice.label, depth + 2)?;
    }
    Ok(())
}

pub(super) fn inspect(i: &mut Inspector, kind: &SessionEventKind) -> Result {
    i.nodes(1, 3)?;
    match kind {
        SessionEventKind::ExternalBackendBound {
            instance, binding, ..
        } => {
            i.nodes(4, 4)?;
            for text in [
                &instance.instance_id,
                &instance.plugin_id,
                &instance.executable,
                &instance.config_namespace,
                &instance.auth.provider_id,
                &instance.auth.profile,
                &instance.auth.credential_namespace,
                &binding.session_id,
                &binding.instance_id,
                &binding.native_session_id,
                &binding.native_version,
            ] {
                i.text(text, 5)?;
            }
        }
        SessionEventKind::ExternalSubmissionAccepted { submission, .. } => {
            i.nodes(2, 4)?;
            for text in [
                &submission.submission_id,
                &submission.attempt_id,
                &submission.session_id,
                &submission.text,
            ] {
                i.text(text, 5)?;
            }
            if let bcode_harness_models::HarnessPlacement::Steer {
                expected_native_turn_id,
            } = &submission.placement
            {
                i.text(expected_native_turn_id, 6)?;
            }
        }
        SessionEventKind::ExternalDeliveryObserved { receipt, .. } => {
            i.nodes(2, 4)?;
            for text in [
                &receipt.submission_id,
                &receipt.attempt_id,
                &receipt.session_id,
            ] {
                i.text(text, 5)?;
            }
            i.optional_text(receipt.native_request_id.as_ref(), 5)?;
        }
        SessionEventKind::ExternalObservationRecorded {
            observation: value, ..
        } => observation(i, value)?,
        SessionEventKind::ExternalInteractionResponded {
            interaction: request,
            response,
            ..
        } => {
            interaction(i, request, 4)?;
            i.nodes(2, 4)?;
            for text in [
                &response.response_id,
                &response.request_id,
                &response.session_id,
            ] {
                i.text(text, 5)?;
            }
            match &response.value {
                HarnessResponseValue::Approval { .. } | HarnessResponseValue::Cancel => {
                    i.nodes(1, 6)?;
                }
                HarnessResponseValue::Answer { choice_id, text } => {
                    i.optional_text(choice_id.as_ref(), 6)?;
                    i.text(text, 6)?;
                }
                HarnessResponseValue::Assistance { text } => i.text(text, 6)?,
            }
        }
        _ => return Err(ObservationRetentionExceeded),
    }
    Ok(())
}

pub(super) fn observation(
    i: &mut Inspector,
    observation: &bcode_harness_models::HarnessObservation,
) -> Result {
    i.nodes(3, 4)?;
    i.text(&observation.observation_id, 5)?;
    i.text(&observation.session_id, 5)?;
    i.collection(observation.native_correlations.capacity(), 5)?;
    for correlation in &observation.native_correlations {
        i.text(correlation, 6)?;
    }
    match &observation.kind {
        HarnessObservationKind::SubmissionAccepted {
            submission_id,
            attempt_id,
            native_turn_id,
        } => {
            for text in [submission_id, attempt_id, native_turn_id] {
                i.text(text, 6)?;
            }
        }
        HarnessObservationKind::BindingConfirmed { open_id, binding } => {
            i.text(open_id, 6)?;
            i.nodes(2, 6)?;
            for text in [
                &binding.session_id,
                &binding.instance_id,
                &binding.native_session_id,
                &binding.native_version,
            ] {
                i.text(text, 7)?;
            }
        }
        HarnessObservationKind::Text { text } | HarnessObservationKind::Result { text } => {
            i.text(text, 6)?;
        }
        HarnessObservationKind::Tool {
            native_tool_id,
            name,
            status,
        } => {
            for text in [native_tool_id, name, status] {
                i.text(text, 6)?;
            }
        }
        HarnessObservationKind::Subagent {
            native_agent_id,
            status,
        } => {
            i.text(native_agent_id, 6)?;
            i.text(status, 6)?;
        }
        HarnessObservationKind::Interaction { interaction: value } => interaction(i, value, 6)?,
        HarnessObservationKind::Status { .. } => i.nodes(5, 6)?,
        HarnessObservationKind::Diagnostic { code, message } => {
            i.text(code, 6)?;
            i.text(message, 6)?;
        }
        HarnessObservationKind::Settlement { native_turn_id } => i.text(native_turn_id, 6)?,
        HarnessObservationKind::Vendor { namespace, detail } => {
            i.text(namespace, 6)?;
            i.json(detail, 6)?;
        }
    }
    Ok(())
}
