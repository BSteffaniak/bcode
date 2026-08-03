//! Composer submission flow for the TUI.

use super::app::DaemonConnectionState;
use super::effects::{SubmitMessageRequest, TuiEffect};
use super::session_flow::ActiveChat;
use super::slash_registry;

fn defer_submission_while_session_opens(chat: &mut ActiveChat) -> bool {
    if chat.opening_session_id.is_none() {
        return false;
    }
    chat.app
        .set_status("Session is still opening; message kept in composer".to_owned());
    true
}

pub enum RootSubmission {
    MessageStaged(bool),
    SlashCommand(String),
}

pub fn stage_root_submission(
    launch_working_directory: &std::path::Path,
    chat: &mut ActiveChat,
    placement: bcode_ipc::PromptPlacement,
) -> RootSubmission {
    if chat.opening_session_id.is_some() {
        chat.app
            .set_status("Session is still opening; message kept in composer".to_owned());
        return RootSubmission::MessageStaged(false);
    }
    let message = chat.app.take_pending_submission();
    if slash_registry::slash_command_name(&message).is_some() {
        chat.app.clear_pending_submission(&message);
        return RootSubmission::SlashCommand(message);
    }
    chat.app.restore_pending_submission(&message);
    chat.app.stage_submission();
    RootSubmission::MessageStaged(stage_session_message(
        launch_working_directory,
        chat,
        placement,
    ))
}

pub fn stage_session_message(
    launch_working_directory: &std::path::Path,
    chat: &mut ActiveChat,
    placement: bcode_ipc::PromptPlacement,
) -> bool {
    if defer_submission_while_session_opens(chat) {
        return false;
    }
    let session_id = chat.app.session_id();
    let message = chat.app.take_pending_submission();
    if message.trim().is_empty() {
        chat.app.clear_pending_submission(&message);
        return false;
    }
    if slash_registry::slash_command_name(&message).is_some() {
        chat.app.clear_pending_submission(&message);
        chat.app
            .set_status("slash command pending root navigation migration".to_owned());
        return false;
    }
    let agent_id = if session_id.is_some() {
        chat.app.pending_agent_id().map(ToOwned::to_owned)
    } else {
        let current = chat.app.current_agent_id().to_owned();
        (current != "build").then_some(current)
    };
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
    chat.start_effect(TuiEffect::SubmitMessage {
        request: Box::new(SubmitMessageRequest {
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
            event_sender: chat.event_sender.clone(),
        }),
    });
    chat.app
        .set_daemon_connection(DaemonConnectionState::Starting);
    chat.app.set_status("starting daemon…".to_owned());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_session_defers_message_without_consuming_composer_text() {
        let (event_sender, event_receiver) = crate::history_flow::session_stream_channel();
        let session_id = bcode_session_models::SessionId::new();
        let mut chat = super::super::session_flow::ActiveChat {
            app: super::super::app::BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            agents: super::super::session_flow::AgentCatalog::default(),
            session_id: None,
            event_sender,
            event_receiver,
            event_task: None,
            opening_session_id: Some(session_id),
            opening_session_progress: None,
            opening_session_anchor_sequence: None,
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
}
