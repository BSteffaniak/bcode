//! Optional `bcode.harness.recovery/v1` evidence: exact resume preflight and physical
//! transport release. Never reconstructs release from a Close acknowledgement alone and never
//! adopts a most-recent or unrelated native thread.
use crate::transport::CodexError;
use crate::{PLUGIN_ID, PluginState, config::Config, launch};
use bcode_harness_models::{
    HARNESS_RECOVERY_SCHEMA_VERSION, HARNESS_SCHEMA_VERSION, HarnessControlRequest,
    HarnessInstance, HarnessOpenRequest, HarnessRecoveryRequest, HarnessRecoveryRequestKind,
    HarnessRecoveryResponse, HarnessRecoveryResponseKind, HarnessRequest, HarnessRequestKind,
    HarnessSessionTarget, MAX_HARNESS_ID_BYTES, MAX_HARNESS_WIRE_BYTES,
};
use bcode_plugin_sdk::prelude::{NativeServiceContext, ServiceResponse};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Bounded wait for the supervisor to reap a just-released native process.
const REAP_WAIT: Duration = Duration::from_secs(3);

impl PluginState {
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
    ) -> Result<HarnessRecoveryResponse, CodexError> {
        if context.request.payload.len() > MAX_HARNESS_WIRE_BYTES {
            return Err(CodexError::Bounds);
        }
        let request: HarnessRecoveryRequest =
            serde_json::from_slice(&context.request.payload).map_err(|_| CodexError::Protocol)?;
        if request.schema_version != HARNESS_RECOVERY_SCHEMA_VERSION
            || request.request_id.trim().is_empty()
            || request.request_id.len() > MAX_HARNESS_ID_BYTES
            || request.instance.plugin_id != PLUGIN_ID
        {
            return Err(CodexError::Protocol);
        }
        request
            .instance
            .validate()
            .map_err(|_| CodexError::Configuration)?;
        let configuration = Config::from_context(context)?;
        configuration.validate_instance(&request.instance)?;
        let kind = match request.kind {
            HarnessRecoveryRequestKind::PrepareResume { request: open } => {
                if context.request.operation != "prepare_resume" {
                    return Err(CodexError::Protocol);
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
                    return Err(CodexError::Protocol);
                }
                let HarnessSessionTarget::Confirmed(binding) = &control.target else {
                    return Err(CodexError::Unsupported);
                };
                let owner = self
                    .owners
                    .get(&binding.session_id)
                    .ok_or(CodexError::Uncertain)?;
                if owner.instance != request.instance || owner.session.is_some() {
                    return Err(CodexError::Uncertain);
                }
                validate_release(owner.released.as_ref(), &control)?;
                HarnessRecoveryResponseKind::TransportReleased {
                    target: control.target,
                    mutation_id: control.mutation_id,
                    reap_id: owner.open.open_id.clone(),
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

/// Require the exact retained Close and the supervisor's physical reap of that process.
fn validate_release(
    released: Option<&(
        HarnessControlRequest,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    )>,
    requested: &HarnessControlRequest,
) -> Result<(), CodexError> {
    let Some((close, reaped)) = released else {
        return Err(CodexError::Uncertain);
    };
    if close != requested {
        return Err(CodexError::Uncertain);
    }
    let deadline = Instant::now() + REAP_WAIT;
    while !reaped.load(Ordering::SeqCst) {
        if Instant::now() >= deadline {
            return Err(CodexError::Uncertain);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// Whether a binding's recorded native version may be resumed.
///
/// Bindings record the executable version probed when the thread was created, which may differ
/// from the currently installed (and configured expected) version after a Codex CLI upgrade.
/// The app-server persists threads in the admitted `CODEX_HOME` and `thread/resume` reads
/// them by exact thread id, so any well-formed recorded version (or the exact configured value)
/// is resumable. The resumed binding keeps its recorded version as evidence.
#[must_use]
pub fn resumable_version(version: &str, expected_version: Option<&str>) -> bool {
    expected_version == Some(version) || crate::transport::well_formed_version(version)
}

/// Validate exact resume selection for a host-published recovery intent, without spawning.
///
/// The resumed app-server calls `thread/resume` with the exact native thread id (never a
/// most-recent selection) and sends nothing until the host submits a new turn. Prior input is
/// never replayed; native history is not hydrated into Bcode.
///
/// # Errors
/// Rejects an invalid request, a binding for another generation or instance, a malformed
/// recorded native version (see [`resumable_version`]), a malformed thread id or a relative
/// workspace.
pub fn validate_resume(
    instance: &HarnessInstance,
    open: &HarnessOpenRequest,
    expected_version: Option<&str>,
) -> Result<(), CodexError> {
    let request = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: open.open_id.clone(),
        instance: instance.clone(),
        kind: HarnessRequestKind::Open(open.clone()),
    };
    request.validate().map_err(|_| CodexError::Protocol)?;
    let binding = open.resume.as_ref().ok_or(CodexError::Protocol)?;
    if binding.generation != open.generation || binding.instance_id != instance.instance_id {
        return Err(CodexError::Stale);
    }
    if !resumable_version(&binding.native_version, expected_version) {
        return Err(CodexError::Unsupported);
    }
    let thread = &binding.native_session_id;
    if thread.is_empty()
        || thread.len() > 64
        || !thread
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(CodexError::Protocol);
    }
    if !std::path::Path::new(&open.working_directory).is_absolute() {
        return Err(CodexError::Configuration);
    }
    launch(instance, &open.working_directory).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_harness_models::{HarnessAuthBinding, HarnessAuthCustody, HarnessSessionBinding};
    use std::sync::{Arc, atomic::AtomicBool};

    fn instance() -> HarnessInstance {
        HarnessInstance {
            instance_id: "codex".into(),
            plugin_id: PLUGIN_ID.into(),
            executable: "/bin/codex".into(),
            config_namespace: "/home/user/.codex".into(),
            auth: HarnessAuthBinding {
                provider_id: crate::AUTH_PROVIDER_ID.into(),
                profile: "native".into(),
                credential_namespace: "/home/user/.codex".into(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        }
    }

    fn open(generation: u64, thread: &str, version: &str) -> HarnessOpenRequest {
        HarnessOpenRequest {
            open_id: "resume-open".into(),
            session_id: "session".into(),
            generation,
            working_directory: "/work".into(),
            resume: Some(HarnessSessionBinding {
                schema_version: HARNESS_SCHEMA_VERSION,
                session_id: "session".into(),
                instance_id: "codex".into(),
                native_session_id: thread.into(),
                native_version: version.into(),
                generation,
            }),
            parent_assistance: None,
        }
    }

    #[test]
    fn resume_requires_exact_generation_and_thread_identity_across_versions() {
        let thread = "01a1bd78-1a93-7ae1-85af-30a00aa6d93c";
        assert!(validate_resume(&instance(), &open(2, thread, "0.164.0"), Some("0.164.0")).is_ok());
        assert!(validate_resume(&instance(), &open(2, thread, "0.164.0"), None).is_ok());
        // A binding recorded by an older (or newer) CLI remains resumable after an upgrade.
        assert!(validate_resume(&instance(), &open(2, thread, "0.163.0"), Some("0.164.0")).is_ok());
        assert!(matches!(
            validate_resume(
                &instance(),
                &open(2, thread, "bad version"),
                Some("0.164.0")
            ),
            Err(CodexError::Protocol | CodexError::Unsupported)
        ));
        assert!(resumable_version("0.163.0", Some("0.164.0")));
        assert!(resumable_version("0.165.0-alpha.1+build", None));
        assert!(!resumable_version("bad version", None));
        assert!(!resumable_version("", None));
        assert!(!resumable_version(&"9".repeat(129), None));
        assert_eq!(
            validate_resume(&instance(), &open(2, "../other", "0.164.0"), None),
            Err(CodexError::Protocol)
        );
        let mut mismatched = open(2, thread, "0.164.0");
        if let Some(binding) = mismatched.resume.as_mut() {
            binding.generation = 1;
        }
        assert!(validate_resume(&instance(), &mismatched, None).is_err());
        let mut fresh = open(2, thread, "0.164.0");
        fresh.resume = None;
        assert_eq!(
            validate_resume(&instance(), &fresh, None),
            Err(CodexError::Protocol)
        );
    }

    #[test]
    fn release_requires_exact_close_and_physical_reap() {
        let close = HarnessControlRequest {
            target: HarnessSessionTarget::Confirmed(open(1, "a-b", "1").resume.unwrap()),
            mutation_id: "close-1".into(),
        };
        assert_eq!(validate_release(None, &close), Err(CodexError::Uncertain));
        let reaped = Arc::new(AtomicBool::new(true));
        let retained = (close.clone(), Arc::clone(&reaped));
        assert!(validate_release(Some(&retained), &close).is_ok());
        let mut other = close.clone();
        other.mutation_id = "close-2".into();
        assert_eq!(
            validate_release(Some(&retained), &other),
            Err(CodexError::Uncertain)
        );
        let pending = (close.clone(), Arc::new(AtomicBool::new(false)));
        assert_eq!(
            validate_release(Some(&pending), &close),
            Err(CodexError::Uncertain)
        );
    }
}
