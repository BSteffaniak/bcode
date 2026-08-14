//! Backend-agnostic slash commands for the TUI.

use super::{daemon_issue, slash_registry};
use bcode_client::BcodeClient;
use bcode_plugin_sdk::path::display_from_current_dir;
use bcode_session_models::SessionId;
use bcode_skill_models::SkillId;
use bcode_worktree_models::WorktreeListRequest;
use std::fmt::Write as _;
use std::path::PathBuf;

/// Local execution context for backend-agnostic slash commands.
#[derive(Debug, Clone, Copy)]
pub struct SlashExecutionContext<'a> {
    pub working_directory: &'a std::path::Path,
    pub current_agent_id: &'a str,
    pub reasoning_display_mode: bcode_config::TuiThinkingMode,
    pub reasoning_visible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommandOutcome {
    /// Command was handled in-place.
    Handled(String),
    /// Execute a plugin-owned slash command.
    PluginCommand {
        action: bcode_command::CommandAction,
        execution: bcode_command::CommandExecution,
        arguments: String,
    },
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
    /// Open the session picker focused on transcript search.
    SearchSessions,
    /// Open model picker.
    PickModel,
    /// Open auth-pool subscription picker.
    PickAuthPool,
    /// Preview a bundled theme without persistence.
    PreviewTheme { theme_id: String },
    /// Apply a bundled theme as the global interactive selection.
    ApplyTheme { theme_id: String },
    /// Clear the global interactive selection and restore the configured default.
    ResetTheme,
    /// Restore the configured theme after preview.
    CancelThemePreview,
    /// Open the interactive theme picker.
    OpenThemePicker,
    /// Open the interactive streaming presentation configurator.
    OpenStreamingConfigurator,
    /// Show the theme catalog as durable transcript content.
    ShowThemeCatalog,
    /// Open worktree create dialog.
    OpenWorktreeCreateDialog,
    /// Open fork session wizard.
    OpenForkSessionWizard,
    /// Clone a session.
    CloneSession {
        session_id: SessionId,
        name: Option<String>,
    },
    /// Set the model for the next draft session.
    SetLocalModel {
        provider_plugin_id: Option<String>,
        model_id: String,
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
    /// Open reasoning output settings dialog.
    OpenThinkingSettings(super::thinking_dialog::ThinkingDialogFocus),
    /// Set local reasoning output display.
    SetThinkingDisplay(bool),
    /// Toggle local reasoning output display.
    ToggleThinkingDisplay,
    /// Set the local reasoning content display mode.
    SetThinkingMode(bcode_config::TuiThinkingMode),
    /// Show a Markdown system note.
    SystemMarkdown(String),
    /// Show a plain-text system note.
    SystemPlain(String),
    /// Unknown slash command.
    Unknown(String),
}

async fn describe_skill(
    client: &BcodeClient,
    skill_id: &str,
) -> Result<SlashCommandOutcome, bcode_client::ClientError> {
    let skill_id = bcode_skill_models::SkillId::new(skill_id);
    let manifest = client.describe_skill(skill_id).await?;
    Ok(SlashCommandOutcome::SystemMarkdown(
        format_skill_details_markdown(
            &manifest.summary.name,
            manifest.summary.id.as_str(),
            &manifest.summary.source.label,
            manifest.summary.description.as_deref(),
            &manifest.instructions,
        ),
    ))
}

/// Format a skill description as a Markdown transcript document.
#[must_use]
pub fn format_skill_details_markdown(
    name: &str,
    id: &str,
    source: &str,
    description: Option<&str>,
    instructions: &str,
) -> String {
    format!(
        "# {name}\n\n* **ID:** `{id}`\n* **Source:** {source}\n\n{}\n\n## Instructions\n\n{instructions}",
        description.unwrap_or("No description.")
    )
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
        Some("mode") if parts.len() == 2 => SlashCommandOutcome::OpenThinkingSettings(
            super::thinking_dialog::ThinkingDialogFocus::Mode,
        ),
        Some("mode") if parts.len() > 2 => match parts[2] {
            "all" => SlashCommandOutcome::SetThinkingMode(bcode_config::TuiThinkingMode::All),
            "summary" => {
                SlashCommandOutcome::SetThinkingMode(bcode_config::TuiThinkingMode::Summary)
            }
            "raw" => SlashCommandOutcome::SetThinkingMode(bcode_config::TuiThinkingMode::Raw),
            value => SlashCommandOutcome::Handled(format!(
                "unsupported reasoning display mode '{value}' (supported: all, summary, raw)"
            )),
        },
        Some("show") => SlashCommandOutcome::SetThinkingDisplay(true),
        Some("hide") => SlashCommandOutcome::SetThinkingDisplay(false),
        Some("toggle") => SlashCommandOutcome::ToggleThinkingDisplay,
        Some("status" | "capabilities") => SlashCommandOutcome::Handled(
            "reasoning output status requires an active session".to_owned(),
        ),
        Some(_) => SlashCommandOutcome::Handled(
            "setting reasoning effort requires an active session".to_owned(),
        ),
    }
}

async fn thinking_command(
    client: &BcodeClient,
    session_id: SessionId,
    parts: &[&str],
    display_mode: bcode_config::TuiThinkingMode,
    display_visible: bool,
) -> Result<SlashCommandOutcome, bcode_client::ClientError> {
    let status = client.session_model_status(session_id).await?;
    match parts.get(1).copied() {
        Some("capabilities") => Ok(SlashCommandOutcome::Handled(thinking_capabilities(&status))),
        Some("status") => Ok(SlashCommandOutcome::Handled(thinking_status(
            &status,
            display_mode,
            display_visible,
        ))),
        None => Ok(SlashCommandOutcome::OpenThinkingSettings(
            super::thinking_dialog::ThinkingDialogFocus::Display,
        )),
        Some("effort") if parts.len() == 2 => Ok(SlashCommandOutcome::OpenThinkingSettings(
            super::thinking_dialog::ThinkingDialogFocus::Effort,
        )),
        Some("summary") if parts.len() == 2 => Ok(SlashCommandOutcome::OpenThinkingSettings(
            super::thinking_dialog::ThinkingDialogFocus::Summary,
        )),
        Some("mode") if parts.len() == 2 => Ok(SlashCommandOutcome::OpenThinkingSettings(
            super::thinking_dialog::ThinkingDialogFocus::Mode,
        )),
        Some("mode") if parts.len() > 2 => match parts[2] {
            "all" => Ok(SlashCommandOutcome::SetThinkingMode(
                bcode_config::TuiThinkingMode::All,
            )),
            "summary" => Ok(SlashCommandOutcome::SetThinkingMode(
                bcode_config::TuiThinkingMode::Summary,
            )),
            "raw" => Ok(SlashCommandOutcome::SetThinkingMode(
                bcode_config::TuiThinkingMode::Raw,
            )),
            value => Ok(SlashCommandOutcome::Handled(format!(
                "unsupported reasoning display mode '{value}' (supported: all, summary, raw)"
            ))),
        },
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
                summary: status.reasoning_summary,
                status: format!("reasoning effort set to {effort}"),
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
                effort: status.reasoning_effort,
                summary: Some(summary.clone()),
                status: format!("visible reasoning summary set to {summary}"),
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
                summary: status.reasoning_summary,
                status: format!("reasoning effort set to {value}"),
            })
        }
    }
}

fn supported_reasoning_values(values: &[String]) -> Vec<String> {
    bcode_model::ordered_reasoning_effort_values(values)
}

fn unsupported_reasoning_value(
    kind: &str,
    value: &str,
    supported: Option<&[String]>,
) -> Option<String> {
    let supported = supported?;
    let ordered;
    let supported = if kind == "effort" {
        ordered = supported_reasoning_values(supported);
        ordered.as_slice()
    } else {
        supported
    };
    if supported.is_empty() || supported.iter().any(|candidate| candidate == value) {
        return None;
    }
    Some(format!(
        "unsupported reasoning {kind} '{value}' (supported: {})",
        list_or_default(supported)
    ))
}

const fn thinking_mode_label(mode: bcode_config::TuiThinkingMode) -> &'static str {
    match mode {
        bcode_config::TuiThinkingMode::All => "all",
        bcode_config::TuiThinkingMode::Summary => "summary",
        bcode_config::TuiThinkingMode::Raw => "raw",
    }
}

fn thinking_status(
    status: &bcode_ipc::SessionModelStatus,
    display_mode: bcode_config::TuiThinkingMode,
    display_visible: bool,
) -> String {
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
        .unwrap_or("not requested");
    format!(
        "reasoning request: effort={effort}, provider_summary={summary}\nlocal display: visible={display_visible}, mode={}{}",
        thinking_mode_label(display_mode),
        status
            .reasoning
            .as_ref()
            .map_or_else(String::new, |reasoning| format!(
                "\nsource: {}\navailable effort: {}\navailable visible summaries: {}",
                reasoning_source_label(reasoning.source),
                list_or_default(&supported_reasoning_values(&reasoning.effort_values)),
                list_or_default(&reasoning.summary_values)
            ))
    )
}

fn thinking_capabilities(status: &bcode_ipc::SessionModelStatus) -> String {
    let Some(reasoning) = &status.reasoning else {
        return "reasoning output: no provider-declared reasoning capabilities for this model"
            .to_owned();
    };
    format!(
        "reasoning capabilities\nsource: {}\neffort values: {}\ndefault effort: {}\nvisible summary supported: {}\nsummary values: {}\ndefault visible summary: {}\nraw reasoning exposed: {}",
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

fn resolve_working_directory_path(base: &std::path::Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn cwd_command(
    session_id: SessionId,
    working_directory: &std::path::Path,
    parts: &[&str],
) -> SlashCommandOutcome {
    if parts.len() <= 1 {
        return SlashCommandOutcome::Handled("usage: /cwd <path>".to_owned());
    }
    let requested_path = parts.iter().skip(1).copied().collect::<Vec<_>>().join(" ");
    SlashCommandOutcome::AttachWorktree {
        session_id,
        path: resolve_working_directory_path(working_directory, PathBuf::from(requested_path)),
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
    working_directory: &std::path::Path,
    parts: &[&str],
) -> Result<SlashCommandOutcome, bcode_client::ClientError> {
    match parts.get(1).copied() {
        Some("list") => {
            let response = client
                .list_worktrees(WorktreeListRequest {
                    cwd: Some(working_directory.to_path_buf()),
                })
                .await?;
            let mut lines = vec![format!(
                "worktrees for {}",
                display_from_current_dir(&response.repo_root)
            )];
            lines.extend(response.worktrees.into_iter().map(|worktree| {
                let marker = if worktree.is_main { "main" } else { "linked" };
                let branch = worktree.branch.unwrap_or_else(|| "<detached>".to_string());
                format!(
                    "* {marker} {branch} — {}",
                    display_from_current_dir(&worktree.path)
                )
            }));
            Ok(SlashCommandOutcome::SystemPlain(lines.join("\n")))
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
                path: resolve_working_directory_path(working_directory, PathBuf::from(path)),
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

fn format_build_info_markdown(info: &bcode_build_info::BuildInfo) -> String {
    use bcode_build_info::{BuildMode, GitState};

    let mode = match info.mode() {
        BuildMode::Developer => "Developer",
        BuildMode::Distribution => "Distribution",
    };
    let (commit, source_state) = match info.git() {
        GitState::Unavailable => ("unavailable".to_owned(), "Git unavailable"),
        GitState::Revision {
            short_commit,
            dirty,
        } => (short_commit.clone(), if *dirty { "Dirty" } else { "Clean" }),
    };
    let features = if info.features().is_empty() {
        "none".to_owned()
    } else {
        info.features().join(", ")
    };
    let built_at = info
        .built_at_unix_seconds()
        .map_or_else(|| "unavailable".to_owned(), format_unix_timestamp_utc);
    let release_channel = info.release_channel().unwrap_or("unavailable");
    format!(
        "# Bcode build\n\n* **Version:** `{}`\n* **Mode:** {mode}\n* **Crate version:** `{}`\n* **Git commit:** `{commit}`\n* **Source state:** {source_state}\n* **Build digest:** `{}`\n* **Target:** `{}`\n* **Profile:** `{}`\n* **Features:** `{features}`\n* **Compiler:** `{}`\n* **Artifact ID:** `{}`\n* **Release channel:** `{release_channel}`\n* **Built/released at:** `{built_at}`",
        info.display_version(),
        info.version(),
        info.digest(),
        info.target(),
        info.profile(),
        info.compiler(),
        bcode_ipc::ARTIFACT_ID,
    )
}

fn format_unix_timestamp_utc(timestamp: u64) -> String {
    let days = timestamp / 86_400;
    let seconds = timestamp % 86_400;
    let (year, month, day) = civil_date_from_days(i64::try_from(days).unwrap_or(i64::MAX));
    let hour = seconds / 3_600;
    let minute = seconds % 3_600 / 60;
    let second = seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_date_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let days = days_since_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn slash_client_issue(label: &str, error: &bcode_client::ClientError) -> SlashCommandOutcome {
    SlashCommandOutcome::Handled(daemon_issue::client_issue_status(label, error))
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
    context: SlashExecutionContext<'_>,
    message: &str,
    resolution: slash_registry::SlashResolution,
) -> Result<SlashCommandOutcome, bcode_client::ClientError> {
    let parts = message.split_whitespace().collect::<Vec<_>>();
    let outcome = match resolution {
        slash_registry::SlashResolution::Builtin(command) => {
            execute_builtin(client, session_id, context, message, &parts, command.name()).await
        }
        slash_registry::SlashResolution::SkillAlias {
            skill_id,
            arguments,
        } => Ok(SlashCommandOutcome::InvokeSkill {
            skill_id,
            arguments,
        }),
        slash_registry::SlashResolution::PluginCommand(contribution) => {
            let contribution = *contribution;
            Ok(SlashCommandOutcome::PluginCommand {
                action: contribution.action,
                execution: contribution.execution,
                arguments: parts.iter().skip(1).copied().collect::<Vec<_>>().join(" "),
            })
        }
        slash_registry::SlashResolution::Unknown => {
            Ok(SlashCommandOutcome::Unknown(message.to_owned()))
        }
    };
    match outcome {
        Ok(outcome) => Ok(outcome),
        Err(error) => Ok(slash_client_issue("slash command unavailable", &error)),
    }
}

fn theme_command(parts: &[&str]) -> SlashCommandOutcome {
    match parts {
        [_] => SlashCommandOutcome::OpenThemePicker,
        [_, "list"] => SlashCommandOutcome::ShowThemeCatalog,
        [_, "preview", theme_id] => SlashCommandOutcome::PreviewTheme {
            theme_id: (*theme_id).to_owned(),
        },
        [_, "apply", theme_id] => SlashCommandOutcome::ApplyTheme {
            theme_id: (*theme_id).to_owned(),
        },
        [_, "reset"] => SlashCommandOutcome::ResetTheme,
        [_, "cancel"] => SlashCommandOutcome::CancelThemePreview,
        _ => SlashCommandOutcome::Handled(
            "usage: /theme [list|preview <theme>|apply <theme>|reset|cancel]".to_owned(),
        ),
    }
}

pub fn format_theme_catalog_markdown(view: &super::theme::ThemeCatalogView) -> String {
    if view.entries.is_empty() {
        return "# Themes\n\nNo valid themes are currently available.".to_owned();
    }
    let mut output = String::from("# Themes\n\nUse `/theme` to open the interactive picker.\n\n");
    for entry in &view.entries {
        let current = if entry.selected { " (current)" } else { "" };
        let variants = match (entry.has_dark_variant, entry.has_light_variant) {
            (true, true) => "dark, light",
            (true, false) => "dark",
            (false, true) => "light",
            (false, false) => "default",
        };
        let _ = writeln!(
            output,
            "* **{}**{current} — {} (`{}`; {variants})",
            entry.display_name, entry.source, entry.id
        );
    }
    if !view.diagnostics.is_empty() {
        output.push_str("\n## Rejected definitions\n\n");
        for diagnostic in &view.diagnostics {
            let _ = writeln!(output, "* {diagnostic}");
        }
    }
    output
}

#[allow(clippy::too_many_lines)]
async fn execute_builtin(
    client: &BcodeClient,
    session_id: Option<SessionId>,
    context: SlashExecutionContext<'_>,
    message: &str,
    parts: &[&str],
    command: &str,
) -> Result<SlashCommandOutcome, bcode_client::ClientError> {
    match command {
        "version" => Ok(SlashCommandOutcome::SystemMarkdown(
            format_build_info_markdown(&super::build_info()),
        )),
        "sessions" => Ok(SlashCommandOutcome::PickSession),
        "search" => Ok(SlashCommandOutcome::SearchSessions),
        "resync" => resync_command(client, parts).await,
        "rescan-imports" => client.refresh_session_catalog(None).await.map(|list| {
            SlashCommandOutcome::Handled(format!(
                "session catalog refresh requested (revision {})",
                list.catalog_revision
            ))
        }),
        "new" => Ok(SlashCommandOutcome::NewDraftSession),
        "plan" | "build" | "agent" => {
            handle_agent_command(client, session_id, context.current_agent_id, parts).await
        }
        "compact" => {
            let Some(session_id) = session_id else {
                return Ok(SlashCommandOutcome::Handled(
                    "compact requires an active session".to_owned(),
                ));
            };
            Ok(SlashCommandOutcome::CompactContext { session_id })
        }
        "theme" => Ok(theme_command(parts)),
        "streaming" => Ok(SlashCommandOutcome::OpenStreamingConfigurator),
        "model" | "models" if parts.len() == 1 => Ok(SlashCommandOutcome::PickModel),
        "auth-pool" | "subscriptions" if parts.len() == 1 => Ok(SlashCommandOutcome::PickAuthPool),
        "model" | "set-model" if parts.len() > 1 => {
            let model_id = parts[1].to_owned();
            if let Some(session_id) = session_id {
                Ok(SlashCommandOutcome::SetSessionModel {
                    session_id,
                    provider_plugin_id: None,
                    model_id,
                })
            } else {
                Ok(SlashCommandOutcome::SetLocalModel {
                    provider_plugin_id: None,
                    model_id,
                })
            }
        }
        "provider" | "set-provider" if parts.len() > 1 => {
            let provider = parts[1].to_owned();
            let status = if let Some(session_id) = session_id {
                client.session_model_status(session_id).await?
            } else {
                client.default_model_status().await?
            };
            let model_id = status.model_id.unwrap_or_else(|| "default".to_owned());
            if let Some(session_id) = session_id {
                Ok(SlashCommandOutcome::SetSessionModel {
                    session_id,
                    provider_plugin_id: Some(provider),
                    model_id,
                })
            } else {
                Ok(SlashCommandOutcome::SetLocalModel {
                    provider_plugin_id: Some(provider),
                    model_id,
                })
            }
        }
        "provider" => {
            let status = if let Some(session_id) = session_id {
                client.session_model_status(session_id).await?
            } else {
                client.default_model_status().await?
            };
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
        "cwd" => {
            let Some(session_id) = session_id else {
                return Ok(SlashCommandOutcome::Handled(
                    "cwd requires an active session".to_owned(),
                ));
            };
            Ok(cwd_command(session_id, context.working_directory, parts))
        }
        "worktree" | "worktrees" => {
            worktree_command(client, session_id, context.working_directory, parts).await
        }
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
            thinking_command(
                client,
                session_id,
                parts,
                context.reasoning_display_mode,
                context.reasoning_visible,
            )
            .await
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
    fn version_markdown_contains_detailed_build_facts() {
        let info = bcode_build_info::BuildInfo::new(
            "1.2.3",
            bcode_build_info::BuildMode::Distribution,
            bcode_build_info::GitState::Revision {
                short_commit: "abcdef12".to_owned(),
                dirty: false,
            },
            "1234abcd",
        )
        .and_then(|info| {
            info.with_diagnostics(
                "aarch64-apple-darwin",
                "release",
                vec!["app".to_owned()],
                "rustc 1.95.0",
                Some("stable".to_owned()),
                Some(1_735_689_600),
            )
        })
        .expect("build info");
        let markdown = format_build_info_markdown(&info);
        for expected in [
            "# Bcode build",
            "`v1.2.3`",
            "Git commit:** `abcdef12`",
            "Target:** `aarch64-apple-darwin`",
            "Release channel:** `stable`",
            "2025-01-01T00:00:00Z",
        ] {
            assert!(markdown.contains(expected), "{expected}");
        }
    }

    #[test]
    fn unix_timestamp_format_is_utc() {
        assert_eq!(format_unix_timestamp_utc(1), "1970-01-01T00:00:01Z");
        assert_eq!(
            format_unix_timestamp_utc(1_735_689_600),
            "2025-01-01T00:00:00Z"
        );
    }

    #[test]
    fn thinking_mode_slash_parser_covers_every_local_mode() {
        for (value, mode) in [
            ("all", bcode_config::TuiThinkingMode::All),
            ("summary", bcode_config::TuiThinkingMode::Summary),
            ("raw", bcode_config::TuiThinkingMode::Raw),
        ] {
            assert_eq!(
                draft_thinking_command(&["/thinking", "mode", value]),
                SlashCommandOutcome::SetThinkingMode(mode)
            );
        }
        assert!(matches!(
            draft_thinking_command(&["/thinking", "mode", "other"]),
            SlashCommandOutcome::Handled(message) if message.contains("supported: all, summary, raw")
        ));
    }

    #[test]
    fn effort_validation_reports_semantic_supported_order() {
        let supported = vec!["high".to_owned(), "none".to_owned(), "low".to_owned()];
        assert_eq!(
            unsupported_reasoning_value("effort", "turbo", Some(&supported)).as_deref(),
            Some("unsupported reasoning effort 'turbo' (supported: none, low, high)")
        );
        assert!(unsupported_reasoning_value("effort", "low", Some(&supported)).is_none());
    }

    #[test]
    fn thinking_status_distinguishes_provider_request_from_local_display() {
        let status = bcode_ipc::SessionModelStatus {
            provider_plugin_id: Some("provider".to_owned()),
            requested_model_id: Some("model".to_owned()),
            effective_model_id: Some("model".to_owned()),
            model_id: Some("model".to_owned()),
            context_window: None,
            context_occupancy: None,
            request_context_error: None,
            auth_profile: None,
            context_format_version: None,
            compatibility_key: None,
            max_output_tokens: None,
            reasoning: None,
            reasoning_effort: Some("high".to_owned()),
            reasoning_summary: Some("detailed".to_owned()),
            prompt_cache_mode: None,
            conversation_reuse_mode: None,
            compaction_mode: None,
            compaction_backend: None,
            proactive_compaction_threshold_percent: None,
            cache: None,
            metadata_source: None,
            pricing: None,
        };

        let text = thinking_status(&status, bcode_config::TuiThinkingMode::Raw, false);
        assert!(text.contains("reasoning request: effort=high, provider_summary=detailed"));
        assert!(text.contains("local display: visible=false, mode=raw"));
    }

    #[test]
    fn skill_details_formatter_preserves_markdown_instructions() {
        let output = format_skill_details_markdown(
            "Review",
            "review",
            "user config",
            Some("Review code."),
            "## Steps\n\n1. Read [guide](https://example.com).\n2. Run:\n\n```sh\ncargo test\n```",
        );

        assert!(output.starts_with("# Review\n\n"));
        assert!(output.contains("* **ID:** `review`"));
        assert!(output.contains("## Instructions\n\n## Steps"));
        assert!(output.contains("```sh\ncargo test\n```"));
    }

    #[test]
    fn bare_theme_opens_picker_and_list_uses_durable_catalog() {
        assert_eq!(
            theme_command(&["/theme"]),
            SlashCommandOutcome::OpenThemePicker
        );
        assert_eq!(
            theme_command(&["/theme", "list"]),
            SlashCommandOutcome::ShowThemeCatalog
        );
        assert_eq!(
            theme_command(&["/theme", "apply", "bcode-light"]),
            SlashCommandOutcome::ApplyTheme {
                theme_id: "bcode-light".to_owned()
            }
        );
        assert_eq!(
            theme_command(&["/theme", "reset"]),
            SlashCommandOutcome::ResetTheme
        );
    }

    #[test]
    fn theme_catalog_markdown_is_durable_and_actionable() {
        let view = super::super::theme::ThemeCatalogView {
            entries: vec![super::super::theme::ThemeCatalogEntry {
                id: "bcode-light".to_owned(),
                display_name: "Bcode Light".to_owned(),
                source: "bundled".to_owned(),
                has_dark_variant: false,
                has_light_variant: true,
                validation: "valid".to_owned(),
                selected: true,
            }],
            diagnostics: vec!["broken.toml: invalid color".to_owned()],
        };

        let markdown = format_theme_catalog_markdown(&view);
        assert!(markdown.contains("Use `/theme` to open the interactive picker"));
        assert!(markdown.contains("**Bcode Light** (current)"));
        assert!(markdown.contains("`bcode-light`; light"));
        assert!(markdown.contains("Rejected definitions"));
        assert!(markdown.contains("broken.toml: invalid color"));
    }

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
    fn relative_session_paths_resolve_against_the_active_working_directory() {
        let session_id = SessionId::new();
        let base = std::path::Path::new("/workspace/project");

        assert_eq!(
            cwd_command(session_id, base, &["/cwd", "../worktree"]),
            SlashCommandOutcome::AttachWorktree {
                session_id,
                path: base.join("../worktree"),
            }
        );
        assert_eq!(
            cwd_command(session_id, base, &["/cwd", "/tmp/absolute"]),
            SlashCommandOutcome::AttachWorktree {
                session_id,
                path: std::path::PathBuf::from("/tmp/absolute"),
            }
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
