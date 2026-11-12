//! Model selection for an approved connection to another coding agent's credentials.
//!
//! Discovery and approval happen in the shared Setup picker; this saves the chosen model
//! together with the approved connection for startup.

use super::CliError;
use bcode_provider_auth_models::external::ExternalAuthCandidate;

/// Save a model choice together with an approved connection, never credential material.
/// Returns false when no model was chosen or an explicit selection already exists.
pub async fn select_external_model_in_shell(
    shell: &mut bcode_tui::TerminalShell,
    candidate: &ExternalAuthCandidate,
) -> Result<bool, CliError> {
    let config = bcode_config::load_config()?;
    if super::has_explicit_selection(&config) {
        return Ok(false);
    }
    let mut host = super::load_cli_plugin_host()?;
    let mut selections = if host
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
    for selection in &mut selections {
        selection.external_auth_binding = Some(candidate.binding_id.clone());
    }
    if selections.is_empty() {
        return Ok(false);
    }
    let outcome = bcode_tui::run_backend_selection_in_shell(
        shell,
        bcode_tui::QuickOnboardingInput {
            choices: selections
                .iter()
                .map(|selection| bcode_tui::QuickConnectionChoice {
                    label: format!("Use {} with {}", selection.model_id, candidate.application),
                    detail:
                        "Saved as your startup model. No session starts from this choice alone."
                            .into(),
                })
                .collect(),
            recovery: Some(
                "Choose a model to use with this connection, or close to keep only the approval."
                    .into(),
            ),
        },
    )
    .await?;
    super::consent_enrolled_selection(&outcome, &selections, |selection| {
        bcode_config::startup::save(selection)
            .map(|_| ())
            .map_err(|_| "Connection approved, but the startup model could not be saved.")
    })
    .map_err(|message| CliError::InvalidArguments(message.into()))
}
