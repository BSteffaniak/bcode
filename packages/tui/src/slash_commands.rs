//! Backend-agnostic slash commands for the TUI.

use bcode_client::BcodeClient;
use bcode_session_models::SessionId;
use bcode_worktree_models::{WorktreeCreateRequest, WorktreeListRequest};

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
    /// Open thinking settings dialog.
    OpenThinkingSettings(super::thinking_dialog::ThinkingDialogFocus),
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
        Some("status") => Ok(SlashCommandOutcome::Handled(thinking_status(&status))),
        None => Ok(SlashCommandOutcome::OpenThinkingSettings(
            super::thinking_dialog::ThinkingDialogFocus::Display,
        )),
        Some("effort") if parts.len() == 2 => Ok(SlashCommandOutcome::OpenThinkingSettings(
            super::thinking_dialog::ThinkingDialogFocus::Effort,
        )),
        Some("summary") if parts.len() == 2 => Ok(SlashCommandOutcome::OpenThinkingSettings(
            super::thinking_dialog::ThinkingDialogFocus::Summary,
        )),
        Some("effort") if parts.len() > 2 => {
            let effort = parts[2].to_owned();
            if let Some(message) = unsupported_reasoning_value(
                "effort",
                &effort,
                status
                    .reasoning
                    .as_ref()
                    .map(|reasoning| reasoning.effort_values.as_slice()),
            ) {
                return Ok(SlashCommandOutcome::Handled(message));
            }
            client
                .set_session_reasoning(session_id, Some(effort.clone()), None)
                .await?;
            Ok(SlashCommandOutcome::Handled(format!(
                "thinking effort set to {effort}"
            )))
        }
        Some("summary") if parts.len() > 2 => {
            let summary = parts[2].to_owned();
            if let Some(message) = unsupported_reasoning_value(
                "summary",
                &summary,
                status
                    .reasoning
                    .as_ref()
                    .map(|reasoning| reasoning.summary_values.as_slice()),
            ) {
                return Ok(SlashCommandOutcome::Handled(message));
            }
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
            if let Some(message) = unsupported_reasoning_value(
                "effort",
                value,
                status
                    .reasoning
                    .as_ref()
                    .map(|reasoning| reasoning.effort_values.as_slice()),
            ) {
                return Ok(SlashCommandOutcome::Handled(message));
            }
            client
                .set_session_reasoning(session_id, Some(value.to_owned()), None)
                .await?;
            Ok(SlashCommandOutcome::Handled(format!(
                "thinking effort set to {value}"
            )))
        }
    }
}

fn unsupported_reasoning_value(
    kind: &str,
    value: &str,
    supported: Option<&[String]>,
) -> Option<String> {
    let supported = supported?;
    if supported.is_empty() || supported.iter().any(|candidate| candidate == value) {
        return None;
    }
    Some(format!(
        "unsupported thinking {kind} '{value}' (supported: {})",
        list_or_default(supported)
    ))
}

fn thinking_status(status: &bcode_ipc::SessionModelStatus) -> String {
    let effort = status
        .reasoning_effort
        .as_deref()
        .or_else(|| {
            status
                .reasoning
                .as_ref()
                .and_then(|reasoning| reasoning.default_effort.as_deref())
        })
        .unwrap_or("provider default");
    let summary = status
        .reasoning_summary
        .as_deref()
        .or_else(|| {
            status
                .reasoning
                .as_ref()
                .and_then(|reasoning| reasoning.default_summary.as_deref())
        })
        .unwrap_or("provider default");
    format!(
        "thinking: effort={effort}, summary={summary}{}",
        status
            .reasoning
            .as_ref()
            .map_or_else(String::new, |reasoning| format!(
                "\nsource: {}\navailable effort: {}\navailable summary: {}",
                reasoning_source_label(reasoning.source),
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
        "thinking capabilities\nsource: {}\neffort: {}\ndefault effort: {}\nvisible summary: {}\nsummary values: {}\ndefault summary: {}\nraw reasoning: {}",
        reasoning_source_label(reasoning.source),
        list_or_default(&reasoning.effort_values),
        reasoning.default_effort.as_deref().unwrap_or("unknown"),
        reasoning.visible_summary_supported,
        list_or_default(&reasoning.summary_values),
        reasoning.default_summary.as_deref().unwrap_or("unknown"),
        reasoning.raw_reasoning_supported,
    )
}

const fn reasoning_source_label(
    source: bcode_model::ModelReasoningCapabilitySource,
) -> &'static str {
    match source {
        bcode_model::ModelReasoningCapabilitySource::ConfigOverride => "config override",
        bcode_model::ModelReasoningCapabilitySource::ProviderMetadata => "provider metadata",
        bcode_model::ModelReasoningCapabilitySource::KnownModelTable => "known model table",
        bcode_model::ModelReasoningCapabilitySource::GenericFallback => {
            "common fallback; provider may reject"
        }
        bcode_model::ModelReasoningCapabilitySource::Unknown => "unknown",
    }
}

fn list_or_default(values: &[String]) -> String {
    if values.is_empty() {
        "unknown".to_owned()
    } else {
        values.join(", ")
    }
}

async fn cwd_command(
    client: &BcodeClient,
    session_id: SessionId,
    parts: &[&str],
) -> Result<SlashCommandOutcome, bcode_client::ClientError> {
    if parts.len() <= 1 {
        return Ok(SlashCommandOutcome::Handled(
            "usage: /cwd <path>".to_owned(),
        ));
    }
    let working_directory = parts.iter().skip(1).copied().collect::<Vec<_>>().join(" ");
    let session = client
        .change_session_working_directory(session_id, working_directory)
        .await?;
    Ok(SlashCommandOutcome::Handled(format!(
        "working directory set to {}",
        session.working_directory.display()
    )))
}

async fn worktree_command(
    client: &BcodeClient,
    session_id: SessionId,
    parts: &[&str],
) -> Result<SlashCommandOutcome, bcode_client::ClientError> {
    match parts.get(1).copied() {
        Some("list") | None => {
            let response = client
                .list_worktrees(WorktreeListRequest { cwd: None })
                .await?;
            let mut lines = vec![format!("worktrees for {}", response.repo_root.display())];
            lines.extend(response.worktrees.into_iter().map(|worktree| {
                let marker = if worktree.is_main { "main" } else { "linked" };
                let branch = worktree.branch.unwrap_or_else(|| "<detached>".to_string());
                format!("* {marker} {branch} — {}", worktree.path.display())
            }));
            Ok(SlashCommandOutcome::SystemNote(lines.join("\n")))
        }
        Some("create") => {
            let name = parts.iter().skip(2).copied().collect::<Vec<_>>().join(" ");
            if name.trim().is_empty() {
                return Ok(SlashCommandOutcome::Handled(
                    "usage: /worktree create <name>".to_string(),
                ));
            }
            let response = client
                .create_worktree(WorktreeCreateRequest {
                    name,
                    cwd: None,
                    path: None,
                    branch: None,
                    new_branch: None,
                    base_ref: Some(bcode_worktree_models::WorktreeBaseRef::Head),
                    detach: false,
                    force: false,
                    attach_session_id: Some(session_id),
                    new_session: false,
                    no_setup: false,
                })
                .await?;
            Ok(SlashCommandOutcome::Handled(format!(
                "created worktree {}",
                response.path.display()
            )))
        }
        Some("attach") if parts.len() > 2 => {
            let path = parts.iter().skip(2).copied().collect::<Vec<_>>().join(" ");
            let session = client
                .change_session_working_directory(session_id, path)
                .await?;
            Ok(SlashCommandOutcome::Handled(format!(
                "working directory set to {}",
                session.working_directory.display()
            )))
        }
        Some(_) => Ok(SlashCommandOutcome::Handled(
            "usage: /worktree [list|create <name>|attach <path>]".to_string(),
        )),
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
        "sessions" | "rescan-imports" => Ok(SlashCommandOutcome::PickSession),
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
        "cwd" => cwd_command(client, session_id, &parts).await,
        "worktree" | "worktrees" => worktree_command(client, session_id, &parts).await,
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
