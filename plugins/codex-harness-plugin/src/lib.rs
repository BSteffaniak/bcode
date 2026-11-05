#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Codex native app-server backend. This plugin never acts as a model provider,
//! dispatches observed native tools, or stores a recovery journal.
pub mod auth;
pub mod session;
pub mod transport;

use auth::CodexAuth;
use bcode_harness_models::{
    HARNESS_INTERFACE_ID, HARNESS_SCHEMA_VERSION, HarnessAuthStep, HarnessCapabilities,
    HarnessCapability, HarnessControlRequest, HarnessFailure, HarnessInspection, HarnessInstance,
    HarnessObservationPage, HarnessObserveRequest, HarnessOpenRequest, HarnessReconciliation,
    HarnessRequest, HarnessRequestKind, HarnessServiceResponse, HarnessServiceResponseKind,
    HarnessSessionBinding, HarnessSessionTarget, HarnessSupportLevel, MAX_HARNESS_WIRE_BYTES,
};
use bcode_plugin_sdk::prelude::*;
use bcode_provider_auth_models::managed::AuthCustodyMode;
use session::CodexSession;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;
use transport::{CodexError, Launch};

const PLUGIN_ID: &str = "bcode.codex-harness";

struct Owner {
    instance: HarnessInstance,
    open: HarnessOpenRequest,
    session: Option<CodexSession>,
    page_number: u64,
    next: Option<String>,
    previous: Option<(Option<String>, HarnessObservationPage)>,
    interrupt: Option<(String, Result<(), CodexError>)>,
}
struct AuthOwner {
    instance: HarnessInstance,
    auth: CodexAuth,
}

/// Native plugin service owner. Host must admit/retain mutations and suppress sensitive traces
/// before invoking this boundary. Serialization is not a grant of execution authority.
#[derive(Default)]
pub struct CodexHarnessPlugin {
    owners: BTreeMap<String, Owner>,
    auth: BTreeMap<String, AuthOwner>,
}

impl RustPlugin for CodexHarnessPlugin {
    fn register_auth_providers(&mut self, registrar: AuthRegistrar) -> Result<(), PluginError> {
        use bcode_provider_auth_models::{
            AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION, AuthMethodContribution,
            AuthProviderContribution, AuthSecretField, AuthSecretValidation,
        };
        // Native custody is handled by managed auth, not an empty vault enrollment.
        registrar
            .register(&AuthProviderContribution {
                schema_version: AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
                provider_id: "bcode.codex".to_owned(),
                display_name: "Codex".to_owned(),
                methods: vec![AuthMethodContribution::SecretFields {
                    method_id: "api_key".to_owned(),
                    display_name: "API key (ephemeral native delivery)".to_owned(),
                    fields: vec![AuthSecretField {
                        credential_id: "api_key".to_owned(),
                        storage_key: "CODEX_API_KEY".to_owned(),
                        prompt: "Codex API key".to_owned(),
                        optional: false,
                        validation: AuthSecretValidation {
                            min_bytes: Some(1),
                            max_bytes: Some(16 * 1024),
                            required_prefix: None,
                        },
                        discovery_sources: Vec::new(),
                        invocation_env: Vec::new(),
                    }],
                    supports_verification: false,
                    supports_revocation: false,
                }],
            })
            .map_err(|_| PluginError::failed("Codex auth registration failed"))
    }

    fn deactivate(&mut self) -> Result<(), PluginError> {
        self.owners.clear();
        self.auth.clear();
        Ok(())
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.plugin_id != PLUGIN_ID || context.request.interface_id != HARNESS_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "Codex harness interface mismatch",
            );
        }
        if context.request.payload.len() > MAX_HARNESS_WIRE_BYTES {
            return ServiceResponse::error(
                "invalid_request",
                "Codex harness request exceeds bounds",
            );
        }
        let Ok(request) = context.request.payload_json::<HarnessRequest>() else {
            return ServiceResponse::error(
                "invalid_request",
                "Invalid typed Codex harness request",
            );
        };
        if request.validate().is_err()
            || request.instance.plugin_id != PLUGIN_ID
            || context.request.operation != request.kind.operation().as_str()
            || context.cancellation.is_cancelled()
        {
            return ServiceResponse::error(
                "invalid_request",
                "Codex harness admission mismatch or cancelled invocation",
            );
        }
        let kind = self.handle(&request, &context).unwrap_or_else(|error| {
            HarnessServiceResponseKind::Failure(match error {
                CodexError::Stale => HarnessFailure::StaleGeneration,
                CodexError::Unsupported => HarnessFailure::Unsupported,
                CodexError::Configuration | CodexError::Bounds | CodexError::Protocol => {
                    HarnessFailure::InvalidRequest
                }
                CodexError::Rejected => HarnessFailure::Policy,
                CodexError::Disconnected => HarnessFailure::Unavailable,
                CodexError::Uncertain => HarnessFailure::Uncertain,
            })
        });
        let response = HarnessServiceResponse {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            operation: request.kind.operation(),
            kind,
        };
        if response.validate_for(&request).is_err() {
            return ServiceResponse::error(
                "invalid_response",
                "Codex harness response failed contract validation",
            );
        }
        ServiceResponse::json(&response).unwrap_or_else(|_| {
            ServiceResponse::error("encode_failed", "Codex harness response encoding failed")
        })
    }
}

impl CodexHarnessPlugin {
    fn handle(
        &mut self,
        request: &HarnessRequest,
        context: &NativeServiceContext,
    ) -> Result<HarnessServiceResponseKind, CodexError> {
        let instance = &request.instance;
        match &request.kind {
            HarnessRequestKind::Inspect => {
                let cwd = context.config.config["auth_working_directory"]
                    .as_str()
                    .ok_or(CodexError::Configuration)?;
                let launch = launch(instance, cwd)?;
                let mut server = transport::AppServer::spawn(&launch)?;
                let native_version = server.initialize(&launch)?;
                Ok(HarnessServiceResponseKind::Inspection(HarnessInspection {
                    native_version,
                    executable_identity: instance.executable.clone(),
                    config_namespace: instance.config_namespace.clone(),
                    capabilities: capabilities(),
                }))
            }
            HarnessRequestKind::Open(open) => self.handle_open(instance, open, context),
            HarnessRequestKind::Submit { target, submission } => {
                Ok(HarnessServiceResponseKind::Receipt(
                    self.session(instance, confirmed(target)?)?
                        .submit(submission)?,
                ))
            }
            HarnessRequestKind::Respond { target, response } => {
                self.session(instance, confirmed(target)?)?
                    .respond(response)?;
                Ok(HarnessServiceResponseKind::Acknowledged {
                    mutation_id: response.response_id.clone(),
                })
            }
            HarnessRequestKind::Interrupt(control) => self.handle_interrupt(instance, control),
            HarnessRequestKind::Close(control) => {
                let binding = confirmed(&control.target)?;
                self.session(instance, binding)?;
                // Closing transport is not cancellation, settlement, or permission to repeat mutations.
                let owner = self
                    .owners
                    .get_mut(&binding.session_id)
                    .ok_or(CodexError::Stale)?;
                owner.session.take();
                Ok(HarnessServiceResponseKind::Acknowledged {
                    mutation_id: control.mutation_id.clone(),
                })
            }
            HarnessRequestKind::Observe(observe) => self.handle_observe(instance, observe),
            HarnessRequestKind::Reconcile(reconcile) => {
                let Some(binding) = &reconcile.binding else {
                    return Ok(HarnessServiceResponseKind::Reconciled(
                        HarnessReconciliation::Uncertain,
                    ));
                };
                // Metadata-only read cannot establish whether an unacknowledged mutation was applied.
                if reconcile.submission_id.is_some() || reconcile.open_id.is_some() {
                    return Ok(HarnessServiceResponseKind::Reconciled(
                        HarnessReconciliation::Uncertain,
                    ));
                }
                let status = self.session(instance, binding)?.reconcile()?;
                Ok(HarnessServiceResponseKind::Reconciled(
                    HarnessReconciliation::Known {
                        binding: binding.clone(),
                        receipt: None,
                        status,
                    },
                ))
            }
            kind => self.handle_auth(instance, kind, context),
        }
    }

    fn handle_auth(
        &mut self,
        instance: &HarnessInstance,
        kind: &HarnessRequestKind,
        context: &NativeServiceContext,
    ) -> Result<HarnessServiceResponseKind, CodexError> {
        match kind {
            HarnessRequestKind::AuthStatus { verify } => {
                if *verify {
                    return Err(CodexError::Unsupported);
                }
                Ok(HarnessServiceResponseKind::AuthStatus(
                    self.auth_owner(instance, context)?.status()?,
                ))
            }
            HarnessRequestKind::AuthBegin(auth) if auth.method_id == "api_key" => {
                let key = context
                    .credentials()
                    .get(&instance.auth.provider_id, "api_key")
                    .ok_or(CodexError::Configuration)?;
                let status = self.auth_owner(instance, context)?.vault_key(key)?;
                Ok(HarnessServiceResponseKind::AuthStep(HarnessAuthStep {
                    flow_id: auth.flow_id.clone(),
                    status,
                    interaction: None,
                    complete: true,
                }))
            }
            HarnessRequestKind::AuthBegin(auth) => Ok(HarnessServiceResponseKind::AuthStep(
                self.auth_owner(instance, context)?
                    .begin(&auth.flow_id, &auth.method_id)?,
            )),
            HarnessRequestKind::AuthContinue(auth) => {
                let owner = self
                    .auth
                    .get_mut(&instance.instance_id)
                    .ok_or(CodexError::Stale)?;
                if owner.instance != *instance || auth.input.is_some() {
                    return Err(CodexError::Unsupported);
                }
                Ok(HarnessServiceResponseKind::AuthStep(
                    owner.auth.poll(&auth.flow_id)?,
                ))
            }
            HarnessRequestKind::AuthCancel(auth) => {
                let owner = self
                    .auth
                    .get_mut(&instance.instance_id)
                    .ok_or(CodexError::Stale)?;
                if owner.instance != *instance {
                    return Err(CodexError::Stale);
                }
                Ok(HarnessServiceResponseKind::AuthStep(
                    owner.auth.cancel(&auth.flow_id)?,
                ))
            }
            _ => Err(CodexError::Unsupported),
        }
    }

    fn handle_open(
        &mut self,
        instance: &HarnessInstance,
        open: &HarnessOpenRequest,
        context: &NativeServiceContext,
    ) -> Result<HarnessServiceResponseKind, CodexError> {
        if let Some(owner) = self.owners.get(&open.session_id) {
            if owner.instance != *instance || owner.open != *open {
                return Err(CodexError::Stale);
            }
            return owner
                .session
                .as_ref()
                .map(|session| HarnessServiceResponseKind::Opened(session.binding().clone()))
                .ok_or(CodexError::Uncertain);
        }
        if self.owners.len() >= 32 {
            return Err(CodexError::Bounds);
        }
        let launch = launch(instance, &open.working_directory)?;
        let binding = open
            .resume
            .clone()
            .unwrap_or_else(|| HarnessSessionBinding {
                schema_version: HARNESS_SCHEMA_VERSION,
                session_id: open.session_id.clone(),
                instance_id: instance.instance_id.clone(),
                native_session_id: open.open_id.clone(),
                native_version: "unverified-app-server-v2".to_owned(),
                generation: open.generation,
            });
        let key = if instance.auth.custody == AuthCustodyMode::VaultManaged {
            Some(
                context
                    .credentials()
                    .get(&instance.auth.provider_id, "api_key")
                    .ok_or(CodexError::Configuration)?,
            )
        } else {
            None
        };
        // Retain an uncertain open before any native mutation; identical retry never spawns again.
        self.owners.insert(
            open.session_id.clone(),
            Owner {
                instance: instance.clone(),
                open: open.clone(),
                session: None,
                page_number: 0,
                next: None,
                previous: None,
                interrupt: None,
            },
        );
        let session = CodexSession::open_admitted(
            &launch,
            binding,
            open.resume.is_some(),
            key,
            open.parent_assistance.as_ref(),
        )?;
        let binding = session.binding().clone();
        self.owners
            .get_mut(&open.session_id)
            .ok_or(CodexError::Stale)?
            .session = Some(session);
        Ok(HarnessServiceResponseKind::Opened(binding))
    }

    fn handle_interrupt(
        &mut self,
        instance: &HarnessInstance,
        control: &HarnessControlRequest,
    ) -> Result<HarnessServiceResponseKind, CodexError> {
        let binding = confirmed(&control.target)?;
        self.session(instance, binding)?;
        let owner = self
            .owners
            .get_mut(&binding.session_id)
            .ok_or(CodexError::Stale)?;
        if let Some((mutation_id, result)) = &owner.interrupt {
            if *mutation_id == control.mutation_id {
                (*result)?;
                return Ok(HarnessServiceResponseKind::Acknowledged {
                    mutation_id: control.mutation_id.clone(),
                });
            }
            // A second interrupt identity needs fresh host reconciliation rather than
            // silently targeting a later turn after a delayed duplicate.
            return Err(CodexError::Uncertain);
        }
        owner.interrupt = Some((control.mutation_id.clone(), Err(CodexError::Uncertain)));
        let result = owner
            .session
            .as_mut()
            .ok_or(CodexError::Uncertain)?
            .interrupt_active(binding.generation);
        owner.interrupt = Some((control.mutation_id.clone(), result));
        result?;
        Ok(HarnessServiceResponseKind::Acknowledged {
            mutation_id: control.mutation_id.clone(),
        })
    }

    fn handle_observe(
        &mut self,
        instance: &HarnessInstance,
        observe: &HarnessObserveRequest,
    ) -> Result<HarnessServiceResponseKind, CodexError> {
        let binding = confirmed(&observe.target)?;
        self.session(instance, binding)?;
        let owner = self
            .owners
            .get_mut(&binding.session_id)
            .ok_or(CodexError::Stale)?;
        if let Some((after, page)) = &owner.previous
            && observe.after == *after
        {
            return Ok(HarnessServiceResponseKind::Observations(page.clone()));
        }
        if observe.after != owner.next {
            return Err(CodexError::Uncertain);
        }
        let (observations, completeness) = owner
            .session
            .as_mut()
            .ok_or(CodexError::Uncertain)?
            .observe(usize::from(observe.limit.min(4)))?;
        owner.page_number = owner.page_number.checked_add(1).ok_or(CodexError::Bounds)?;
        let next_after = Some(format!(
            "codex-page-{}-{}",
            binding.generation, owner.page_number
        ));
        let status = owner
            .session
            .as_ref()
            .ok_or(CodexError::Uncertain)?
            .status();
        let page = HarnessObservationPage {
            observations,
            next_after: next_after.clone(),
            completeness,
            status,
        };
        owner.previous = Some((observe.after.clone(), page.clone()));
        owner.next = next_after;
        Ok(HarnessServiceResponseKind::Observations(page))
    }

    fn session(
        &mut self,
        instance: &HarnessInstance,
        binding: &HarnessSessionBinding,
    ) -> Result<&mut CodexSession, CodexError> {
        let owner = self
            .owners
            .get_mut(&binding.session_id)
            .ok_or(CodexError::Uncertain)?;
        if owner.instance != *instance {
            return Err(CodexError::Stale);
        }
        let session = owner.session.as_mut().ok_or(CodexError::Uncertain)?;
        if session.binding() != binding {
            return Err(CodexError::Stale);
        }
        Ok(session)
    }

    fn auth_owner(
        &mut self,
        instance: &HarnessInstance,
        context: &NativeServiceContext,
    ) -> Result<&mut CodexAuth, CodexError> {
        if !self.auth.contains_key(&instance.instance_id) {
            if self.auth.len() >= 16 {
                return Err(CodexError::Bounds);
            }
            // Host must supply an absolute admitted auth cwd; do not inherit the plugin process cwd.
            let cwd = context.config.config["auth_working_directory"]
                .as_str()
                .ok_or(CodexError::Configuration)?;
            let auth = CodexAuth::open(&launch(instance, cwd)?, instance.auth.clone())?;
            self.auth.insert(
                instance.instance_id.clone(),
                AuthOwner {
                    instance: instance.clone(),
                    auth,
                },
            );
        }
        let owner = self
            .auth
            .get_mut(&instance.instance_id)
            .ok_or(CodexError::Stale)?;
        if owner.instance != *instance {
            return Err(CodexError::Stale);
        }
        Ok(&mut owner.auth)
    }
}

const fn confirmed(target: &HarnessSessionTarget) -> Result<&HarnessSessionBinding, CodexError> {
    match target {
        HarnessSessionTarget::Confirmed(binding) => Ok(binding),
        HarnessSessionTarget::Prepared(_) => Err(CodexError::Unsupported),
    }
}

fn launch(instance: &HarnessInstance, cwd: &str) -> Result<Launch, CodexError> {
    if instance.auth.provider_id != "bcode.codex"
        || instance.config_namespace != instance.auth.credential_namespace
        || instance.auth.custody == AuthCustodyMode::External
    {
        return Err(CodexError::Configuration);
    }
    Ok(Launch {
        executable: PathBuf::from(&instance.executable),
        codex_home: PathBuf::from(&instance.config_namespace),
        working_directory: PathBuf::from(cwd),
        ephemeral_credentials: instance.auth.custody == AuthCustodyMode::VaultManaged,
        timeout: Duration::from_secs(30),
    })
}

fn capabilities() -> HarnessCapabilities {
    let constrained = |constraint: &str| HarnessCapability {
        support: HarnessSupportLevel::Constrained,
        constraints: vec![constraint.to_owned()],
    };
    let unsupported = || HarnessCapability {
        support: HarnessSupportLevel::Unsupported,
        constraints: Vec::new(),
    };
    HarnessCapabilities {
        resume: constrained("Exact binding and top-level version probe; metadata-only resume"),
        steering: constrained(
            "Expected native turn ID required; NextTurn only once on a fresh local-only non-resumed thread; later conditional start unsupported",
        ),
        approval: constrained(
            "Exact generation callback; file approval requires retained bounded native diff; root/network/additional-permission grants declined",
        ),
        questions: constrained(
            "One question per callback; multiple-question responses unsupported",
        ),
        assistance: constrained(
            "Explicit bcode_parent_assistance native dynamic tool only; host authorizes response",
        ),
        interrupt: constrained("Requires retained active turn; acknowledgment is not settlement"),
        authoritative_settlement: constrained(
            "Exact correlated successful native turn, verified multi_agent=false and no gap; never assignment acceptance",
        ),
        background_continuation: unsupported(),
        authentication: constrained(
            "Native browser/device; scoped vault key requires verified ephemeral mode; no remote verification",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_harness_models::HarnessPreparedSession;
    #[test]
    fn prepared_target_is_not_promoted_to_an_invented_codex_thread() {
        let target = HarnessSessionTarget::Prepared(HarnessPreparedSession {
            open_id: "open".into(),
            session_id: "session".into(),
            instance_id: "instance".into(),
            generation: 1,
            native_version: "0.0.0".into(),
            requested_native_session_id: None,
        });
        assert_eq!(confirmed(&target), Err(CodexError::Unsupported));
    }
}

/// Return this plugin's native static registration using the same manifest as dynamic loading.
#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(
        CodexHarnessPlugin,
        include_str!("../bcode-plugin.toml")
    )
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(CodexHarnessPlugin, include_str!("../bcode-plugin.toml"));
