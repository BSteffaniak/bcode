//! Composer submission flow for the TUI.

use super::effects::{SubmitMessageRequest, TuiEffect};
use super::session_flow::ActiveChat;
use super::slash_registry;

fn defer_submission_while_session_opens(chat: &mut ActiveChat) -> bool {
    if chat.opening_session_id().is_none() {
        return false;
    }
    chat.app
        .set_status("Session is still opening; message kept in composer".to_owned());
    true
}

fn retain_rejected_managed_input(chat: &mut ActiveChat) {
    if chat.app.composer().text().is_empty() {
        let text = chat.app.take_pending_submission();
        if !text.is_empty() {
            chat.app.clear_pending_submission(&text);
            chat.app.replace_composer_with(&text);
        }
    }
}

pub enum RootSubmission {
    MessageStaged(bool),
    SlashCommand(String),
}

pub fn stage_root_submission(
    launch_working_directory: &std::path::Path,
    launch_options: super::TuiLaunchOptions,
    chat: &mut ActiveChat,
    placement: bcode_ipc::PromptPlacement,
) -> RootSubmission {
    if chat.opening_session_id().is_some() {
        chat.app
            .set_status("Session is still opening; message kept in composer".to_owned());
        return RootSubmission::MessageStaged(false);
    }
    if chat.app.managed_execution_session().is_some_and(|owner| {
        !chat.app.managed_input_ready() || chat.attached_session_id() != Some(owner)
    }) {
        retain_rejected_managed_input(chat);
        chat.app
            .set_status("Managed view unavailable; reattach explicitly before sending".into());
        return RootSubmission::MessageStaged(false);
    }
    let message = chat.app.take_pending_submission();
    // Allow-listed native slash commands (for example /compact) are ordinary managed input.
    let native_passthrough = chat.app.managed_execution_session().is_some()
        && super::managed_controls::is_native_passthrough(&message);
    if !native_passthrough && slash_registry::slash_command_name(&message).is_some() {
        chat.app.clear_pending_submission(&message);
        return RootSubmission::SlashCommand(message);
    }
    chat.app.restore_pending_submission(&message);
    chat.app.stage_submission();
    RootSubmission::MessageStaged(stage_session_message(
        launch_working_directory,
        launch_options,
        chat,
        placement,
    ))
}

fn managed_input_available(
    chat: &mut ActiveChat,
    session_id: Option<bcode_session_models::SessionId>,
) -> bool {
    if let Some(owner) = chat.app.managed_execution_session() {
        if !chat.app.managed_input_ready() {
            retain_rejected_managed_input(chat);
            chat.app.set_status(
                "Managed event continuity lost; reattach explicitly before sending".into(),
            );
            return false;
        }
        if session_id != Some(owner) {
            retain_rejected_managed_input(chat);
            chat.app
                .set_status("Managed session changed; reattach explicitly before sending".into());
            return false;
        }
        if chat.app.managed_composer.placement.is_none() {
            chat.app.managed_composer.placement =
                Some(bcode_harness_models::HarnessPlacement::NextTurn);
        }
    }
    true
}

pub fn stage_session_message(
    launch_working_directory: &std::path::Path,
    launch_options: super::TuiLaunchOptions,
    chat: &mut ActiveChat,
    placement: bcode_ipc::PromptPlacement,
) -> bool {
    if defer_submission_while_session_opens(chat) {
        return false;
    }
    let session_id = chat.attached_session_id();
    if !managed_input_available(chat, session_id) {
        return false;
    }
    if let Some(session_id) = session_id {
        let unresolved = chat.app.managed_composer.requests.values().any(|request| {
            let (owner, _) = request.target();
            owner == session_id.to_string()
        });
        if unresolved {
            chat.app.set_status("Native input remains unresolved. Use /native retry <submission-id> for the retained attempt; no new input was sent".into());
            return false;
        }
    }
    let message = chat.app.take_pending_submission();
    if message.trim().is_empty() {
        chat.app.clear_pending_submission(&message);
        return false;
    }
    let native_passthrough = chat.app.managed_execution_session().is_some()
        && super::managed_controls::is_native_passthrough(&message);
    if !native_passthrough && slash_registry::slash_command_name(&message).is_some() {
        chat.app.clear_pending_submission(&message);
        chat.app
            .set_status("slash command pending root navigation migration".to_owned());
        return false;
    }
    let agent_id = chat
        .app
        .managed_execution_session()
        .is_none()
        .then(|| chat.app.display_agent_id().to_owned());
    let draft_provider_plugin_id = session_id
        .is_none()
        .then(|| {
            chat.app
                .selected_provider_plugin_id()
                .map(ToOwned::to_owned)
        })
        .flatten();
    let draft_model_id = session_id
        .is_none()
        .then(|| chat.app.selected_model_id().map(ToOwned::to_owned))
        .flatten();
    let expected_native_turn_id = match &chat.app.managed_composer.placement {
        Some(bcode_harness_models::HarnessPlacement::Steer {
            expected_native_turn_id,
        }) => Some(expected_native_turn_id.clone()),
        _ => None,
    };
    let placement = match &chat.app.managed_composer.placement {
        Some(bcode_harness_models::HarnessPlacement::NextTurn) => {
            bcode_ipc::PromptPlacement::FollowUp
        }
        Some(bcode_harness_models::HarnessPlacement::Steer { .. }) => {
            bcode_ipc::PromptPlacement::Steering
        }
        None => placement,
    };
    chat.start_effect(TuiEffect::SubmitMessage {
        request: Box::new(SubmitMessageRequest {
            managed: None,
            managed_only: chat.app.managed_execution_session().is_some()
                || chat.app.managed_composer.placement.is_some(),
            attempt_id: bcode_session_models::SessionId::new().to_string(),
            expected_native_turn_id,
            submission_id: chat
                .app
                .pending_submissions()
                .last()
                .expect("staged submission")
                .id()
                .clone(),
            session_id,
            launch_working_directory: launch_working_directory.to_path_buf(),
            message,
            placement,
            provider_plugin_id: draft_provider_plugin_id,
            model_id: draft_model_id,
            agent_id,
            reasoning_effort: chat.app.reasoning_effort().map(ToOwned::to_owned),
            reasoning_summary: chat.app.reasoning_summary().map(ToOwned::to_owned),
            reasoning_effort_generation: chat.app.pending_reasoning_effort_generation(),
            execution: launch_options.turn_execution_options(),
            event_sender: chat.event_sender.clone(),
        }),
    });
    chat.app.set_status("sending message…".to_owned());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_managed_composer_fences_discontinuity_and_foreign_session_without_fallback() {
        use crate::session_flow::{AgentCatalog, ChatSessionAttachment};
        let owner = bcode_session_models::SessionId::new();
        let (event_sender, event_receiver) = crate::history_flow::session_stream_channel();
        let mut chat = ActiveChat {
            app: crate::app::BmuxApp::new_with_history(Some(owner), &[], &[]),
            agents: AgentCatalog::default(),
            attachment: ChatSessionAttachment::Attached { session_id: owner },
            event_sender,
            event_receiver,
            event_task: None,
            opening_session_progress: None,
            pending_effects: crate::effects::TuiEffectQueue::default(),
        };
        chat.app.set_managed_execution(owner);
        chat.app.replace_composer_with("native-only input");
        chat.app.stage_submission();
        assert!(stage_session_message(
            std::path::Path::new("."),
            crate::TuiLaunchOptions::default(),
            &mut chat,
            bcode_ipc::PromptPlacement::Steering
        ));
        let (effects, _) = chat.pending_effects.drain_runtime();
        let request = effects
            .into_iter()
            .find_map(|(_, effect)| match effect {
                TuiEffect::SubmitMessage { request } => Some(request),
                _ => None,
            })
            .unwrap();
        assert!(request.managed_only);
        assert_eq!(request.session_id, Some(owner));
        assert_eq!(request.placement, bcode_ipc::PromptPlacement::FollowUp);
        assert!(request.provider_plugin_id.is_none() && request.model_id.is_none());
        chat.app.replace_composer_with("keep this input");
        chat.app.stage_submission();
        chat.app.suspend_managed_input();
        assert!(!stage_session_message(
            std::path::Path::new("."),
            crate::TuiLaunchOptions::default(),
            &mut chat,
            bcode_ipc::PromptPlacement::Steering
        ));
        assert!(chat.pending_effects.drain_runtime().0.is_empty());
        assert_eq!(chat.app.composer().text(), "keep this input");
        chat.app.set_managed_execution(owner);
        chat.attachment = ChatSessionAttachment::Attached {
            session_id: bcode_session_models::SessionId::new(),
        };
        assert!(!stage_session_message(
            std::path::Path::new("."),
            crate::TuiLaunchOptions::default(),
            &mut chat,
            bcode_ipc::PromptPlacement::FollowUp
        ));
        assert!(chat.pending_effects.drain_runtime().0.is_empty());
        assert_eq!(chat.app.managed_execution_session(), Some(owner));
        drop(chat);
    }

    #[test]
    fn native_composer_stages_explicit_placement_and_attempt_without_model_dispatch() {
        use super::super::session_flow::{ActiveChat, AgentCatalog, ChatSessionAttachment};
        let session_id = bcode_session_models::SessionId::new();
        let (event_sender, event_receiver) = crate::history_flow::session_stream_channel();
        let mut chat = ActiveChat {
            app: super::super::app::BmuxApp::new_with_history(Some(session_id), &[], &[]),
            agents: AgentCatalog::default(),
            attachment: ChatSessionAttachment::Attached { session_id },
            event_sender,
            event_receiver,
            event_task: None,
            opening_session_progress: None,
            pending_effects: super::super::effects::TuiEffectQueue::default(),
        };
        for expected in [None, Some("turn-7".to_owned())] {
            chat.app.managed_composer.placement = Some(expected.clone().map_or(
                bcode_harness_models::HarnessPlacement::NextTurn,
                |expected_native_turn_id| bcode_harness_models::HarnessPlacement::Steer {
                    expected_native_turn_id,
                },
            ));
            chat.app.replace_composer_with("native input");
            chat.app.stage_submission();
            assert!(stage_session_message(
                std::path::Path::new("."),
                crate::TuiLaunchOptions::default(),
                &mut chat,
                bcode_ipc::PromptPlacement::Steering
            ));
            let (effects, _) = chat.pending_effects.drain_runtime();
            let request = effects
                .into_iter()
                .find_map(|(_, effect)| match effect {
                    TuiEffect::SubmitMessage { request } => Some(request),
                    _ => None,
                })
                .unwrap();
            assert!(request.managed_only);
            assert!(
                request.managed.is_none(),
                "staging must not invent a target"
            );
            assert_ne!(request.attempt_id, request.submission_id.0);
            assert_eq!(request.expected_native_turn_id, expected);
            assert_eq!(
                request.placement,
                if expected.is_some() {
                    bcode_ipc::PromptPlacement::Steering
                } else {
                    bcode_ipc::PromptPlacement::FollowUp
                }
            );
        }
        drop(chat);
    }

    #[test]
    fn submission_preserves_observed_connection_for_new_and_existing_sessions() {
        use super::super::app::{BmuxApp, DaemonConnectionState};
        use super::super::session_flow::{ActiveChat, AgentCatalog, ChatSessionAttachment};
        for attached in [false, true] {
            for connection in [
                DaemonConnectionState::Connected,
                DaemonConnectionState::Connecting,
                DaemonConnectionState::IdleOffline,
                DaemonConnectionState::Unavailable,
            ] {
                let (event_sender, event_receiver) = crate::history_flow::session_stream_channel();
                let session_id = attached.then(bcode_session_models::SessionId::new);
                let mut chat = ActiveChat {
                    app: BmuxApp::new_with_history(session_id, &[], &[]),
                    agents: AgentCatalog::default(),
                    attachment: session_id.map_or(ChatSessionAttachment::Draft, |session_id| {
                        ChatSessionAttachment::Attached { session_id }
                    }),
                    event_sender,
                    event_receiver,
                    event_task: None,
                    opening_session_progress: None,
                    pending_effects: super::super::effects::TuiEffectQueue::default(),
                };
                chat.app.set_daemon_connection(connection);
                chat.app.replace_composer_with("fresh message");
                chat.app.stage_submission();
                assert!(stage_session_message(
                    std::path::Path::new("."),
                    crate::TuiLaunchOptions::default(),
                    &mut chat,
                    bcode_ipc::PromptPlacement::FollowUp
                ));
                assert_eq!(chat.app.daemon_connection(), connection);
                assert_eq!(chat.app.status(), "sending message…");
                assert_eq!(chat.pending_effects.queued_agent_id(), Some("build"));
                assert!(chat.pending_effects.queued_execution_options().is_some());
                drop(chat);
            }
        }
    }

    #[test]
    fn opening_session_defers_message_without_consuming_composer_text() {
        let (event_sender, event_receiver) = crate::history_flow::session_stream_channel();
        let session_id = bcode_session_models::SessionId::new();
        let mut chat = super::super::session_flow::ActiveChat {
            app: super::super::app::BmuxApp::new_with_history(Some(session_id), &[], &[]),
            agents: super::super::session_flow::AgentCatalog::default(),
            attachment: super::super::session_flow::ChatSessionAttachment::Opening {
                session_id,
                anchor_sequence: None,
            },
            event_sender,
            event_receiver,
            event_task: None,
            opening_session_progress: None,
            pending_effects: super::super::effects::TuiEffectQueue::default(),
        };
        chat.app.replace_composer_with("message after migration");
        chat.app.stage_submission();

        assert!(defer_submission_while_session_opens(&mut chat));
        assert_eq!(
            chat.app.take_pending_submission(),
            "message after migration"
        );
        assert!(chat.app.status().contains("still opening"));
    }

    #[test]
    fn active_plugin_status_does_not_block_ordinary_composer_staging() {
        let mut app = super::super::app::BmuxApp::new_with_history(None, &[], &[]);
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
        let status = app.plugin_status().next().map(|status| status.text.clone());
        drop(app);
        assert_eq!(status.as_deref(), Some("Loop active"));
    }
}
