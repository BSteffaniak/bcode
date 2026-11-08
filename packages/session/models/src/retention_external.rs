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
    i.optional_text(value.tool_name.as_ref(), depth + 1)?;
    i.optional_text(value.input_preview.as_ref(), depth + 1)?;
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
        HarnessObservationKind::Execution { event } => execution(i, event)?,
        HarnessObservationKind::SubmissionAccepted {
            submission_id,
            attempt_id,
            native_turn_id,
        } => {
            for text in [submission_id, attempt_id, native_turn_id] {
                i.text(text, 6)?;
            }
        }
        HarnessObservationKind::SubmissionConsumed {
            submission_id,
            attempt_id,
            native_frame_id,
        }
        | HarnessObservationKind::SubmissionResultObserved {
            submission_id,
            attempt_id,
            native_frame_id,
            ..
        } => {
            for text in [submission_id, attempt_id, native_frame_id] {
                i.text(text, 6)?;
            }
            i.nodes(2, 6)?;
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
        HarnessObservationKind::Text { .. }
        | HarnessObservationKind::Reasoning { .. }
        | HarnessObservationKind::Result { .. }
        | HarnessObservationKind::Tool { .. }
        | HarnessObservationKind::ToolResult { .. }
        | HarnessObservationKind::Usage { .. } => display_observation(i, &observation.kind)?,
        HarnessObservationKind::Subagent {
            native_agent_id,
            status,
        } => {
            i.text(native_agent_id, 6)?;
            i.text(status, 6)?;
        }
        HarnessObservationKind::Interaction { interaction: value } => interaction(i, value, 6)?,
        HarnessObservationKind::InteractionWithdrawn { request_id } => i.text(request_id, 6)?,
        HarnessObservationKind::Status { .. } => i.nodes(5, 6)?,
        HarnessObservationKind::Diagnostic { code, message } => {
            i.text(code, 6)?;
            i.text(message, 6)?;
        }
        HarnessObservationKind::Settlement { native_turn_id }
        | HarnessObservationKind::NativeTurnTerminal { native_turn_id, .. } => {
            i.text(native_turn_id, 6)?;
            i.nodes(1, 6)?;
        }
        HarnessObservationKind::Vendor { namespace, detail } => {
            i.text(namespace, 6)?;
            i.json(detail, 6)?;
        }
    }
    Ok(())
}

fn display_observation(i: &mut Inspector, kind: &HarnessObservationKind) -> Result {
    match kind {
        HarnessObservationKind::Text { text, segment_id }
        | HarnessObservationKind::Reasoning {
            text, segment_id, ..
        } => {
            i.text(text, 6)?;
            i.optional_text(segment_id.as_ref(), 6)?;
        }
        HarnessObservationKind::Result { text } => {
            i.text(text, 6)?;
        }
        HarnessObservationKind::Tool {
            native_tool_id,
            name,
            status,
            input_json,
        } => {
            for text in [native_tool_id, name, status] {
                i.text(text, 6)?;
            }
            i.optional_text(input_json.as_ref(), 6)?;
        }
        HarnessObservationKind::ToolResult {
            native_tool_id,
            output,
            ..
        } => {
            i.text(native_tool_id, 6)?;
            i.text(output, 6)?;
            i.nodes(2, 6)?;
        }
        HarnessObservationKind::Usage { model, .. } => {
            i.nodes(6, 6)?;
            i.optional_text(model.as_ref(), 6)?;
        }
        _ => {}
    }
    Ok(())
}

fn execution(i: &mut Inspector, event: &bcode_harness_models::HarnessExecutionEvent) -> Result {
    use bcode_harness_models::HarnessExecutionEvent as E;
    i.nodes(8, 6)?;
    match event {
        E::SubmissionTerminal {
            attempt,
            native_turn_id,
            native_frame_id,
            output,
            ..
        } => {
            attempt_identity(i, attempt)?;
            for value in [native_turn_id, native_frame_id, output]
                .into_iter()
                .flatten()
            {
                i.text(value, 7)?;
            }
        }
        E::BackgroundWork {
            work_id,
            native_task_id,
            origin,
            parent_work_id,
            label,
            ..
        } => {
            i.text(work_id, 7)?;
            i.text(label, 7)?;
            for value in [native_task_id, parent_work_id].into_iter().flatten() {
                i.text(value, 7)?;
            }
            if let Some(origin) = origin {
                attempt_identity(i, origin)?;
            }
        }
        E::Continuation {
            continuation_id,
            origin,
            source_work_id,
            native_turn_id,
            output,
            ..
        } => {
            i.text(continuation_id, 7)?;
            for value in [source_work_id, native_turn_id, output]
                .into_iter()
                .flatten()
            {
                i.text(value, 7)?;
            }
            if let Some(origin) = origin {
                attempt_identity(i, origin)?;
            }
        }
        E::Coverage { attempt, .. } => attempt_identity(i, attempt)?,
    }
    Ok(())
}

fn attempt_identity(
    i: &mut Inspector,
    attempt: &bcode_harness_models::HarnessAttemptIdentity,
) -> Result {
    i.nodes(2, 7)?;
    i.text(&attempt.submission_id, 8)?;
    i.text(&attempt.attempt_id, 8)
}
