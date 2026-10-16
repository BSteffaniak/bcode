//! Explicit subscription enrollment into the custody selected by a broker operator.
//! Command declaration/argument parsing remains in the CLI root. No secrets are command arguments.
//!
//! Coordinator command contract (all operations preserve selected config/profile/state root):
//! * `initialize`: `--provider <id> --profile <name> --directory <absolute-path>`
//!   `--identity-file <absolute-path> --recipient <public-age-recipient>`.
//!   Requires Linux, an interactive terminal, and the literal confirmation `CONNECT`.
//! * `reconnect`: the same provider/profile/directory/identity-file arguments, without a recipient.
//!   Requires the broker stopped, previous provider work settled/cancelled, and `RECONNECT`.
//! * `readiness`: the same provider/profile/directory/identity-file arguments, without a recipient.
//!   Performs no remote exchange, asks no confirmation, and does not repair existing state.
//!
//! The declared profile must explicitly pin the enabled provider plugin owner, subscription method,
//! sshenv backend, and `device_seal = "off"`. No API key, token, identity value or flow state is an
//! argument. The identity file contains one plain age secret identity and must be owner-only.
//! Native age wrapping is provided by the pinned sshenv vault. Preserve initialization failures
//! without substituting SSH/device custody or manually constructing vault ciphertext.

use crate::CliError;
use std::path::PathBuf;

/// Explicit operation; readiness never performs remote enrollment or repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SetupOperation {
    /// Create a new custody directory, then connect through the registered interactive method.
    Initialize,
    /// Connect anew after explicit operator confirmation that prior provider work has settled.
    Reconnect,
    /// Inspect local readiness without a remote provider call.
    Readiness,
}

/// Non-secret command inputs. The profile must already declare its provider/plugin ownership,
/// subscription method, sshenv backend and explicit `device_seal = "off"` policy.
pub struct SetupOptions {
    /// Registered authentication provider ID from an enabled plugin.
    pub provider_id: String,
    /// Exact existing profile name; no ambient/default selection.
    pub profile: String,
    /// Absolute retained custody directory, with operator-controlled ancestors.
    pub directory: PathBuf,
    /// Absolute owner-only file containing one age secret identity.
    pub identity_file: PathBuf,
    /// Public age recipient, required for initialization only.
    pub recipient: Option<String>,
}

/// Run the connect-once custody application operation using the selected CLI configuration.
/// Does not publish or rewrite configuration, copy legacy ciphertext, or probe before commit.
///
/// # Errors
/// Unsupported platforms, unpinned profiles, unsupported methods/factors, ownership failures,
/// and unresolved provider/publication outcomes fail closed. Failed enrollment requires reconnect;
/// interrupted storage initialization requires maintenance, not automatic deletion or recreation.
#[cfg(target_os = "linux")]
pub async fn run(operation: SetupOperation, options: SetupOptions) -> Result<(), CliError> {
    #[cfg(target_os = "linux")]
    {
        let mut host = crate::load_cli_plugin_host()?;
        let result = run_with_host(&host, operation, options).await;
        let cleanup = host.deactivate_all().map_err(CliError::from);
        result.and(cleanup)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let SetupOptions {
            provider_id,
            profile,
            directory,
            identity_file,
            recipient,
        } = options;
        let _ = (
            operation,
            provider_id,
            profile,
            directory,
            identity_file,
            recipient,
        );
        Err(setup_error(
            "Broker service custody setup is supported only on Linux.",
        ))
    }
}

#[cfg(not(target_os = "linux"))]
pub fn run(
    operation: SetupOperation,
    options: SetupOptions,
) -> std::future::Ready<Result<(), CliError>> {
    let SetupOptions {
        provider_id,
        profile,
        directory,
        identity_file,
        recipient,
    } = options;
    let _ = (
        operation,
        provider_id,
        profile,
        directory,
        identity_file,
        recipient,
    );
    std::future::ready(Err(setup_error(
        "Broker service custody setup is supported only on Linux.",
    )))
}

fn setup_error(message: &str) -> CliError {
    CliError::LoginProfile(message.to_owned())
}

#[cfg(target_os = "linux")]
struct FreshKey;

#[cfg(target_os = "linux")]
impl bcode_provider_auth::operations::AuthCustodyKeySource for FreshKey {
    fn generate(
        &self,
    ) -> Result<zeroize::Zeroizing<[u8; 32]>, bcode_provider_auth::lifecycle::AuthVaultLifecycleError>
    {
        Ok(zeroize::Zeroizing::new(
            *sshenv_vault::crypto::generate_data_key(),
        ))
    }
}

#[cfg(target_os = "linux")]
async fn run_with_host(
    host: &bcode_plugin::PluginHost,
    operation: SetupOperation,
    options: SetupOptions,
) -> Result<(), CliError> {
    use bcode_provider_auth::operations::{CustodyEnrollmentMode, CustodyEnrollmentReadiness};
    use std::io::Write as _;

    let provider = crate::registered_auth_provider(host, &options.provider_id)?;
    let (retained, flow_operation) = acquire_custody(&provider, operation, &options)?;
    if operation == SetupOperation::Readiness {
        let readiness = retained.enrollment_readiness().map_err(|_| {
            setup_error("Custody policy, identity or durable state requires maintenance.")
        })?;
        let message = match readiness {
            CustodyEnrollmentReadiness::Empty => {
                "Custody is empty; subscription enrollment is required."
            }
            CustodyEnrollmentReadiness::Ready => {
                "Custody is locally ready; this is not a remote provider verification."
            }
            CustodyEnrollmentReadiness::ReconnectRequired => {
                "Custody is blocked by an unresolved exchange; explicitly reconnect after settling prior work."
            }
        };
        writeln!(std::io::stdout(), "{message}")?;
        return if readiness == CustodyEnrollmentReadiness::Ready {
            Ok(())
        } else {
            Err(setup_error(message))
        };
    }
    let mode = match operation {
        SetupOperation::Initialize => CustodyEnrollmentMode::Initialize,
        SetupOperation::Reconnect => CustodyEnrollmentMode::Reconnect,
        SetupOperation::Readiness => unreachable!("readiness returned before enrollment"),
    };
    let mut enrollment = retained.begin_enrollment(mode)
        .map_err(|_| setup_error("Enrollment was not admitted. Unsupported factors, competing ownership or unresolved durable state require maintenance/reconnect."))?;
    drive_enrollment(host, &provider.plugin_id, &flow_operation, &mut enrollment).await?;
    retained.complete_enrollment(enrollment)
        .map_err(|_| setup_error("Credential publication is unresolved; custody remains blocked. Do not retry the exchange; explicitly reconnect after settling prior work."))?;
    // This read happens only after the durable commit. No implicit remote probe uses a rotating
    // subscription, legacy profile, or different custody owner.
    if retained
        .enrollment_readiness()
        .map_err(|_| setup_error("Committed custody requires maintenance."))?
        != CustodyEnrollmentReadiness::Ready
    {
        return Err(setup_error(
            "Enrollment did not produce locally ready custody.",
        ));
    }
    writeln!(
        std::io::stdout(),
        "Subscription enrollment committed to retained custody. Local readiness passed; no remote probe was performed."
    )?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn acquire_custody(
    provider: &bcode_plugin::RegisteredAuthProvider,
    operation: SetupOperation,
    options: &SetupOptions,
) -> Result<
    (
        bcode_provider_auth::operations::RetainedAuthRequestCustody,
        String,
    ),
    CliError,
> {
    let config = bcode_config::load_config()?;
    let runtime = bcode_config::load_runtime_auth_subscriptions();
    // Explicit resolution through the shared application boundary: no profile preparation,
    // pool rotation, inferred default profile, or legacy-vault lookup.
    let resolved = bcode_provider_auth::resolve_auth_provider_profile(
        &config,
        &options.provider_id,
        &provider.plugin_id,
        Some(&options.profile),
        &runtime,
    )
    .map_err(|_| {
        setup_error("Setup requires an explicitly configured, owner-pinned auth profile.")
    })?;
    validate_profile(&resolved, &options.provider_id, &provider.plugin_id)?;
    let method = crate::resolved_auth_method(provider, &resolved)?.clone();
    let bcode_provider_auth_models::AuthMethodContribution::Interactive {
        operation: flow_operation,
        ..
    } = &method
    else {
        return Err(setup_error(
            "Broker custody requires a registered subscription enrollment method, not an API key.",
        ));
    };
    let flow_operation = flow_operation.clone();
    let retained = open_selected_custody(provider, operation, options, resolved, method)?;
    Ok((retained, flow_operation))
}

#[cfg(target_os = "linux")]
fn open_selected_custody(
    provider: &bcode_plugin::RegisteredAuthProvider,
    operation: SetupOperation,
    options: &SetupOptions,
    resolved: bcode_provider_auth::ResolvedAuthProfile,
    method: bcode_provider_auth_models::AuthMethodContribution,
) -> Result<bcode_provider_auth::operations::RetainedAuthRequestCustody, CliError> {
    use bcode_provider_auth::{
        custody_storage::CredentialCustodyStorage, operations::RetainedAuthRequestCustody,
    };
    if !options.directory.is_absolute() {
        return Err(setup_error(
            "Custody directory must be an absolute path with trusted ancestors.",
        ));
    }
    let identity = bcode_provider_auth::operations::read_custody_identity(&options.identity_file)
        .map_err(|_| setup_error("Custody identity must be one age identity in an absolute owner-only regular file, without symlinks."))?;
    if operation != SetupOperation::Readiness {
        confirm(operation)?;
    }
    match operation {
        SetupOperation::Initialize => {
            let recipient = options.recipient.as_deref().ok_or_else(|| {
                setup_error(
                    "Initialization requires the public age recipient matching the identity file.",
                )
            })?;
            RetainedAuthRequestCustody::initialize_enrollment(
                &options.directory, resolved, &options.provider_id, &provider.plugin_id,
                method, recipient, vec![identity], &FreshKey,
            ).map_err(|_| setup_error("Custody initialization failed; preserve existing state for maintenance. No provider exchange was admitted."))
        }
        SetupOperation::Reconnect | SetupOperation::Readiness => {
            let storage = CredentialCustodyStorage::open(&options.directory)
                .map_err(|_| setup_error("Custody is unavailable or owned by another process; stop the broker before setup. Preserve interrupted state for maintenance."))?;
            RetainedAuthRequestCustody::new(
                storage,
                resolved,
                &options.provider_id,
                &provider.plugin_id,
                method,
                vec![identity],
                None,
            )
            .map_err(|_| setup_error("Custody profile ownership or method is invalid."))
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn validate_profile(
    resolved: &bcode_provider_auth::ResolvedAuthProfile,
    provider: &str,
    owner: &str,
) -> Result<(), CliError> {
    if resolved.provider_id != provider
        || resolved.owner_plugin_id != owner
        || resolved.profile.provider_id.as_deref() != Some(provider)
        || resolved.profile.owner_plugin_id.as_deref() != Some(owner)
        || resolved.profile.backend != "sshenv"
        || resolved
            .profile
            .settings
            .get("device_seal")
            .map(String::as_str)
            != Some("off")
    {
        return Err(setup_error(
            "Setup requires explicit matching provider/plugin ownership, sshenv and device_seal = off. Sealed/native/passphrase custody is unsupported.",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn confirm(operation: SetupOperation) -> Result<(), CliError> {
    use std::io::{IsTerminal as _, Write as _};
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(setup_error(
            "Subscription setup requires an interactive terminal; credentials and consent cannot be passed in arguments.",
        ));
    }
    let message = match operation {
        SetupOperation::Initialize => {
            "Create fresh age-only custody and enroll this subscription? Type CONNECT to authorize: "
        }
        SetupOperation::Reconnect => {
            "Stop the broker and settle/cancel all prior provider work before reconnecting. Type RECONNECT to confirm this and authorize a NEW subscription enrollment: "
        }
        SetupOperation::Readiness => return Ok(()),
    };
    print!("{message}");
    std::io::stdout().flush()?;
    let expected = if operation == SetupOperation::Initialize {
        "CONNECT"
    } else {
        "RECONNECT"
    };
    if crate::read_stdin_line()?.trim() != expected {
        return Err(setup_error(
            "Subscription setup was not authorized; no remote exchange was admitted.",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn drive_enrollment(
    host: &bcode_plugin::PluginHost,
    owner: &str,
    operation: &str,
    enrollment: &mut bcode_provider_auth::operations::CustodyEnrollment,
) -> Result<(), CliError> {
    drive_with(enrollment, |request| {
        host.invoke_service_json::<_, bcode_provider_auth_models::AuthFlowResponse>(
            owner, bcode_provider_auth_models::AUTH_INTERFACE_ID, operation, request,
        ).map_err(|_| setup_error("Provider enrollment outcome is unresolved; do not reissue the exchange. Explicit reconnect is required."))
    }, |effect| {
        crate::write_auth_effect(&mut std::io::stdout(), effect)?;
        if matches!(effect, bcode_provider_auth_models::AuthFlowEffect::Prompt { .. }) {
            Ok(Some(crate::read_stdin_line()?))
        } else {
            Ok(None)
        }
    }).await
}

#[cfg(target_os = "linux")]
async fn drive_with(
    enrollment: &mut bcode_provider_auth::operations::CustodyEnrollment,
    mut invoke: impl FnMut(
        &bcode_provider_auth_models::AuthFlowRequest,
    ) -> Result<bcode_provider_auth_models::AuthFlowResponse, CliError>,
    mut present: impl FnMut(
        &bcode_provider_auth_models::AuthFlowEffect,
    ) -> Result<Option<String>, CliError>,
) -> Result<(), CliError> {
    use bcode_provider_auth_models::{AuthFlowEffect, AuthFlowStatus};
    loop {
        let request = enrollment
            .next_request()
            .map_err(|_| setup_error("Enrollment invocation is unresolved; reconnect required."))?;
        let response = invoke(request)?;
        // Accept before rendering. Opaque continuation state and credential output never enter
        // CLI diagnostics, logs or ordinary login's legacy replace_owned path.
        let progress = enrollment.accept(response).map_err(|_| {
            setup_error("Provider enrollment response is invalid; custody remains fenced.")
        })?;
        match progress.status {
            AuthFlowStatus::Succeeded => return Ok(()),
            AuthFlowStatus::Failed | AuthFlowStatus::Cancelled => {
                return Err(setup_error(
                    "Provider enrollment did not succeed. Custody remains blocked until explicit reconnect.",
                ));
            }
            AuthFlowStatus::Pending => {}
        }
        for effect in progress.effects {
            let answer = present(&effect)?;
            match &effect {
                AuthFlowEffect::Prompt { .. } => enrollment
                    .answer(
                        &effect,
                        answer.ok_or_else(|| {
                            setup_error(
                                "Enrollment prompt was not answered; custody remains fenced.",
                            )
                        })?,
                    )
                    .map_err(|_| {
                        setup_error("Invalid enrollment answer; custody remains fenced.")
                    })?,
                AuthFlowEffect::Wait { millis } => {
                    tokio::time::sleep(std::time::Duration::from_millis(*millis)).await;
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn setup_operations_have_exact_parser_values() {
        use clap::ValueEnum as _;
        for (text, expected) in [
            ("initialize", SetupOperation::Initialize),
            ("reconnect", SetupOperation::Reconnect),
            ("readiness", SetupOperation::Readiness),
        ] {
            assert_eq!(SetupOperation::from_str(text, false).unwrap(), expected);
        }
        assert!(SetupOperation::from_str("import", false).is_err());
    }

    #[test]
    fn setup_rejects_implicit_and_foreign_profile_ownership() {
        let mut resolved = bcode_provider_auth::ResolvedAuthProfile {
            profile_name: "subscription".into(),
            provider_id: "provider".into(),
            owner_plugin_id: "plugin".into(),
            source: bcode_provider_auth::AuthProfileSource::Declarative,
            profile: bcode_config::AuthProfileConfig {
                backend: "sshenv".into(),
                scheme: Some("device".into()),
                provider_id: Some("provider".into()),
                owner_plugin_id: Some("plugin".into()),
                settings: BTreeMap::from([("device_seal".into(), "off".into())]),
                ..Default::default()
            },
        };
        assert!(validate_profile(&resolved, "provider", "plugin").is_ok());
        assert!(validate_profile(&resolved, "provider", "foreign-plugin").is_err());
        resolved.profile.owner_plugin_id = None;
        assert!(validate_profile(&resolved, "provider", "plugin").is_err());
        resolved.profile.owner_plugin_id = Some("plugin".into());
        for policy in [None, Some("preferred"), Some("required")] {
            resolved.profile.settings.clear();
            if let Some(policy) = policy {
                resolved
                    .profile
                    .settings
                    .insert("device_seal".into(), policy.into());
            }
            assert!(validate_profile(&resolved, "provider", "plugin").is_err());
        }
    }
}
