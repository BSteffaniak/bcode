//! Optional exact-intent recovery evidence; never reconstruct release from Close Ack.
#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
use super::{
    ClaudeCodePlugin, Config, HARNESS_SCHEMA_VERSION, HarnessControlRequest, HarnessFailure,
    HarnessInstance, HarnessOpenRequest, HarnessRequest, HarnessRequestKind, InstanceConfig,
    MAX_HARNESS_ID_BYTES, MAX_HARNESS_WIRE_BYTES, NativeServiceContext, PLUGIN_ID, ServiceResponse,
};
use bcode_harness_models::{
    HARNESS_RECOVERY_SCHEMA_VERSION, HarnessRecoveryRequest, HarnessRecoveryRequestKind,
    HarnessRecoveryResponse, HarnessRecoveryResponseKind,
};

impl ClaudeCodePlugin {
    pub(super) fn recovery_service(&mut self, context: &NativeServiceContext) -> ServiceResponse {
        let result = self.recovery_evidence(context);
        result.map_or_else(
            |_| {
                ServiceResponse::error(
                    "recovery_unavailable",
                    "Exact recovery evidence unavailable",
                )
            },
            |response| {
                ServiceResponse::json(&response).unwrap_or_else(|_| {
                    ServiceResponse::error("invalid_response", "Harness response encoding failed")
                })
            },
        )
    }

    fn recovery_evidence(
        &mut self,
        context: &NativeServiceContext,
    ) -> Result<HarnessRecoveryResponse, HarnessFailure> {
        if context.request.payload.len() > MAX_HARNESS_WIRE_BYTES {
            return Err(HarnessFailure::InvalidRequest);
        }
        let request: HarnessRecoveryRequest = serde_json::from_slice(&context.request.payload)
            .map_err(|_| HarnessFailure::InvalidRequest)?;
        if request.schema_version != HARNESS_RECOVERY_SCHEMA_VERSION
            || request.request_id.trim().is_empty()
            || request.request_id.len() > MAX_HARNESS_ID_BYTES
            || request.instance.plugin_id != PLUGIN_ID
        {
            return Err(HarnessFailure::InvalidRequest);
        }
        request
            .instance
            .validate()
            .map_err(|_| HarnessFailure::InvalidRequest)?;
        let configuration: Config = context.config().map_err(|_| HarnessFailure::Unavailable)?;
        let selected = configuration
            .instances
            .get(&request.instance.instance_id)
            .ok_or(HarnessFailure::Unavailable)?;
        let kind = match request.kind {
            HarnessRecoveryRequestKind::PrepareResume { request: open } => {
                if context.request.operation != "prepare_resume" {
                    return Err(HarnessFailure::InvalidRequest);
                }
                validate_resume(&request.instance, &open, selected)?;
                HarnessRecoveryResponseKind::ResumeValidated { request: open }
            }
            HarnessRecoveryRequestKind::ReleaseTransport { request: control } => {
                if context.request.operation != "release_transport" {
                    return Err(HarnessFailure::InvalidRequest);
                }
                // No stop, no native request, no synthesized Close, no mutation replay.
                // Host independently proves canonical Close; this adapter proves its retained reap.
                selected.inspection_options(&request.instance)?;
                let process = self.retained_process(&request.instance, &control.target)?;
                if process.prepared.native_version != selected.exact_version {
                    return Err(HarnessFailure::Conflict);
                }
                validate_release(process.closed, process.released_close.as_ref(), &control)?;
                HarnessRecoveryResponseKind::TransportReleased {
                    target: control.target,
                    mutation_id: control.mutation_id,
                    reap_id: process.open.open_id.clone(),
                }
            }
        };
        Ok(HarnessRecoveryResponse {
            schema_version: HARNESS_RECOVERY_SCHEMA_VERSION,
            request_id: request.request_id,
            kind,
        })
    }
}

fn validate_release(
    closed: bool,
    retained: Option<&HarnessControlRequest>,
    requested: &HarnessControlRequest,
) -> Result<(), HarnessFailure> {
    if closed && retained == Some(requested) {
        Ok(())
    } else {
        Err(HarnessFailure::Uncertain)
    }
}

/// Validate exact resume selection, then fail closed until inert startup is supported.
/// # Errors
/// Rejects invalid selection and returns `Unsupported` for otherwise matching intent.
pub fn validate_resume(
    instance: &HarnessInstance,
    open: &HarnessOpenRequest,
    selected: &InstanceConfig,
) -> Result<(), HarnessFailure> {
    validate_resume_selection(instance, open, selected)?;
    // Exact --resume selection is not proof of inert startup. The current native
    // transport keeps stdin piped and has no verified no-autocontinuation contract
    // for unresolved tools/background work. Never certify recovery from identity alone.
    Err(HarnessFailure::Unsupported)
}

fn validate_resume_selection(
    instance: &HarnessInstance,
    open: &HarnessOpenRequest,
    selected: &InstanceConfig,
) -> Result<(), HarnessFailure> {
    let request = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: open.open_id.clone(),
        instance: instance.clone(),
        kind: HarnessRequestKind::Open(open.clone()),
    };
    request
        .validate()
        .map_err(|_| HarnessFailure::InvalidRequest)?;
    let binding = open.resume.as_ref().ok_or(HarnessFailure::InvalidRequest)?;
    // Host constructs the prospective N+1 binding before preflight and CAS. Never increment or
    // normalize here: the entire binding must already match the requested generation.
    if binding.native_version != selected.exact_version {
        return Err(HarnessFailure::Conflict);
    }
    if binding.generation != open.generation || binding.instance_id != instance.instance_id {
        return Err(HarnessFailure::Conflict);
    }
    bcode_claude_code::validate_session_uuid(&binding.native_session_id)
        .map_err(|_| HarnessFailure::InvalidRequest)?;
    // Pure launch validation only: does not fetch credentials, run --version, or spawn.
    selected.inspection_options(instance)?;
    if !std::path::Path::new(&open.working_directory).is_absolute() {
        return Err(HarnessFailure::Policy);
    }
    Ok(())
}
