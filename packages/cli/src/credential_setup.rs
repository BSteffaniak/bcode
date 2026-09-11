//! CLI presentation for provider-owned credential discovery and explicit import.

use super::{CliError, load_cli_plugin_host, registered_auth_provider, selected_auth_method};
use bcode_provider_auth_models::{AuthCredentialSource, AuthMethodContribution};
use std::collections::BTreeMap;
use std::io::{IsTerminal as _, Write as _};
use std::path::PathBuf;

pub async fn run_setup(discovery_enabled: bool) -> Result<bool, CliError> {
    match Box::pin(bcode_tui::run_setup_screen(discovery_enabled)).await? {
        bcode_settings::SetupContinuation::Launch => Ok(true),
        bcode_settings::SetupContinuation::Close => Ok(false),
        _ => Err(CliError::InvalidArguments(
            "Setup editors must remain inside the terminal runtime".to_owned(),
        )),
    }
}

pub fn validate_launch_selection() -> Result<(), CliError> {
    let selection = bcode_config::load_config()?.resolved_model_selection();
    selection
        .validate_selection()
        .map_err(|error| CliError::InvalidArguments(error.to_string()))
}

pub fn edit_setting(
    file: PathBuf,
    keys: &[String],
    value: Option<&str>,
    remove: bool,
) -> Result<(), CliError> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(CliError::InvalidArguments(
            "Settings editing requires an interactive review".to_owned(),
        ));
    }
    let value = match (remove, value) {
        (true, None) => None,
        (false, Some(value)) => {
            let parsed: toml::Value = toml::from_str(&format!("value = {value}"))
                .map_err(|_| CliError::InvalidArguments("Value must be valid TOML".to_owned()))?;
            Some(
                parsed
                    .get("value")
                    .cloned()
                    .ok_or_else(|| CliError::InvalidArguments("Missing value".to_owned()))?,
            )
        }
        _ => {
            return Err(CliError::InvalidArguments(
                "Specify --value or --remove".to_owned(),
            ));
        }
    };
    let edit = bcode_config::edit::plan_field_edit(file, keys, value)?;
    println!(
        "Configuration file: {}\nField: {}\nOperation: {}",
        edit.path().display(),
        keys.join(" / "),
        if remove {
            "Remove override"
        } else {
            "Set supplied value"
        }
    );
    println!(
        "Higher-priority configuration may override this file. Changes apply when configuration is next loaded."
    );
    print!("Type apply to confirm: ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if answer.trim() == "apply" {
        edit.apply()?;
        println!("Configuration updated.");
    }
    Ok(())
}

pub fn discover_for_onboarding(enabled: bool) -> Result<(), CliError> {
    if enabled {
        discover_auth_credentials()
    } else {
        println!("Automatic credential discovery is disabled.");
        Ok(())
    }
}

pub fn handle_command(command: super::AuthCommand) -> Result<(), CliError> {
    match command {
        super::AuthCommand::Discover => discover_auth_credentials(),
        super::AuthCommand::Import {
            provider,
            method,
            credential,
            source,
            profile,
            vault,
        } => import_auth_credential(
            &provider,
            &method,
            &credential,
            source,
            profile.as_deref(),
            vault,
        ),
        _ => Err(CliError::InvalidArguments(
            "Expected credential discovery or import".to_owned(),
        )),
    }
}

fn authorized_home() -> Result<PathBuf, CliError> {
    std::env::home_dir().ok_or_else(|| {
        CliError::LoginProfile(
            "Home directory is unavailable; credential discovery is disabled.".to_owned(),
        )
    })
}

pub fn discover_auth_credentials() -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    if !config
        .onboarding
        .credential_discovery_enabled(false, &bcode_config::ProcessConfigEnvironment)
    {
        println!("Automatic credential discovery is disabled.");
        return Ok(());
    }
    let home = authorized_home()?;
    let mut host = load_cli_plugin_host()?;
    for provider in host.auth_provider_registry().providers() {
        for method in &provider.contribution.methods {
            let AuthMethodContribution::SecretFields { fields, .. } = method else {
                continue;
            };
            for field in fields {
                for (index, status) in bcode_provider_auth::discovery::discover(true, &home, field)
                {
                    if status == bcode_provider_auth::discovery::CredentialSourceStatus::Missing {
                        continue;
                    }
                    println!(
                        "{} / {} / {} / source {index}: {} — {status:?}",
                        provider.contribution.provider_id,
                        method.method_id(),
                        field.credential_id,
                        source_label(&field.discovery_sources[index])
                    );
                }
            }
        }
    }
    host.deactivate_all()?;
    println!(
        "Use bcode auth import PROVIDER --source INDEX to review a copy into sshenv. OAuth sign-ins require a supported fresh login; tokens are not copied."
    );
    Ok(())
}

fn source_label(source: &AuthCredentialSource) -> String {
    match source {
        AuthCredentialSource::Environment { name } => format!("environment {name}"),
        AuthCredentialSource::JsonFile {
            application,
            relative_path,
            ..
        } => format!("{application}: ~/{relative_path}"),
    }
}

pub fn import_auth_credential(
    provider_id: &str,
    method_id: &str,
    credential_id: &str,
    source: usize,
    profile: Option<&str>,
    vault: Option<PathBuf>,
) -> Result<(), CliError> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(CliError::LoginProfile(
            "Credential import requires an interactive review.".to_owned(),
        ));
    }
    let mut host = load_cli_plugin_host()?;
    let result = import_with_host(
        &host,
        provider_id,
        method_id,
        credential_id,
        source,
        profile,
        vault,
    );
    let cleanup = host.deactivate_all().map_err(CliError::from);
    result.and(cleanup)
}

fn import_with_host(
    host: &bcode_plugin::PluginHost,
    provider_id: &str,
    method_id: &str,
    credential_id: &str,
    source: usize,
    profile: Option<&str>,
    vault: Option<PathBuf>,
) -> Result<(), CliError> {
    let provider = registered_auth_provider(host, provider_id)?;
    let method = selected_auth_method(&provider, Some(method_id))?;
    let AuthMethodContribution::SecretFields { fields, .. } = method else {
        return Err(CliError::LoginProfile(
            "Sign-in tokens cannot be copied. Use auth login for this method.".to_owned(),
        ));
    };
    let field = fields
        .iter()
        .find(|field| field.credential_id == credential_id)
        .ok_or_else(|| {
            CliError::LoginProfile("Credential is not declared by this provider.".to_owned())
        })?;
    let declaration = field.discovery_sources.get(source).ok_or_else(|| {
        CliError::LoginProfile("Source is not declared by this provider.".to_owned())
    })?;
    let (resolved, persist_runtime) =
        super::resolve_or_prepare_auth_profile(&provider, method, profile, vault, None)?;
    let lifecycle = bcode_provider_auth::lifecycle::AuthVaultLifecycle::new(
        &resolved,
        provider_id,
        &provider.plugin_id,
        method,
    )
    .map_err(|_| {
        CliError::LoginProfile(
            "Destination profile ownership or method is inconsistent.".to_owned(),
        )
    })?;
    println!("Copy from: {}", source_label(declaration));
    println!(
        "Provider: {provider_id}\nProfile: {}\nVault: {}",
        resolved.profile_name,
        resolved
            .profile
            .settings
            .get("vault")
            .map_or("(unresolved)", String::as_str)
    );
    println!("The source will not be modified. No remote verification is performed.");
    print!("Type import to confirm: ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if answer.trim() != "import" {
        return Ok(());
    }
    let value = bcode_provider_auth::discovery::read_selected(&authorized_home()?, field, source)
        .map_err(|_| {
            CliError::LoginProfile(
                "Selected credential source is unavailable or incompatible.".to_owned(),
            )
        })?
        .ok_or_else(|| {
            CliError::LoginProfile("Selected credential is no longer present.".to_owned())
        })?;
    lifecycle
        .import_new(BTreeMap::from([(
            credential_id.to_owned(),
            value.to_string(),
        )]))
        .map_err(|_| {
            CliError::LoginProfile(
                "Import could not be completed safely; inspect the destination before retrying."
                    .to_owned(),
            )
        })?;
    if persist_runtime {
        super::persist_prepared_runtime_profile(&resolved)?;
    }
    println!(
        "Credential copied into the owned sshenv profile. Connection has not been remotely verified."
    );
    Ok(())
}
