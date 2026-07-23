//! Composer submission flow for the TUI.

use std::io::Write;

use bcode_session_models::SessionId;

use super::activity::ActivityState;
use super::app::DaemonConnectionState;
use super::effects::{SubmitMessageRequest, TuiEffect};
use super::runtime_context::{TuiIo, TuiServices};
use super::session_flow::ActiveChat;
use super::{
    TuiError, helpers, model_flow, ralph_flow, session_flow, session_fork_flow, skill_flow,
    slash_commands, slash_registry, thinking_dialog, worktree_flow,
};
use bcode_session_models::WorkId;

/// Result of submitting staged composer text.
pub type SubmitComposerOutcome = Option<ComposerModalRequest>;

/// Modal requested by a composer submission.
pub enum ComposerModalRequest {
    /// Open reasoning output settings.
    Thinking(thinking_dialog::ThinkingDialogState),
    /// Open timeline browser.
    Timeline(super::timeline_dialog::TimelineDialogState),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposerSubmissionRoute {
    PluginOrBuiltinCommand,
    SessionMessage,
}

fn composer_submission_route(
    message: &str,
    resolution: Option<&slash_registry::SlashResolution>,
) -> ComposerSubmissionRoute {
    if slash_registry::slash_command_name(message).is_some()
        && resolution.is_some_and(slash_registry::SlashResolution::is_known)
    {
        ComposerSubmissionRoute::PluginOrBuiltinCommand
    } else {
        ComposerSubmissionRoute::SessionMessage
    }
}

fn agent_selection_status(chat: &ActiveChat, agent_name: &str) -> String {
    if matches!(chat.app.activity(), ActivityState::Idle) {
        format!("agent {agent_name} selected")
    } else {
        format!("agent {agent_name} selected for next message")
    }
}

fn apply_draft_agent_selection(
    chat: &mut ActiveChat,
    agent_id: String,
    agent_name: &str,
    agent_accent: Option<String>,
) {
    if chat.app.session_id().is_some() {
        chat.app.set_pending_agent(agent_id, agent_accent);
        chat.app
            .set_status(agent_selection_status(chat, agent_name));
    } else {
        chat.app.set_current_agent(agent_id, agent_accent);
        chat.app.set_status(format!("agent set to {agent_name}"));
    }
}

async fn open_thinking_settings(
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
    session_id: Option<SessionId>,
    focus: thinking_dialog::ThinkingDialogFocus,
) -> Result<SubmitComposerOutcome, TuiError> {
    let status = match if let Some(session_id) = session_id {
        services.client.session_model_status(session_id).await
    } else {
        services.client.default_model_status().await
    } {
        Ok(status) => status,
        Err(error) => {
            helpers::report_client_issue(
                &mut chat.app,
                "reasoning output settings unavailable",
                &error,
            );
            return Ok(None);
        }
    };
    chat.app.apply_model_status(status.clone());
    chat.app
        .set_status("reasoning output settings: enter apply, esc cancel".to_owned());
    Ok(Some(ComposerModalRequest::Thinking(
        thinking_dialog::ThinkingDialogState::new_focused(
            chat.app.reasoning_visible(),
            &status,
            focus,
        ),
    )))
}

#[allow(clippy::too_many_lines)]
async fn handle_slash_command<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
    session_id: Option<SessionId>,
    message: &str,
    resolution: slash_registry::SlashResolution,
) -> Result<SubmitComposerOutcome, TuiError> {
    match slash_commands::execute_resolved(
        services.passive_client,
        session_id,
        chat.app
            .working_directory()
            .unwrap_or(services.launch_working_directory),
        chat.app.current_agent_id(),
        message,
        resolution,
    )
    .await?
    {
        slash_commands::SlashCommandOutcome::Unknown(_command) => {}
        slash_commands::SlashCommandOutcome::Handled(status) => {
            chat.app.clear_pending_submission(message);
            chat.app.set_status(status);
        }
        slash_commands::SlashCommandOutcome::SetThinkingDisplay(show) => {
            chat.app.clear_pending_submission(message);
            chat.app.set_reasoning_visible(show);
            chat.app.set_status(if show {
                "reasoning output shown".to_owned()
            } else {
                "reasoning output hidden".to_owned()
            });
        }
        slash_commands::SlashCommandOutcome::ToggleThinkingDisplay => {
            chat.app.clear_pending_submission(message);
            let show = !chat.app.reasoning_visible();
            chat.app.set_reasoning_visible(show);
            chat.app.set_status(if show {
                "reasoning output shown".to_owned()
            } else {
                "reasoning output hidden".to_owned()
            });
        }
        slash_commands::SlashCommandOutcome::SystemNote(note) => {
            chat.app.clear_pending_submission(message);
            chat.app.push_system_note(note);
            chat.app.set_status("slash command handled".to_owned());
        }
        slash_commands::SlashCommandOutcome::OpenThinkingSettings(focus) => {
            chat.app.clear_pending_submission(message);
            return open_thinking_settings(services, chat, session_id, focus).await;
        }
        slash_commands::SlashCommandOutcome::OpenTimeline => {
            chat.app.clear_pending_submission(message);
            let entries = if session_id.is_some() {
                chat.app.timeline_entries()
            } else {
                Vec::new()
            };
            chat.app.set_status(if entries.is_empty() {
                "timeline: no user messages".to_owned()
            } else {
                "timeline: select a user message".to_owned()
            });
            return Ok(Some(ComposerModalRequest::Timeline(
                super::timeline_dialog::TimelineDialogState::new(entries),
            )));
        }
        slash_commands::SlashCommandOutcome::NewDraftSession => {
            chat.app.clear_pending_submission(message);
            session_flow::switch_to_draft_session(chat);
            chat.replace_effect(TuiEffect::LoadDraftStatus {
                launch_working_directory: std::env::current_dir()?,
            });
        }
        slash_commands::SlashCommandOutcome::DraftAgentSelected {
            agent_id,
            agent_name,
            agent_accent,
        } => {
            chat.app.clear_pending_submission(message);
            apply_draft_agent_selection(chat, agent_id, &agent_name, agent_accent);
        }
        slash_commands::SlashCommandOutcome::PluginCommand {
            action,
            execution,
            arguments,
        } => {
            chat.app.clear_pending_submission(message);
            super::palette_flow::execute_plugin_slash_command(
                io, services, chat, action, execution, arguments,
            )
            .await?;
        }
        slash_commands::SlashCommandOutcome::PickSession => {
            chat.app.clear_pending_submission(message);
            match session_flow::pick_session(io, services, chat).await? {
                session_flow::PickSessionOutcome::Existing(next_session_id) => {
                    session_flow::switch_session(io.terminal, chat, next_session_id)?;
                }
                session_flow::PickSessionOutcome::Draft => {
                    session_flow::switch_to_draft_session(chat);
                    chat.replace_effect(TuiEffect::LoadDraftStatus {
                        launch_working_directory: std::env::current_dir()?,
                    });
                }
            }
        }
        slash_commands::SlashCommandOutcome::PickModel => {
            chat.app.clear_pending_submission(message);
            model_flow::pick_model_for_session(io, services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::PickSkill => {
            chat.app.clear_pending_submission(message);
            skill_flow::pick_skill_for_session(io, services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::InvokeSkill {
            skill_id,
            arguments,
        } => {
            chat.app.clear_pending_submission(message);
            skill_flow::start_invoke_skill_for_session(chat, skill_id, arguments)?;
        }
        slash_commands::SlashCommandOutcome::OpenWorktreeCreateDialog => {
            chat.app.clear_pending_submission(message);
            worktree_flow::create_for_current_session(io, services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::OpenForkSessionWizard => {
            chat.app.clear_pending_submission(message);
            session_fork_flow::fork_current_session(io, services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::CloneSession { session_id, name } => {
            chat.app.clear_pending_submission(message);
            chat.start_effect(TuiEffect::CloneSession {
                session_id,
                name,
                switch_after_create: true,
                install_draft: true,
                initial_window_request: super::history_flow::initial_transcript_window_request(
                    super::render::transcript_area_for_frame(&chat.app, io.terminal.area()),
                ),
            });
            chat.app.set_status("cloning session…".to_owned());
        }
        slash_commands::SlashCommandOutcome::SetLocalModel {
            provider_plugin_id,
            model_id,
        } => {
            chat.app.clear_pending_submission(message);
            chat.app
                .apply_local_model_selection(provider_plugin_id, &model_id);
        }
        slash_commands::SlashCommandOutcome::SetSessionModel {
            session_id,
            provider_plugin_id,
            model_id,
        } => {
            chat.app.clear_pending_submission(message);
            chat.start_effect(TuiEffect::SetSessionModel {
                session_id,
                provider_plugin_id,
                model_id,
            });
            chat.app.set_status("setting model…".to_owned());
        }
        slash_commands::SlashCommandOutcome::SetSessionReasoning {
            session_id,
            effort,
            summary,
            status,
        } => {
            chat.app.clear_pending_submission(message);
            chat.start_effect(TuiEffect::SetSessionReasoning {
                session_id,
                effort,
                summary,
                status,
            });
            chat.app.set_status("setting thinking…".to_owned());
        }
        slash_commands::SlashCommandOutcome::CancelTurn { session_id } => {
            chat.app.clear_pending_submission(message);
            chat.start_effect(TuiEffect::CancelTurn { session_id });
            chat.app.set_cancelling();
            chat.app.set_status("requesting cancellation…".to_owned());
        }
        slash_commands::SlashCommandOutcome::CancelRuntimeWork {
            session_id,
            work_id,
        } => {
            chat.app.clear_pending_submission(message);
            chat.start_effect(TuiEffect::CancelRuntimeWork {
                session_id,
                work_id: WorkId::new(work_id),
            });
            chat.app
                .set_status("requesting runtime cancellation…".to_owned());
        }
        slash_commands::SlashCommandOutcome::CompactContext { session_id } => {
            chat.app.clear_pending_submission(message);
            chat.start_effect(TuiEffect::CompactContext { session_id });
            chat.app.set_status("compacting context…".to_owned());
        }
        slash_commands::SlashCommandOutcome::AttachWorktree { session_id, path } => {
            chat.app.clear_pending_submission(message);
            chat.start_effect(TuiEffect::AttachWorktree { session_id, path });
            chat.app.set_status("attaching worktree…".to_owned());
        }
        slash_commands::SlashCommandOutcome::OpenRalphHome => {
            chat.app.clear_pending_submission(message);
            ralph_flow::open_home(io, services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::OpenRalphStartDialog => {
            chat.app.clear_pending_submission(message);
            ralph_flow::start_loop(io, services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::ShowRalphStatus => {
            chat.app.clear_pending_submission(message);
            ralph_flow::show_status(services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::RunRalphLoop => {
            chat.app.clear_pending_submission(message);
            ralph_flow::run_loop(services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::ApproveRalphRun => {
            chat.app.clear_pending_submission(message);
            ralph_flow::approve_run(services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::StopRalphLoop => {
            chat.app.clear_pending_submission(message);
            ralph_flow::stop_loop(services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::ListRalphRuns => {
            chat.app.clear_pending_submission(message);
            ralph_flow::list_runs(services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::ListRalphIterations => {
            chat.app.clear_pending_submission(message);
            ralph_flow::list_iterations(services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::ResumeRalphRun => {
            chat.app.clear_pending_submission(message);
            ralph_flow::resume_run(services, chat).await?;
        }
        slash_commands::SlashCommandOutcome::OpenRalphProgress => {
            chat.app.clear_pending_submission(message);
            ralph_flow::open_progress(chat)?;
        }
        slash_commands::SlashCommandOutcome::BuildRalphPrompt(kind) => {
            chat.app.clear_pending_submission(message);
            ralph_flow::show_prompt(chat, kind)?;
        }
    }
    Ok(None)
}

/// Submit the staged composer text.
pub async fn submit_composer<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
    placement: bcode_ipc::PromptPlacement,
) -> Result<SubmitComposerOutcome, TuiError> {
    let session_id = chat.app.session_id();
    let message = chat.app.take_pending_submission();
    if message.trim().is_empty() {
        chat.app.clear_pending_submission(&message);
        return Ok(None);
    }
    let slash_resolution = if slash_registry::slash_command_name(&message).is_some() {
        slash_registry::resolve(services.passive_client, &message)
            .await
            .ok()
    } else {
        None
    };
    if composer_submission_route(&message, slash_resolution.as_ref())
        == ComposerSubmissionRoute::PluginOrBuiltinCommand
    {
        let resolution = slash_resolution.expect("known slash resolution");
        if session_id.is_some() || resolution.is_draft_safe() {
            return handle_slash_command(io, services, chat, session_id, &message, resolution)
                .await;
        }
        chat.app.clear_pending_submission(&message);
        chat.app.set_status(
            "slash command requires an active session; send a message first".to_owned(),
        );
        return Ok(None);
    }
    let agent_id = if session_id.is_some() {
        chat.app.pending_agent_id().map(ToOwned::to_owned)
    } else {
        let current = chat.app.current_agent_id().to_owned();
        (current != "build").then_some(current)
    };
    let draft_provider_plugin_id = if session_id.is_none() {
        chat.app
            .selected_provider_plugin_id()
            .map(ToOwned::to_owned)
    } else {
        None
    };
    let draft_model_id = if session_id.is_none() {
        chat.app.selected_model_id().map(ToOwned::to_owned)
    } else {
        None
    };
    chat.start_effect(TuiEffect::SubmitMessage {
        request: Box::new(SubmitMessageRequest {
            session_id,
            launch_working_directory: std::env::current_dir()?,
            message,
            placement,
            provider_plugin_id: draft_provider_plugin_id,
            model_id: draft_model_id,
            agent_id,
            reasoning_effort: chat.app.reasoning_effort().map(ToOwned::to_owned),
            reasoning_summary: chat.app.reasoning_summary().map(ToOwned::to_owned),
            event_sender: chat.event_sender.clone(),
        }),
    });
    chat.app
        .set_daemon_connection(DaemonConnectionState::Starting);
    chat.app.set_status("starting daemon…".to_owned());
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_command::{
        CommandAction, CommandContribution, CommandExecution, CommandOwner, CommandSurface,
    };
    use std::collections::BTreeSet;

    fn loop_command() -> slash_registry::SlashResolution {
        slash_registry::SlashResolution::PluginCommand(CommandContribution {
            id: "loop".to_owned(),
            title: "Loop".to_owned(),
            description: None,
            category: Some("automation".to_owned()),
            surfaces: BTreeSet::from([CommandSurface::Slash]),
            execution: CommandExecution::Immediate,
            owner: CommandOwner::Plugin {
                plugin_id: "bcode.loop".to_owned(),
            },
            action: CommandAction::Plugin {
                plugin_id: "bcode.loop".to_owned(),
                command_id: "loop".to_owned(),
            },
        })
    }

    #[test]
    fn ordinary_messages_and_loop_commands_keep_distinct_routes() {
        assert_eq!(
            composer_submission_route("ordinary steering", None),
            ComposerSubmissionRoute::SessionMessage
        );
        let resolution = loop_command();
        assert_eq!(
            composer_submission_route("/loop stop", Some(&resolution)),
            ComposerSubmissionRoute::PluginOrBuiltinCommand
        );
    }

    #[test]
    fn active_plugin_status_does_not_block_ordinary_composer_staging() {
        let mut app = super::super::app::BmuxApp::new_with_history(None, &[], &[], false);
        app.set_plugin_status(vec![bcode_session_view_models::PluginStatusView {
            plugin_id: "bcode.loop".to_owned(),
            note_id: "loop-active".to_owned(),
            text: "Loop active".to_owned(),
            priority: 1,
            metadata: std::collections::BTreeMap::new(),
        }]);
        app.paste_composer_text("manual steering");
        app.stage_submission();

        assert_eq!(app.take_pending_submission(), "manual steering");
        assert_eq!(
            app.plugin_status()
                .next()
                .map(|status| status.text.as_str()),
            Some("Loop active")
        );
    }

    #[test]
    fn unknown_slash_text_remains_an_ordinary_session_message() {
        assert_eq!(
            composer_submission_route(
                "/not-a-command",
                Some(&slash_registry::SlashResolution::Unknown)
            ),
            ComposerSubmissionRoute::SessionMessage
        );
    }
}
