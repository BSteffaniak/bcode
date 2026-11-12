//! Startup model for an approved connection to another coding agent's credentials.
//!
//! Discovery and approval happen in the shared Setup picker or `bcode auth connect`; this saves
//! the provider's recommended model together with the approved connection, so setup takes one
//! confirmation. The model can be changed later with `/model`.

use super::CliError;
use bcode_provider_auth_models::external::ExternalAuthCandidate;

/// Save the provider's top recommended model with an approved connection, never credential
/// material. Returns the saved model, or `None` when an explicit selection already exists or the
/// provider recommends no model for the connection's method.
///
/// # Errors
/// Fails when configuration, the plugin host, or the startup selection cannot be loaded or saved.
pub fn save_recommended_external_model(
    candidate: &ExternalAuthCandidate,
) -> Result<Option<String>, CliError> {
    let config = bcode_config::load_config()?;
    if super::has_explicit_selection(&config) {
        return Ok(None);
    }
    let mut host = super::load_cli_plugin_host()?;
    let selections = if host
        .auth_provider(&candidate.target.provider_id)
        .is_some_and(|provider| provider.plugin_id == candidate.target.plugin_id)
    {
        super::model_choices(
            &host,
            &config,
            &candidate.target.provider_id,
            &candidate.target.method_id,
            None,
            None,
        )
    } else {
        Vec::new()
    };
    host.deactivate_all()?;
    // Providers return recommendations first.
    let Some(mut selection) = selections.into_iter().next() else {
        return Ok(None);
    };
    selection.external_auth_binding = Some(candidate.binding_id.clone());
    bcode_config::startup::save(&selection).map_err(|_| {
        CliError::InvalidArguments(
            "Connection approved, but the startup model could not be saved.".into(),
        )
    })?;
    Ok(Some(selection.model_id))
}
