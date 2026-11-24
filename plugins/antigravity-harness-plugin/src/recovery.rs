//! Optional `bcode.harness.recovery/v1` evidence: exact resume preflight and physical
//! transport release. Never reconstructs release from a Close acknowledgement alone and never
//! adopts a most-recent or unrelated native session.
use crate::transport::AcpError;
use crate::{AntigravityHarnessPlugin, PLUGIN_ID, config::Config};
use bcode_harness_models::{
    HARNESS_RECOVERY_SCHEMA_VERSION, HarnessControlRequest, HarnessInstance, HarnessOpenRequest,
    HarnessRecoveryRequest, HarnessRecoveryRequestKind, HarnessRecoveryResponse,
    HarnessRecoveryResponseKind, HarnessSessionTarget, MAX_HARNESS_ID_BYTES,
    MAX_HARNESS_WIRE_BYTES,
};
use bcode_plugin_sdk::prelude::{NativeServiceContext, ServiceResponse};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Bounded wait for the supervisor to reap a just-released native process.
const REAP_WAIT: Duration = Duration::from_secs(3);

impl AntigravityHarnessPlugin {
    pub(crate) fn recovery_service(&self, context: &NativeServiceContext) -> ServiceResponse {
        self.recovery_evidence(context).map_or_else(
            |_| {
                ServiceResponse::error(
                    "recovery_unavailable",
                    "Exact recovery evidence unavailable",
                )
            },
            |response| {
                ServiceResponse::json(&response).unwrap_or_else(|_| {
                    ServiceResponse::error("invalid_response", "Recovery response encoding failed")
                })
            },
        )
    }

    fn recovery_evidence(
        &self,
        context: &NativeServiceContext,
    ) -> Result<HarnessRecoveryResponse, AcpError> {
        if context.request.payload.len() > MAX_HARNESS_WIRE_BYTES {
            return Err(AcpError::Bounds);
        }
        let request: HarnessRecoveryRequest =
            serde_json::from_slice(&context.request.payload).map_err(|_| AcpError::Protocol)?;
        if request.schema_version != HARNESS_RECOVERY_SCHEMA_VERSION
            || request.request_id.trim().is_empty()
            || request.request_id.len() > MAX_HARNESS_ID_BYTES
            || request.instance.plugin_id != PLUGIN_ID
        {
            return Err(AcpError::Protocol);
        }
        request
            .instance
            .validate()
            .map_err(|_| AcpError::Configuration)?;
        let configuration = Config::from_context(context)?;
        configuration.validate_instance(&request.instance)?;
        let kind = match request.kind {
            HarnessRecoveryRequestKind::PrepareResume { request: open } => {
                if context.request.operation != "prepare_resume" {
                    return Err(AcpError::Protocol);
                }
                let expected = configuration
                    .instances
                    .get(&request.instance.instance_id)
                    .map(|selected| selected.exact_version.as_str());
                validate_resume(&request.instance, &open, expected)?;
                HarnessRecoveryResponseKind::ResumeValidated { request: open }
            }
            HarnessRecoveryRequestKind::ReleaseTransport { request: control } => {
                if context.request.operation != "release_transport" {
                    return Err(AcpError::Protocol);
                }
                let HarnessSessionTarget::Confirmed(binding) = &control.target else {
                    return Err(AcpError::Unsupported);
                };
                // Gather evidence under the lock; the bounded reap wait runs without it.
                let (released, reap_id) = self
                    .state
                    .lock()
                    .map_err(|_| AcpError::Uncertain)
                    .and_then(|state| {
                        let owner = state
                            .owners
                            .get(&binding.session_id)
                            .ok_or(AcpError::Uncertain)?;
                        if owner.instance != request.instance || owner.session.is_some() {
                            return Err(AcpError::Uncertain);
                        }
                        Ok((owner.released.clone(), owner.open.open_id.clone()))
                    })?;
                validate_release(released, &control)?;
                HarnessRecoveryResponseKind::TransportReleased {
                    target: control.target,
                    mutation_id: control.mutation_id,
                    reap_id,
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

fn validate_resume(
    instance: &HarnessInstance,
    open: &HarnessOpenRequest,
    expected_version: Option<&str>,
) -> Result<(), AcpError> {
    let Some(binding) = &open.resume else {
        return Err(AcpError::Unsupported);
    };
    binding.validate().map_err(|_| AcpError::Protocol)?;
    if binding.session_id != open.session_id
        || binding.instance_id != instance.instance_id
        || binding.generation != open.generation
        || !Path::new(&open.working_directory).is_absolute()
    {
        return Err(AcpError::Configuration);
    }
    if let Some(version) = expected_version
        && binding.native_version != version
    {
        return Err(AcpError::Unsupported);
    }
    Ok(())
}

fn validate_release(
    released: Option<(HarnessControlRequest, Arc<AtomicBool>)>,
    requested: &HarnessControlRequest,
) -> Result<(), AcpError> {
    let Some((close, reaped)) = released else {
        return Err(AcpError::Uncertain);
    };
    if close != *requested {
        return Err(AcpError::Uncertain);
    }
    let deadline = Instant::now() + REAP_WAIT;
    while !reaped.load(Ordering::SeqCst) {
        if Instant::now() >= deadline {
            return Err(AcpError::Uncertain);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}
