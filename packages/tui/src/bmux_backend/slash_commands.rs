//! Backend-agnostic slash commands for the BMUX backend.

use bcode_client::BcodeClient;
use bcode_session_models::SessionId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SlashCommandOutcome {
    /// Command was handled in-place.
    Handled(String),
    /// Switch to a session.
    SwitchSession(SessionId),
    /// Open the session picker.
    PickSession,
    /// Open model picker.
    PickModel,
    /// Open skill picker.
    PickSkill,
    /// Show a system note.
    SystemNote(String),
    /// Unknown slash command.
    Unknown(String),
}

/// Execute a slash command.
///
/// # Errors
///
/// Returns an error when the daemon rejects a requested operation.
pub(super) async fn execute(
    client: &BcodeClient,
    session_id: SessionId,
    message: &str,
) -> Result<SlashCommandOutcome, bcode_client::ClientError> {
    let parts = message.split_whitespace().collect::<Vec<_>>();
    let Some(command) = parts.first().map(|part| part.trim_start_matches('/')) else {
        return Ok(SlashCommandOutcome::Unknown(message.to_owned()));
    };
    match command {
        "sessions" => Ok(SlashCommandOutcome::PickSession),
        "new" => {
            let session = client.create_session(None).await?;
            Ok(SlashCommandOutcome::SwitchSession(session.id))
        }
        "plan" | "build" => {
            client
                .set_session_agent(session_id, command.to_owned())
                .await?;
            Ok(SlashCommandOutcome::Handled(format!(
                "agent set to {command}"
            )))
        }
        "agent" if parts.len() > 1 => {
            let agent_id = parts[1].to_owned();
            client
                .set_session_agent(session_id, agent_id.clone())
                .await?;
            Ok(SlashCommandOutcome::Handled(format!(
                "agent set to {agent_id}"
            )))
        }
        "compact" => {
            let message = client.compact_session(session_id).await?;
            Ok(SlashCommandOutcome::Handled(message))
        }
        "model" | "models" if parts.len() == 1 => Ok(SlashCommandOutcome::PickModel),
        "model" | "set-model" if parts.len() > 1 => {
            let model_id = parts[1].to_owned();
            client
                .set_session_model(session_id, None, model_id.clone())
                .await?;
            Ok(SlashCommandOutcome::Handled(format!(
                "model set to {model_id}"
            )))
        }
        "provider" | "set-provider" if parts.len() > 1 => {
            let provider = parts[1].to_owned();
            let model_id = client
                .session_model_status(session_id)
                .await?
                .model_id
                .unwrap_or_else(|| "default".to_owned());
            client
                .set_session_model(session_id, Some(provider.clone()), model_id.clone())
                .await?;
            Ok(SlashCommandOutcome::Handled(format!(
                "provider set to {provider}; model {model_id}"
            )))
        }
        "provider" => {
            let status = client.session_model_status(session_id).await?;
            Ok(SlashCommandOutcome::Handled(format!(
                "current provider: {}",
                status.provider_plugin_id.as_deref().unwrap_or("auto")
            )))
        }
        "skills" => Ok(SlashCommandOutcome::PickSkill),
        "skill" if parts.get(1) == Some(&"describe") && parts.len() > 2 => {
            let skill_id = bcode_skill_models::SkillId::new(parts[2]);
            let manifest = client.describe_skill(skill_id.clone()).await?;
            Ok(SlashCommandOutcome::SystemNote(format!(
                "Skill: {}\nName: {}\nDescription: {}\nSource: {}\nInstructions:\n{}",
                manifest.summary.id,
                manifest.summary.name,
                manifest
                    .summary
                    .description
                    .as_deref()
                    .unwrap_or("no description"),
                manifest.summary.source.label,
                manifest.instructions
            )))
        }
        "skill" if parts.len() > 1 => {
            let skill_id = bcode_skill_models::SkillId::new(parts[1]);
            let arguments = parts.iter().skip(2).copied().collect::<Vec<_>>().join(" ");
            let display_text = if arguments.is_empty() {
                format!("Invoke skill {skill_id}")
            } else {
                format!("Invoke skill {skill_id}: {arguments}")
            };
            let acceptance = client
                .invoke_skill(session_id, skill_id.clone(), arguments, display_text)
                .await?;
            Ok(SlashCommandOutcome::Handled(if acceptance.queued {
                format!("skill {skill_id} queued")
            } else {
                format!("skill {skill_id} invoked")
            }))
        }
        _ => Ok(SlashCommandOutcome::Unknown(message.to_owned())),
    }
}
