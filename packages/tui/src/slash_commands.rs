//! Backend-agnostic slash commands for the TUI.

use bcode_client::BcodeClient;
use bcode_session_models::SessionId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommandOutcome {
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
    /// Toggle diff panel.
    ToggleDiff,
    /// Toggle local thinking display.
    SetThinkingDisplay(bool),
    /// Toggle local thinking display.
    ToggleThinkingDisplay,
    /// Show a system note.
    SystemNote(String),
    /// Unknown slash command.
    Unknown(String),
}

async fn describe_skill(
    client: &BcodeClient,
    skill_id: &str,
) -> Result<SlashCommandOutcome, bcode_client::ClientError> {
    let skill_id = bcode_skill_models::SkillId::new(skill_id);
    let manifest = client.describe_skill(skill_id).await?;
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

async fn runtime_status(
    client: &BcodeClient,
) -> Result<SlashCommandOutcome, bcode_client::ClientError> {
    let status = client.server_status().await?;
    let running = status
        .plugin_runtime
        .iter()
        .map(|plugin| plugin.running)
        .sum::<usize>();
    let queued = status
        .plugin_runtime
        .iter()
        .map(|plugin| plugin.queued)
        .sum::<usize>();
    Ok(SlashCommandOutcome::Handled(format!(
        "runtime: {running} running, {queued} queued"
    )))
}

async fn thinking_command(
    client: &BcodeClient,
    session_id: SessionId,
    parts: &[&str],
) -> Result<SlashCommandOutcome, bcode_client::ClientError> {
    let status = client.session_model_status(session_id).await?;
    match parts.get(1).copied() {
        Some("capabilities") => Ok(SlashCommandOutcome::Handled(thinking_capabilities(&status))),
        Some("status") | None => Ok(SlashCommandOutcome::Handled(thinking_status(&status))),
        Some("effort") if parts.len() > 2 => {
            let effort = parts[2].to_owned();
            client
                .set_session_reasoning(session_id, Some(effort.clone()), None)
                .await?;
            Ok(SlashCommandOutcome::Handled(format!(
                "thinking effort set to {effort}"
            )))
        }
        Some("summary") if parts.len() > 2 => {
            let summary = parts[2].to_owned();
            client
                .set_session_reasoning(session_id, None, Some(summary.clone()))
                .await?;
            Ok(SlashCommandOutcome::Handled(format!(
                "thinking summary set to {summary}"
            )))
        }
        Some("show") => Ok(SlashCommandOutcome::SetThinkingDisplay(true)),
        Some("hide") => Ok(SlashCommandOutcome::SetThinkingDisplay(false)),
        Some("toggle") => Ok(SlashCommandOutcome::ToggleThinkingDisplay),
        Some(value) => {
            client
                .set_session_reasoning(session_id, Some(value.to_owned()), None)
                .await?;
            Ok(SlashCommandOutcome::Handled(format!(
                "thinking effort set to {value}"
            )))
        }
    }
}

fn thinking_status(status: &bcode_ipc::SessionModelStatus) -> String {
    format!(
        "thinking: effort={}, summary={}{}",
        status.reasoning_effort.as_deref().unwrap_or("default"),
        status.reasoning_summary.as_deref().unwrap_or("default"),
        status
            .reasoning
            .as_ref()
            .map_or_else(String::new, |reasoning| format!(
                "\navailable effort: {}\navailable summary: {}",
                list_or_default(&reasoning.effort_values),
                list_or_default(&reasoning.summary_values)
            ))
    )
}

fn thinking_capabilities(status: &bcode_ipc::SessionModelStatus) -> String {
    let Some(reasoning) = &status.reasoning else {
        return "thinking: no provider-declared reasoning capabilities for this model".to_owned();
    };
    format!(
        "thinking capabilities\neffort: {}\ndefault effort: {}\nvisible summary: {}\nsummary values: {}\ndefault summary: {}\nraw reasoning: {}",
        list_or_default(&reasoning.effort_values),
        reasoning.default_effort.as_deref().unwrap_or("unknown"),
        reasoning.visible_summary_supported,
        list_or_default(&reasoning.summary_values),
        reasoning.default_summary.as_deref().unwrap_or("unknown"),
        reasoning.raw_reasoning_supported,
    )
}

fn list_or_default(values: &[String]) -> String {
    if values.is_empty() {
        "unknown".to_owned()
    } else {
        values.join(", ")
    }
}

/// Execute a slash command.
///
/// # Errors
///
/// Returns an error when the daemon rejects a requested operation.
pub async fn execute(
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
        "diff" => Ok(SlashCommandOutcome::ToggleDiff),
        "skills" => Ok(SlashCommandOutcome::PickSkill),
        "skill" if parts.get(1) == Some(&"describe") && parts.len() > 2 => {
            describe_skill(client, parts[2]).await
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
        "thinking" => thinking_command(client, session_id, &parts).await,
        "runtime" | "status" => runtime_status(client).await,
        _ => Ok(SlashCommandOutcome::Unknown(message.to_owned())),
    }
}
