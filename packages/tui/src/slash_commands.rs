//! Backend-agnostic slash commands for the TUI.

use super::{daemon_issue, slash_registry};
use bcode_client::BcodeClient;
use bcode_session_models::SessionId;
use bcode_skill_models::SkillId;
use bcode_worktree_models::WorktreeListRequest;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommandOutcome {
    /// Command was handled in-place.
    Handled(String),
    /// Open timeline message browser.
    OpenTimeline,
    /// Switch to a new unpersisted draft session.
    NewDraftSession,
    /// Set the draft session agent locally.
    DraftAgentSelected {
        agent_id: String,
        agent_name: String,
        agent_accent: Option<String>,
    },
    /// Open the session picker.
    PickSession,
    /// Open model picker.
    PickModel,
    /// Open worktree create dialog.
    OpenWorktreeCreateDialog,
    /// Open fork session wizard.
    OpenForkSessionWizard,
    /// Clone a session.
    CloneSession {
        session_id: SessionId,
        name: Option<String>,
    },
    /// Set the active model for a session.
    SetSessionModel {
        session_id: SessionId,
        provider_plugin_id: Option<String>,
        model_id: String,
    },
    /// Set reasoning effort/summary for a session.
    SetSessionReasoning {
        session_id: SessionId,
        effort: Option<String>,
        summary: Option<String>,
        status: String,
    },
    /// Request active turn cancellation.
    CancelTurn { session_id: SessionId },
    /// Request runtime work cancellation.
    CancelRuntimeWork {
        session_id: SessionId,
        work_id: String,
    },
    /// Request context compaction.
    CompactContext { session_id: SessionId },
    /// Attach the active session to a path.
    AttachWorktree {
        session_id: SessionId,
        path: PathBuf,
    },
    /// Open the plugin-owned Ralph home UI.
    OpenRalphHome,
    /// Open Ralph loop start dialog.
    OpenRalphStartDialog,
    /// Show Ralph loop status.
    ShowRalphStatus,
    /// Start a Ralph autonomous run.
    RunRalphLoop,
    /// Approve a prepared Ralph autonomous run.
    ApproveRalphRun,
    /// Stop the active Ralph autonomous run.
    StopRalphLoop,
    /// List recent Ralph runs.
    ListRalphRuns,
    /// List iterations for the latest Ralph run.
    ListRalphIterations,
    /// Prepare an approval-gated resume run.
    ResumeRalphRun,
    /// Show the latest Ralph progress doc path.
    OpenRalphProgress,
    /// Build a Ralph work prompt.
    BuildRalphPrompt(bcode_ralph::RalphPromptKind),
    /// Open skill picker.
    PickSkill,
    /// Invoke a skill after creating an active session if needed.
    InvokeSkill {
        skill_id: bcode_skill_models::SkillId,
        arguments: String,
    },
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
    session_id: SessionId,
    parts: &[&str],
) -> Result<SlashCommandOutcome, bcode_client::ClientError> {
    if parts.get(1) == Some(&"history") {
        let spans = client.runtime_work_spans(session_id, 50).await?;
        if spans.is_empty() {
            return Ok(SlashCommandOutcome::Handled(
                "runtime history: empty".to_string(),
            ));
        }
        let lines = spans
            .into_iter()
            .map(|span| {
                format!(
                    "{} {:?} duration_ms={:?} parent={} {}{}",
                    span.work_id,
                    span.status,
                    span.duration_ms(),
                    span.parent_work_id
                        .as_ref()
                        .map_or_else(|| "-".to_string(), ToString::to_string),
                    span.label,
                    span.message
                        .as_ref()
                        .map_or_else(String::new, |message| format!(" — {message}"))
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(SlashCommandOutcome::Handled(format!(
            "runtime history:\n{lines}"
        )));
    }
    let work = client.list_runtime_work(session_id).await?;
    if work.is_empty() {
        return Ok(SlashCommandOutcome::Handled("runtime: idle".to_string()));
    }
    let lines = work
        .into_iter()
        .map(|item| {
            format!(
                "{} {:?} {:?} {}",
                item.work_id, item.kind, item.status, item.label
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(SlashCommandOutcome::Handled(format!(
        "runtime work:\n{lines}"
    )))
}

fn draft_thinking_command(parts: &[&str]) -> SlashCommandOutcome {
    match parts.get(1).copied() {
        None => SlashCommandOutcome::OpenThinkingSettings(
            super::thinking_dialog::ThinkingDialogFocus::Display,
        ),
        Some("effort") if parts.len() == 2 => SlashCommandOutcome::OpenThinkingSettings(
            super::thinking_dialog::ThinkingDialogFocus::Effort,
        ),
        Some("summary") if parts.len() == 2 => SlashCommandOutcome::OpenThinkingSettings(
            super::thinking_dialog::ThinkingDialogFocus::Summary,
        ),
        Some("show") => SlashCommandOutcome::SetThinkingDisplay(true),
        Some("hide") => SlashCommandOutcome::SetThinkingDisplay(false),
        Some("toggle") => SlashCommandOutcome::ToggleThinkingDisplay,
        Some("status" | "capabilities") => {
            SlashCommandOutcome::Handled("thinking status requires an active session".to_owned())
        }
        Some(_) => SlashCommandOutcome::Handled(
            "setting thinking effort requires an active session".to_owned(),
        ),
    }
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
            Ok(SlashCommandOutcome::SetSessionReasoning {
                session_id,
                effort: Some(effort.clone()),
                summary: None,
                status: format!("thinking effort set to {effort}"),
            })
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
            Ok(SlashCommandOutcome::SetSessionReasoning {
                session_id,
                effort: None,
                summary: Some(summary.clone()),
                status: format!("thinking summary set to {summary}"),
            })
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
            Ok(SlashCommandOutcome::SetSessionReasoning {
                session_id,
                effort: Some(value.to_owned()),
                summary: None,
                status: format!("thinking effort set to {value}"),
            })
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

fn goal_command(parts: &[&str]) -> SlashCommandOutcome {
    let mut ralph_parts = Vec::with_capacity(parts.len().max(2));
    ralph_parts.push("/ralph");
    if parts.len() == 1 {
        ralph_parts.push("start");
    } else {
        ralph_parts.extend(parts.iter().skip(1).copied());
    }
    ralph_command(&ralph_parts)
}

fn cwd_command(session_id: SessionId, parts: &[&str]) -> SlashCommandOutcome {
    if parts.len() <= 1 {
        return SlashCommandOutcome::Handled("usage: /cwd <path>".to_owned());
    }
    let working_directory = parts.iter().skip(1).copied().collect::<Vec<_>>().join(" ");
    SlashCommandOutcome::AttachWorktree {
        session_id,
        path: PathBuf::from(working_directory),
    }
}

fn ralph_command(parts: &[&str]) -> SlashCommandOutcome {
    match parts.get(1).copied() {
        Some("ui" | "home") => SlashCommandOutcome::OpenRalphHome,
        Some("start") | None => SlashCommandOutcome::OpenRalphStartDialog,
        Some("status") => SlashCommandOutcome::ShowRalphStatus,
        Some("open") => SlashCommandOutcome::OpenRalphProgress,
        Some("run") => SlashCommandOutcome::RunRalphLoop,
        Some("approve") => SlashCommandOutcome::ApproveRalphRun,
        Some("audit") => SlashCommandOutcome::BuildRalphPrompt(bcode_ralph::RalphPromptKind::Audit),
        Some("replan") => {
            SlashCommandOutcome::BuildRalphPrompt(bcode_ralph::RalphPromptKind::Replan)
        }
        Some("stop") => SlashCommandOutcome::StopRalphLoop,
        Some("runs") => SlashCommandOutcome::ListRalphRuns,
        Some("iterations") => SlashCommandOutcome::ListRalphIterations,
        Some("resume") => SlashCommandOutcome::ResumeRalphRun,
        Some(_) => SlashCommandOutcome::Handled(
            "usage: /ralph [ui|start|run|approve|stop|status|runs|iterations|resume|audit|replan|open]"
                .to_owned(),
        ),
    }
}

async fn worktree_command(
    client: &BcodeClient,
    session_id: Option<SessionId>,
    parts: &[&str],
) -> Result<SlashCommandOutcome, bcode_client::ClientError> {
    match parts.get(1).copied() {
        Some("list") => {
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
        Some("create") | None => Ok(SlashCommandOutcome::OpenWorktreeCreateDialog),
        Some("attach") if parts.len() > 2 => {
            let Some(session_id) = session_id else {
                return Ok(SlashCommandOutcome::Handled(
                    "worktree attach requires an active session".to_owned(),
                ));
            };
            let path = parts.iter().skip(2).copied().collect::<Vec<_>>().join(" ");
            Ok(SlashCommandOutcome::AttachWorktree {
                session_id,
                path: PathBuf::from(path),
            })
        }
        Some(_) => Ok(SlashCommandOutcome::Handled(
            "usage: /worktree [list|create|attach <path>]".to_string(),
        )),
    }
}

async fn resync_command(
    client: &BcodeClient,
    parts: &[&str],
) -> Result<SlashCommandOutcome, bcode_client::ClientError> {
    match parts.get(1).copied() {
        Some("sessions") | None => {
            let sources = if parts.len() > 2 {
                Some(
                    parts
                        .iter()
                        .skip(2)
                        .map(|part| (*part).to_owned())
                        .collect(),
                )
            } else {
                None
            };
            let list = client.refresh_session_catalog(sources).await?;
            Ok(SlashCommandOutcome::Handled(format!(
                "session catalog refresh requested (revision {})",
                list.catalog_revision
            )))
        }
        Some(other) => Ok(SlashCommandOutcome::Handled(format!(
            "unknown resync target: {other}; usage: /resync sessions [source]"
        ))),
    }
}

async fn skill_command(
    client: &BcodeClient,
    parts: &[&str],
) -> Result<SlashCommandOutcome, bcode_client::ClientError> {
    if parts.get(1) == Some(&"describe") && parts.len() > 2 {
        return describe_skill(client, parts[2]).await;
    }
    let Some(skill) = parts.get(1) else {
        return Ok(SlashCommandOutcome::PickSkill);
    };
    let skill_id = SkillId::new(*skill);
    let arguments = parts.iter().skip(2).copied().collect::<Vec<_>>().join(" ");
    Ok(SlashCommandOutcome::InvokeSkill {
        skill_id,
        arguments,
    })
}

const fn stop_command(session_id: SessionId) -> SlashCommandOutcome {
    SlashCommandOutcome::CancelTurn { session_id }
}

fn cancel_runtime_command(session_id: SessionId, parts: &[&str]) -> SlashCommandOutcome {
    let Some(work_id) = parts.get(1) else {
        return SlashCommandOutcome::Handled("usage: /cancel-runtime <work-id>".to_string());
    };
    SlashCommandOutcome::CancelRuntimeWork {
        session_id,
        work_id: (*work_id).to_owned(),
    }
}

async fn handle_agent_command(
    client: &BcodeClient,
    _session_id: Option<SessionId>,
    current_agent_id: &str,
    parts: &[&str],
) -> Result<SlashCommandOutcome, bcode_client::ClientError> {
    let command = parts[0].trim_start_matches('/');
    let agent_id = if command == "agent" {
        let Some(agent_id) = parts.get(1) else {
            return Ok(SlashCommandOutcome::Handled(format!(
                "agent: {current_agent_id}"
            )));
        };
        (*agent_id).to_owned()
    } else {
        command.to_owned()
    };

    let agents = client.list_agents().await?;
    let Some(agent) = agents
        .iter()
        .find(|agent| agent.id == agent_id || agent.aliases.iter().any(|alias| alias == &agent_id))
    else {
        return Ok(SlashCommandOutcome::Handled(format!(
            "unknown agent profile: {agent_id}"
        )));
    };

    Ok(SlashCommandOutcome::DraftAgentSelected {
        agent_id: agent.id.clone(),
        agent_name: agent.name.clone(),
        agent_accent: agent.accent.clone(),
    })
}

fn slash_client_issue(label: &str, error: &bcode_client::ClientError) -> SlashCommandOutcome {
    let issue = daemon_issue::classify_client_error(error);
    SlashCommandOutcome::Handled(issue.message(label).status)
}

/// Execute a slash command.
///
/// # Errors
///
/// Returns an error when the daemon rejects a requested operation.
#[allow(clippy::too_many_lines)]
pub async fn execute_resolved(
    client: &BcodeClient,
    session_id: Option<SessionId>,
    current_agent_id: &str,
    message: &str,
    resolution: slash_registry::SlashResolution,
) -> Result<SlashCommandOutcome, bcode_client::ClientError> {
    let parts = message.split_whitespace().collect::<Vec<_>>();
    let outcome = match resolution {
        slash_registry::SlashResolution::Builtin(command) => {
            execute_builtin(
                client,
                session_id,
                current_agent_id,
                message,
                &parts,
                command.name(),
            )
            .await
        }
        slash_registry::SlashResolution::SkillAlias {
            skill_id,
            arguments,
        } => Ok(SlashCommandOutcome::InvokeSkill {
            skill_id,
            arguments,
        }),
        slash_registry::SlashResolution::Unknown => {
            Ok(SlashCommandOutcome::Unknown(message.to_owned()))
        }
    };
    match outcome {
        Ok(outcome) => Ok(outcome),
        Err(error) => Ok(slash_client_issue("slash command unavailable", &error)),
    }
}

#[allow(clippy::too_many_lines)]
async fn execute_builtin(
    client: &BcodeClient,
    session_id: Option<SessionId>,
    current_agent_id: &str,
    message: &str,
    parts: &[&str],
    command: &str,
) -> Result<SlashCommandOutcome, bcode_client::ClientError> {
    match command {
        "sessions" => Ok(SlashCommandOutcome::PickSession),
        "resync" => resync_command(client, parts).await,
        "rescan-imports" => client.refresh_session_catalog(None).await.map(|list| {
            SlashCommandOutcome::Handled(format!(
                "session catalog refresh requested (revision {})",
                list.catalog_revision
            ))
        }),
        "new" => Ok(SlashCommandOutcome::NewDraftSession),
        "plan" | "build" | "agent" => {
            handle_agent_command(client, session_id, current_agent_id, parts).await
        }
        "compact" => {
            let Some(session_id) = session_id else {
                return Ok(SlashCommandOutcome::Handled(
                    "compact requires an active session".to_owned(),
                ));
            };
            Ok(SlashCommandOutcome::CompactContext { session_id })
        }
        "model" | "models" if parts.len() == 1 => Ok(SlashCommandOutcome::PickModel),
        "model" | "set-model" if parts.len() > 1 => {
            let Some(session_id) = session_id else {
                return Ok(SlashCommandOutcome::Handled(
                    "model selection requires an active session".to_owned(),
                ));
            };
            let model_id = parts[1].to_owned();
            Ok(SlashCommandOutcome::SetSessionModel {
                session_id,
                provider_plugin_id: None,
                model_id,
            })
        }
        "provider" | "set-provider" if parts.len() > 1 => {
            let Some(session_id) = session_id else {
                return Ok(SlashCommandOutcome::Handled(
                    "provider selection requires an active session".to_owned(),
                ));
            };
            let provider = parts[1].to_owned();
            let model_id = client
                .session_model_status(session_id)
                .await?
                .model_id
                .unwrap_or_else(|| "default".to_owned());
            Ok(SlashCommandOutcome::SetSessionModel {
                session_id,
                provider_plugin_id: Some(provider),
                model_id,
            })
        }
        "provider" => {
            let Some(session_id) = session_id else {
                return Ok(SlashCommandOutcome::Handled(
                    "provider status requires an active session".to_owned(),
                ));
            };
            let status = client.session_model_status(session_id).await?;
            Ok(SlashCommandOutcome::Handled(format!(
                "current provider: {}",
                status.provider_plugin_id.as_deref().unwrap_or("auto")
            )))
        }
        "context-strategy" | "context" => {
            let Some(session_id) = session_id else {
                return Ok(SlashCommandOutcome::Handled(
                    "context-strategy requires an active session".to_owned(),
                ));
            };
            let status = client.session_model_status(session_id).await?;
            Ok(SlashCommandOutcome::Handled(format!(
                "context strategy: prompt_cache={}, conversation_reuse={}, compaction={}",
                status.prompt_cache_mode.as_deref().unwrap_or("unknown"),
                status
                    .conversation_reuse_mode
                    .as_deref()
                    .unwrap_or("unknown"),
                status.compaction_mode.as_deref().unwrap_or("unknown")
            )))
        }
        "diff" => Ok(SlashCommandOutcome::ToggleDiff),
        "cwd" => {
            let Some(session_id) = session_id else {
                return Ok(SlashCommandOutcome::Handled(
                    "cwd requires an active session".to_owned(),
                ));
            };
            Ok(cwd_command(session_id, parts))
        }
        "worktree" | "worktrees" => worktree_command(client, session_id, parts).await,
        "fork" => {
            if session_id.is_none() {
                return Ok(SlashCommandOutcome::Handled(
                    "fork requires an active session".to_owned(),
                ));
            }
            Ok(SlashCommandOutcome::OpenForkSessionWizard)
        }
        "clone" => {
            let Some(session_id) = session_id else {
                return Ok(SlashCommandOutcome::Handled(
                    "clone requires an active session".to_owned(),
                ));
            };
            let name = parts.get(1).map(|value| (*value).to_owned());
            Ok(SlashCommandOutcome::CloneSession { session_id, name })
        }
        "ralph" => Ok(ralph_command(parts)),
        "goal" => Ok(goal_command(parts)),
        "skills" => Ok(SlashCommandOutcome::PickSkill),
        "skill" => {
            if parts.get(1) == Some(&"describe") {
                if let Some(skill_id) = parts.get(2) {
                    return describe_skill(client, skill_id).await;
                }
                return Ok(SlashCommandOutcome::Handled(
                    "usage: /skill describe <skill-id>".to_owned(),
                ));
            }
            if session_id.is_none() {
                let Some(skill) = parts.get(1) else {
                    return Ok(SlashCommandOutcome::PickSkill);
                };
                return Ok(SlashCommandOutcome::InvokeSkill {
                    skill_id: bcode_skill_models::SkillId::new(*skill),
                    arguments: parts.iter().skip(2).copied().collect::<Vec<_>>().join(" "),
                });
            }
            skill_command(client, parts).await
        }
        "thinking" => {
            let Some(session_id) = session_id else {
                return Ok(draft_thinking_command(parts));
            };
            thinking_command(client, session_id, parts).await
        }
        "timeline" => Ok(SlashCommandOutcome::OpenTimeline),
        "stop" => {
            let Some(session_id) = session_id else {
                return Ok(SlashCommandOutcome::Handled(
                    "stop requires an active session".to_owned(),
                ));
            };
            Ok(stop_command(session_id))
        }
        "cancel-runtime" => {
            let Some(session_id) = session_id else {
                return Ok(SlashCommandOutcome::Handled(
                    "runtime cancellation requires an active session".to_owned(),
                ));
            };
            Ok(cancel_runtime_command(session_id, parts))
        }
        "runtime" | "status" => {
            let Some(session_id) = session_id else {
                return Ok(SlashCommandOutcome::Handled(
                    "runtime: no active session".to_owned(),
                ));
            };
            runtime_status(client, session_id, parts).await
        }
        _ => Ok(SlashCommandOutcome::Unknown(message.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_ralph::RalphPromptKind;

    #[test]
    fn ralph_start_routes_to_start_dialog() {
        assert_eq!(
            ralph_command(&["/ralph"]),
            SlashCommandOutcome::OpenRalphStartDialog
        );
        assert_eq!(
            ralph_command(&["/ralph", "start"]),
            SlashCommandOutcome::OpenRalphStartDialog
        );
    }

    #[test]
    fn ralph_status_and_open_route_to_state_views() {
        assert_eq!(
            ralph_command(&["/ralph", "status"]),
            SlashCommandOutcome::ShowRalphStatus
        );
        assert_eq!(
            ralph_command(&["/ralph", "open"]),
            SlashCommandOutcome::OpenRalphProgress
        );
    }

    #[test]
    fn ralph_run_and_stop_route_to_runner_actions() {
        assert_eq!(
            ralph_command(&["/ralph", "run"]),
            SlashCommandOutcome::RunRalphLoop
        );
        assert_eq!(
            ralph_command(&["/ralph", "approve"]),
            SlashCommandOutcome::ApproveRalphRun
        );
        assert_eq!(
            ralph_command(&["/ralph", "stop"]),
            SlashCommandOutcome::StopRalphLoop
        );
    }

    #[test]
    fn ralph_audit_and_replan_route_to_prompt_builders() {
        assert_eq!(
            ralph_command(&["/ralph", "audit"]),
            SlashCommandOutcome::BuildRalphPrompt(RalphPromptKind::Audit)
        );
        assert_eq!(
            ralph_command(&["/ralph", "replan"]),
            SlashCommandOutcome::BuildRalphPrompt(RalphPromptKind::Replan)
        );
    }

    #[test]
    fn ralph_runs_and_iterations_route_to_list_views() {
        assert_eq!(
            ralph_command(&["/ralph", "runs"]),
            SlashCommandOutcome::ListRalphRuns
        );
        assert_eq!(
            ralph_command(&["/ralph", "iterations"]),
            SlashCommandOutcome::ListRalphIterations
        );
        assert_eq!(
            ralph_command(&["/ralph", "resume"]),
            SlashCommandOutcome::ResumeRalphRun
        );
    }

    #[test]
    fn goal_alias_routes_to_ralph_workflow() {
        assert_eq!(
            goal_command(&["/goal"]),
            SlashCommandOutcome::OpenRalphStartDialog
        );
        assert_eq!(
            goal_command(&["/goal", "run"]),
            SlashCommandOutcome::RunRalphLoop
        );
        assert_eq!(
            goal_command(&["/goal", "approve"]),
            SlashCommandOutcome::ApproveRalphRun
        );
        assert_eq!(
            goal_command(&["/goal", "status"]),
            SlashCommandOutcome::ShowRalphStatus
        );
    }

    #[test]
    fn ralph_unknown_subcommand_reports_usage() {
        assert_eq!(
            ralph_command(&["/ralph", "wat"]),
            SlashCommandOutcome::Handled(
                "usage: /ralph [ui|start|run|approve|stop|status|runs|iterations|resume|audit|replan|open]"
                    .to_owned()
            )
        );
    }
}
