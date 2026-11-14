//! Managed-conversation state transitions for native dialogs, settings and models.
//!
//! This module adapts `SessionView` pending external interactions into the existing native
//! permission dialog and question surface, and turns user decisions into managed effects.
//! Policy (allow-lists, adapters, response identities) lives in `managed_controls`.

use bcode_harness_models::{
    HarnessServiceResponse, HarnessServiceResponseKind, HarnessSessionSetting,
};
use bcode_session_view_models::external::ExternalInteractionKind;

use super::app::ManagedExitRequest;
use super::chat_loop::ChatLoopState;
use super::effects::TuiEffect;
use super::managed_controls::{
    self, ManagedInteractionKey, ManagedResponseOutcome, interaction_key,
};
use super::permission_dialog::{
    PermissionDialogResolution, PermissionDialogState, PermissionOrigin,
};
use super::session_flow::ActiveChat;
use super::slash_commands::SlashCommandOutcome;

fn current_pending(
    chat: &ActiveChat,
    loop_state: &ChatLoopState,
) -> Vec<bcode_session_view_models::external::ExternalInteractionView> {
    let generation = loop_state.managed_generation();
    chat.app
        .pending_external_interactions()
        .iter()
        .filter(|view| Some(view.provenance.generation) == generation)
        .cloned()
        .collect()
}

fn managed_dialog_key(dialog: &PermissionDialogState) -> Option<ManagedInteractionKey> {
    match dialog.origin() {
        PermissionOrigin::Managed {
            generation,
            request_id,
        } => Some((*generation, request_id.clone())),
        PermissionOrigin::Native => None,
    }
}

/// Present the next pending managed interaction through the native dialog or question
/// surface, and withdraw presentations whose interaction is no longer pending.
///
/// Returns whether presentation state changed.
pub fn reconcile_interactions(chat: &mut ActiveChat, loop_state: &mut ChatLoopState) -> bool {
    let Some(owner) = chat.app.managed_execution_session() else {
        return false;
    };
    if resume_ended_native(chat, loop_state) {
        return true;
    }
    let pending = current_pending(chat, loop_state);
    let live = pending
        .iter()
        .map(interaction_key)
        .collect::<std::collections::BTreeSet<_>>();
    let presented_withdrawn = loop_state.managed_interactions.retain_pending(&pending);
    let dialog_withdrawn = loop_state
        .permission_dialog
        .as_ref()
        .and_then(managed_dialog_key)
        .is_some_and(|key| !live.contains(&key));
    if dialog_withdrawn {
        loop_state.permission_dialog = None;
    }
    let surface_withdrawn = loop_state
        .interactive_surface
        .as_ref()
        .and_then(|surface| {
            managed_controls::parse_surface_interaction_id(surface.interaction_id())
        })
        .is_some_and(|key| !live.contains(&key));
    if surface_withdrawn {
        loop_state.interactive_surface = None;
    }
    let mut changed = presented_withdrawn || dialog_withdrawn || surface_withdrawn;
    for key in loop_state.managed_interactions.stale_surface_keys(&live) {
        changed |= loop_state
            .interactive_surface_queue
            .remove(&managed_controls::surface_interaction_id(&key));
    }
    // A presentation the host closed for another reason (session change, cancellation) no
    // longer blocks the next interaction; it stays pending and may be presented again.
    if let Some(presented) = loop_state.managed_interactions.presented().cloned() {
        let surface_id = managed_controls::surface_interaction_id(&presented);
        let shown = loop_state
            .permission_dialog
            .as_ref()
            .and_then(managed_dialog_key)
            .is_some_and(|key| key == presented)
            || loop_state
                .interactive_surface
                .as_ref()
                .is_some_and(|surface| surface.interaction_id() == surface_id)
            || loop_state.interactive_surface_queue.contains(&surface_id);
        if !shown {
            loop_state.managed_interactions.clear_presented();
        }
    }
    if loop_state.permission_dialog.is_some() || loop_state.interactive_surface.is_some() {
        return changed;
    }
    let Some(view) = loop_state
        .managed_interactions
        .next_to_present(&pending)
        .cloned()
    else {
        return changed;
    };
    let key = interaction_key(&view);
    match view.interaction_kind {
        ExternalInteractionKind::Approval => {
            loop_state.permission_dialog = Some(PermissionDialogState::managed(
                managed_controls::approval_permission_view(owner, &view),
                key.0,
                key.1.clone(),
            ));
            chat.app
                .set_status("Native approval requested; approve or deny".to_owned());
        }
        ExternalInteractionKind::Question | ExternalInteractionKind::Assistance => {
            let Some(request) = managed_controls::question_surface_request(&view) else {
                return changed;
            };
            let _queued = loop_state.interactive_surface_queue.enqueue(request, None);
            loop_state.managed_interactions.note_surface(key.clone());
            chat.app.set_status(
                if view.interaction_kind == ExternalInteractionKind::Question {
                    "Native question awaiting your answer"
                } else {
                    "Native assistance requested; reply below"
                }
                .to_owned(),
            );
        }
    }
    loop_state.managed_interactions.present(key);
    true
}

/// When the attached native process has ended, resume it: the conversation is recovered
/// under a new generation without replaying any input. Returns whether recovery started.
fn resume_ended_native(chat: &mut ActiveChat, loop_state: &ChatLoopState) -> bool {
    let ended = chat.app.session_view_snapshot().native_connection_ended;
    if ended.is_none() || ended != loop_state.managed_generation() {
        return false;
    }
    let label = chat
        .app
        .managed_label()
        .unwrap_or("The native agent")
        .to_owned();
    chat.app.request_managed_exit(ManagedExitRequest::Recover);
    chat.app
        .set_status(format!("{label} stopped; resuming the conversation…"));
    true
}

fn dispatch_response(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    key: &ManagedInteractionKey,
    value: bcode_harness_models::HarnessResponseValue,
) {
    if loop_state.managed_interactions.is_in_flight(key) {
        chat.app
            .set_status("A native response for this request is still pending".to_owned());
        return;
    }
    let response_id = loop_state.managed_interactions.response_id(key);
    loop_state.managed_interactions.begin(key);
    chat.start_effect(TuiEffect::RespondManagedInteraction {
        generation: key.0,
        request_id: key.1.clone(),
        response_id,
        value,
    });
    chat.app.set_status("Sending native response…".to_owned());
}

/// Route a permission-dialog decision to its recipient and close the dialog.
pub fn dispatch_permission_resolution(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    resolution: &PermissionDialogResolution,
) {
    let Some(dialog) = loop_state.permission_dialog.take() else {
        return;
    };
    match dialog.origin() {
        PermissionOrigin::Managed {
            generation,
            request_id,
        } => {
            let key = (*generation, request_id.clone());
            dispatch_response(
                chat,
                loop_state,
                &key,
                bcode_harness_models::HarnessResponseValue::Approval {
                    approved: resolution.approved,
                },
            );
        }
        PermissionOrigin::Native => {
            let permission_id = dialog.permission().permission_id.clone();
            let batch_id = dialog
                .permission()
                .batch
                .as_ref()
                .map(|batch| batch.batch_id.clone());
            chat.start_effect(TuiEffect::ResolvePermission {
                permission_id,
                approved: resolution.approved,
                remember: resolution.remember,
                apply_to_batch: resolution.apply_to_batch,
                batch_id,
            });
            chat.app
                .set_status(format!("resolving permission: {}", resolution.label));
        }
    }
}

/// Handle a question-surface resolution for a managed interaction.
/// Returns `false` when the interaction id is not a managed one.
pub fn resolve_surface(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    interaction_id: &str,
    resolution: &bcode_session_models::ToolExchangeResolution,
) -> bool {
    let Some(key) = managed_controls::parse_surface_interaction_id(interaction_id) else {
        return false;
    };
    let view = current_pending(chat, loop_state)
        .into_iter()
        .find(|view| interaction_key(view) == key);
    let value = view
        .as_ref()
        .and_then(|view| managed_controls::question_response_value(view, resolution));
    if let Some(value) = value {
        dispatch_response(chat, loop_state, &key, value);
    } else {
        loop_state.managed_interactions.dismiss(&key);
        chat.app.set_status(if view.is_some() {
            "Native request left unanswered; /permissions shows it again".to_owned()
        } else {
            "Native request is no longer pending".to_owned()
        });
    }
    true
}

/// A message sent while the native agent's question is open answers that question, as in the
/// native tool. Returns whether `text` was used as an answer; the open question's inline box
/// closes because the question is now answered.
pub fn answer_open_question(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    text: &str,
) -> bool {
    if chat.app.managed_execution_session().is_none() || text.trim().is_empty() {
        return false;
    }
    let Some(view) = current_pending(chat, loop_state).into_iter().find(|view| {
        view.interaction_kind == ExternalInteractionKind::Question
            && loop_state.managed_interactions.presented() == Some(&interaction_key(view))
    }) else {
        return false;
    };
    let key = interaction_key(&view);
    if loop_state.managed_interactions.is_in_flight(&key) {
        return false;
    }
    let surface_id = managed_controls::surface_interaction_id(&key);
    if loop_state
        .interactive_surface
        .as_ref()
        .is_some_and(|surface| surface.interaction_id() == surface_id)
    {
        loop_state.interactive_surface = None;
    }
    loop_state.interactive_surface_queue.remove(&surface_id);
    dispatch_response(
        chat,
        loop_state,
        &key,
        bcode_harness_models::HarnessResponseValue::Answer {
            choice_id: None,
            text: text.to_owned(),
        },
    );
    true
}

/// Show the daemon's held input and the outcome of a withdrawal.
pub fn apply_queue_result(
    chat: &mut ActiveChat,
    withdrawal: Option<(String, bool)>,
    result: Result<bcode_ipc::harness::ManagedQueuedInput, bcode_client::ClientError>,
) {
    let label = chat
        .app
        .managed_label()
        .unwrap_or("The native agent")
        .to_owned();
    let Ok(queue) = result else {
        if withdrawal.is_some() {
            chat.app
                .set_status("Could not withdraw that message; it stays queued".into());
        }
        return;
    };
    if let Some((submission_id, edit)) = withdrawal {
        let text = chat
            .app
            .managed_queue()
            .iter()
            .find(|input| input.submission_id == submission_id)
            .map(|input| {
                bcode_session_view::external_projection::split_agent_block(&input.text)
                    .1
                    .to_owned()
            });
        let status = match (queue.withdrawn, edit, text) {
            (true, true, Some(text)) => {
                let draft = chat.app.composer().text().to_owned();
                let combined = if draft.trim().is_empty() {
                    text
                } else {
                    format!("{text}\n\n{draft}")
                };
                chat.app.replace_composer_with(&combined);
                "Withdrawn from the queue; edit it and send again".to_owned()
            }
            (true, _, _) => "Withdrawn from the queue; it will not be sent".to_owned(),
            (false, _, _) => format!("Already sent to {label}; it can no longer be withdrawn"),
        };
        chat.app.set_status(status);
    }
    chat.app.sync_managed_queue(&queue.queued);
}

/// Withdraw the newest queued message, returning its text to the composer when `edit`.
pub fn withdraw_newest_queued(chat: &mut ActiveChat, edit: bool) -> bool {
    if chat.app.managed_execution_session().is_none() {
        return false;
    }
    let Some(newest) = chat.app.managed_queue().last() else {
        return false;
    };
    let submission_id = newest.submission_id.clone();
    chat.start_effect(TuiEffect::WithdrawManagedQueued {
        submission_id,
        edit,
    });
    true
}

/// Apply one managed response outcome without retrying it.
pub fn apply_response_result(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    key: &ManagedInteractionKey,
    response_id: &str,
    result: &Result<HarnessServiceResponse, bcode_client::ClientError>,
) {
    let outcome = managed_controls::classify_response(result);
    loop_state.managed_interactions.finish(key, outcome);
    chat.app.set_status(match (outcome, result) {
        (ManagedResponseOutcome::Acknowledged, _) => "Native response recorded".to_owned(),
        (ManagedResponseOutcome::Refused, Ok(response)) => format!(
            "Native response refused ({}); /permissions to review it again",
            response_kind_label(response)
        ),
        (ManagedResponseOutcome::Refused, Err(error)) => {
            format!("Native response refused: {error}; /permissions to review it again")
        }
        (ManagedResponseOutcome::Uncertain, _) => format!(
            "Native response outcome is uncertain and was not retried; /permissions reuses response {response_id}"
        ),
    });
    let _changed = reconcile_interactions(chat, loop_state);
}

fn response_kind_label(response: &HarnessServiceResponse) -> String {
    match &response.kind {
        HarnessServiceResponseKind::Failure(failure) => format!("{failure:?}").to_lowercase(),
        _ => "unexpected response".to_owned(),
    }
}

fn setting_label(setting: &HarnessSessionSetting) -> String {
    match setting {
        HarnessSessionSetting::Model { model_id } => format!("native model {model_id}"),
        HarnessSessionSetting::PermissionMode { mode } => {
            managed_controls::permission_mode_label(*mode).to_owned()
        }
    }
}

/// Start one managed setting change with a fresh retained mutation identity.
pub fn start_configure(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    setting: HarnessSessionSetting,
) {
    let Some(generation) = loop_state.managed_generation() else {
        chat.app
            .set_status("Native settings require an attached managed conversation".to_owned());
        return;
    };
    if loop_state.managed_configure_in_flight.is_some() {
        chat.app.set_status(
            "A native setting change is still pending; wait for its outcome".to_owned(),
        );
        return;
    }
    let mutation_id = uuid::Uuid::new_v4().to_string();
    loop_state.managed_configure_in_flight = Some(mutation_id.clone());
    chat.app
        .set_status(format!("Requesting {}…", setting_label(&setting)));
    chat.start_effect(TuiEffect::ConfigureManagedSession {
        generation,
        mutation_id,
        setting,
    });
}

/// Cycle the native permission mode: Default → `AcceptEdits` → Plan → Default.
/// Cycle Bcode's agents in a native conversation, as in any Bcode conversation. The native
/// agent runs a read-only Bcode agent in its plan mode and any other agent in its default mode.
pub fn cycle_native_agent(chat: &mut ActiveChat, loop_state: &mut ChatLoopState) {
    if chat.agents.is_empty() {
        chat.replace_effect(TuiEffect::LoadAgentCatalog);
        chat.app.set_status("Agents are still loading".to_owned());
        return;
    }
    let Some(agent) = chat.agents.next_agent(chat.app.current_agent_id()).cloned() else {
        chat.app.set_status("No agents available".to_owned());
        return;
    };
    let mode = if agent.read_only {
        bcode_harness_models::HarnessPermissionMode::Plan
    } else {
        bcode_harness_models::HarnessPermissionMode::Default
    };
    chat.agents
        .apply_agent_to_app(&mut chat.app, agent.id.clone());
    if let Some(session_id) = chat.attached_session_id() {
        // The daemon decides the native agent's permission requests with this agent.
        chat.start_effect(TuiEffect::SetSessionAgent {
            session_id,
            agent_id: agent.id.clone(),
        });
    }
    let label = chat
        .app
        .managed_label()
        .unwrap_or("The native agent")
        .to_owned();
    chat.app.push_ephemeral_system_plain(if agent.read_only {
        format!(
            "Switched to the {} agent: {label} plans without changing files.",
            agent.name
        )
    } else {
        format!("Switched to the {} agent.", agent.name)
    });
    start_configure(
        chat,
        loop_state,
        HarnessSessionSetting::PermissionMode { mode },
    );
}

/// Apply one managed setting outcome. Ambiguous outcomes are reported, never replayed.
pub fn apply_configure_result(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    mutation_id: &str,
    setting: &HarnessSessionSetting,
    result: &Result<HarnessServiceResponse, bcode_client::ClientError>,
) {
    if loop_state.managed_configure_in_flight.as_deref() == Some(mutation_id) {
        loop_state.managed_configure_in_flight = None;
    }
    let label = setting_label(setting);
    let status = match result {
        Ok(response) => match &response.kind {
            HarnessServiceResponseKind::Acknowledged { .. } => {
                chat.app.apply_managed_setting(setting);
                format!("Switched to {label}")
            }
            HarnessServiceResponseKind::Failure(
                bcode_harness_models::HarnessFailure::Unsupported,
            ) => {
                format!("This native harness cannot switch to {label} mid-session")
            }
            HarnessServiceResponseKind::Failure(
                bcode_harness_models::HarnessFailure::Uncertain,
            ) => {
                format!("Switching to {label} has an uncertain outcome; it was not retried")
            }
            _ => format!(
                "Switching to {label} was refused ({})",
                response_kind_label(response)
            ),
        },
        Err(bcode_client::ClientError::Server { code, .. })
            if code == "managed_session_unsupported" =>
        {
            format!("This native harness cannot switch to {label} mid-session")
        }
        Err(error) => format!("Switching to {label} did not complete: {error}; it was not retried"),
    };
    chat.app.set_status(status);
}

/// Apply managed-specific meanings of slash outcomes; returns the outcome when the ordinary
/// handler should apply it.
pub fn intercept_slash_outcome(
    chat: &mut ActiveChat,
    loop_state: &mut ChatLoopState,
    outcome: SlashCommandOutcome,
) -> Option<SlashCommandOutcome> {
    if chat.app.managed_execution_session().is_none() {
        return Some(outcome);
    }
    match outcome {
        SlashCommandOutcome::ManagedConfigure(setting) => {
            start_configure(chat, loop_state, setting);
        }
        SlashCommandOutcome::PickModel => {
            super::chat_loop::request_execution_picker(chat, loop_state);
        }
        SlashCommandOutcome::ReviewPermissions => {
            loop_state.managed_interactions.review_again();
            if !reconcile_interactions(chat, loop_state) {
                chat.app
                    .set_status("No unanswered native requests".to_owned());
            }
        }
        SlashCommandOutcome::NewDraftSession => {
            chat.app
                .request_managed_exit(ManagedExitRequest::Navigate(None));
            chat.app
                .set_status("Leaving the managed conversation to start a new session…".to_owned());
        }
        other => return Some(other),
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_view_models::external::{
        ExternalActionKind, ExternalActionView, ExternalChoiceView, ExternalInteractionView,
        ExternalProvenanceView,
    };

    fn chat(owner: bcode_session_models::SessionId) -> ActiveChat {
        let (event_sender, event_receiver) = super::super::history_flow::session_stream_channel();
        let mut app = super::super::app::BmuxApp::new_with_history(Some(owner), &[], &[]);
        app.set_managed_execution(owner);
        ActiveChat {
            app,
            agents: super::super::session_flow::AgentCatalog::default(),
            attachment: super::super::session_flow::ChatSessionAttachment::Attached {
                session_id: owner,
            },
            event_sender,
            event_receiver,
            event_task: None,
            opening_session_progress: None,
            pending_effects: super::super::effects::TuiEffectQueue::default(),
        }
    }

    fn interaction(kind: ExternalInteractionKind, request_id: &str) -> ExternalInteractionView {
        ExternalInteractionView {
            request_id: request_id.into(),
            provenance: ExternalProvenanceView {
                session_id: "s".into(),
                instance_id: "claude".into(),
                plugin_id: "p".into(),
                native_session_id: "n".into(),
                generation: 1,
            },
            interaction_kind: kind,
            kind: "k".into(),
            prompt: "Allow?".into(),
            scope: String::new(),
            choices: vec![ExternalChoiceView {
                id: "yes".into(),
                label: "Yes".into(),
                description: None,
            }],
            tool_name: Some("Bash".into()),
            input_preview: Some(r#"{"command":"ls"}"#.into()),
            response: ExternalActionView {
                action: ExternalActionKind::Approve,
                enabled: true,
                reason: String::new(),
            },
        }
    }

    fn with_agents(chat: &mut ActiveChat) {
        chat.agents = crate::session_flow::AgentCatalog::from_agents(vec![
            agent("build", false),
            agent("plan", true),
        ]);
        chat.agents.apply_agent_to_app(&mut chat.app, "build");
    }

    fn agent(id: &str, read_only: bool) -> bcode_agent_profile::AgentInfo {
        bcode_agent_profile::AgentInfo {
            id: id.into(),
            name: id.into(),
            description: String::new(),
            badge: None,
            accent: None,
            aliases: Vec::new(),
            is_default: !read_only,
            read_only,
        }
    }

    fn drained(chat: &mut ActiveChat) -> Vec<TuiEffect> {
        chat.pending_effects
            .drain_runtime()
            .0
            .into_iter()
            .map(|(_, effect)| effect)
            .collect()
    }

    #[test]
    fn approval_decision_emits_one_managed_response_and_never_a_bcode_permission() {
        let owner = bcode_session_models::SessionId::new();
        let mut chat = chat(owner);
        let mut loop_state = ChatLoopState::new_access(None, None, None, false);
        loop_state.set_managed_generation_for_test(1);
        chat.app
            .set_pending_external_interactions_for_test(vec![interaction(
                ExternalInteractionKind::Approval,
                "req-1",
            )]);
        assert!(reconcile_interactions(&mut chat, &mut loop_state));
        let dialog = loop_state
            .permission_dialog
            .as_ref()
            .expect("managed dialog");
        assert_eq!(dialog.permission().tool_name, "Bash");
        assert!(!dialog.permission().can_remember);
        dispatch_permission_resolution(
            &mut chat,
            &mut loop_state,
            &PermissionDialogResolution {
                approved: true,
                remember: false,
                apply_to_batch: false,
                label: "approve",
            },
        );
        let effects = drained(&mut chat);
        assert_eq!(effects.len(), 1);
        let TuiEffect::RespondManagedInteraction {
            generation,
            request_id,
            response_id,
            value,
        } = &effects[0]
        else {
            panic!("expected a managed response effect");
        };
        assert_eq!((*generation, request_id.as_str()), (1, "req-1"));
        assert_eq!(
            value,
            &bcode_harness_models::HarnessResponseValue::Approval { approved: true }
        );
        // An uncertain outcome is not retried and the same identity is reused on review.
        apply_response_result(
            &mut chat,
            &mut loop_state,
            &(1, "req-1".into()),
            response_id,
            &Err(bcode_client::ClientError::RequestTimeout {
                timeout: std::time::Duration::from_secs(1),
            }),
        );
        assert!(loop_state.permission_dialog.is_none());
        assert!(drained(&mut chat).is_empty());
        let _ = intercept_slash_outcome(
            &mut chat,
            &mut loop_state,
            SlashCommandOutcome::ReviewPermissions,
        );
        dispatch_permission_resolution(
            &mut chat,
            &mut loop_state,
            &PermissionDialogResolution {
                approved: false,
                remember: false,
                apply_to_batch: false,
                label: "deny",
            },
        );
        let retried = drained(&mut chat);
        let TuiEffect::RespondManagedInteraction {
            response_id: reused,
            ..
        } = &retried[0]
        else {
            panic!("expected managed response");
        };
        assert_eq!(reused, response_id);
        drop(chat);
    }

    #[test]
    fn the_native_agent_hears_of_each_agent_change_once() {
        let owner = bcode_session_models::SessionId::new();
        let mut chat = chat(owner);
        chat.agents = crate::session_flow::AgentCatalog::from_agents(vec![
            agent("build", false),
            agent("plan", true),
        ]);
        chat.agents.apply_agent_to_app(&mut chat.app, "build");
        assert_eq!(
            chat.app.take_native_agent_announcement().as_deref(),
            Some("build")
        );
        assert_eq!(chat.app.take_native_agent_announcement(), None);
        chat.agents.apply_agent_to_app(&mut chat.app, "plan");
        assert_eq!(
            chat.app.take_native_agent_announcement().as_deref(),
            Some("plan")
        );
        assert_eq!(chat.app.take_native_agent_announcement(), None);
        // A message that was not delivered announces the agent again next time.
        chat.app.forget_native_agent_announcement();
        assert_eq!(
            chat.app.take_native_agent_announcement().as_deref(),
            Some("plan")
        );
        assert_eq!(chat.agents.name_of("plan"), "plan");
        drop(chat);
    }

    #[test]
    fn queued_rows_follow_the_daemon_and_withdrawing_returns_text_to_edit() {
        let owner = bcode_session_models::SessionId::new();
        let mut chat = chat(owner);
        let held =
            |id: &str, text: &str| bcode_session_models::external_harness::QueuedNativeInput {
                submission_id: id.into(),
                attempt_id: "attempt".into(),
                text: text.into(),
                queued_at_ms: 1,
            };
        // After a restart, held input reappears as queued rows; a re-queued identity keeps its row.
        apply_queue_result(
            &mut chat,
            None,
            Ok(bcode_ipc::harness::ManagedQueuedInput {
                queued: vec![held("first.retry", "first"), held("second", "second")],
                withdrawn: false,
            }),
        );
        let rows: Vec<_> = chat
            .app
            .pending_submissions()
            .iter()
            .map(|row| (row.id().0.clone(), row.text().to_owned()))
            .collect();
        assert_eq!(
            rows,
            [
                ("first".to_owned(), "first".to_owned()),
                ("second".to_owned(), "second".to_owned())
            ]
        );
        assert!(withdraw_newest_queued(&mut chat, true));
        assert!(matches!(
            &drained(&mut chat)[..],
            [TuiEffect::WithdrawManagedQueued { submission_id, edit: true }] if submission_id == "second"
        ));
        chat.app.replace_composer_with("draft");
        apply_queue_result(
            &mut chat,
            Some(("second".into(), true)),
            Ok(bcode_ipc::harness::ManagedQueuedInput {
                queued: vec![held("first.retry", "first")],
                withdrawn: true,
            }),
        );
        assert_eq!(chat.app.composer().text(), "second\n\ndraft");
        assert_eq!(chat.app.pending_submissions().len(), 1);
        // Too late: it was already being sent, so nothing returns to the composer.
        apply_queue_result(
            &mut chat,
            Some(("first.retry".into(), false)),
            Ok(bcode_ipc::harness::ManagedQueuedInput {
                queued: Vec::new(),
                withdrawn: false,
            }),
        );
        assert!(
            chat.app.status().contains("Already sent"),
            "{}",
            chat.app.status()
        );
        assert_eq!(chat.app.pending_submissions().len(), 0);
        drop(chat);
    }

    #[test]
    fn a_message_sent_while_a_question_is_open_answers_it() {
        let owner = bcode_session_models::SessionId::new();
        let mut chat = chat(owner);
        let mut loop_state = ChatLoopState::new_access(None, None, None, false);
        loop_state.set_managed_generation_for_test(1);
        // Nothing is open: the message is ordinary input.
        assert!(!answer_open_question(&mut chat, &mut loop_state, "hello"));
        chat.app
            .set_pending_external_interactions_for_test(vec![interaction(
                ExternalInteractionKind::Question,
                "q",
            )]);
        assert!(reconcile_interactions(&mut chat, &mut loop_state));
        assert!(!answer_open_question(&mut chat, &mut loop_state, "   "));
        assert!(answer_open_question(
            &mut chat,
            &mut loop_state,
            "what model are you?"
        ));
        assert!(loop_state.next_surface_open_request().is_none());
        let effects = drained(&mut chat);
        assert!(matches!(
            &effects[..],
            [TuiEffect::RespondManagedInteraction {
                value: bcode_harness_models::HarnessResponseValue::Answer { choice_id: None, text },
                ..
            }] if text == "what model are you?"
        ));
        drop(chat);
    }

    #[test]
    fn questions_open_the_question_surface_and_dismissal_sends_nothing() {
        let owner = bcode_session_models::SessionId::new();
        let mut chat = chat(owner);
        let mut loop_state = ChatLoopState::new_access(None, None, None, false);
        loop_state.set_managed_generation_for_test(1);
        chat.app
            .set_pending_external_interactions_for_test(vec![interaction(
                ExternalInteractionKind::Question,
                "q",
            )]);
        assert!(reconcile_interactions(&mut chat, &mut loop_state));
        let request = loop_state
            .next_surface_open_request()
            .expect("queued surface");
        assert_eq!(
            request.surface_kind_for_test(),
            managed_controls::QUESTION_INLINE_SURFACE
        );
        let id = request.interaction_id().to_owned();
        assert!(resolve_surface(
            &mut chat,
            &mut loop_state,
            &id,
            &super::super::interactive_surface::InteractiveSurfaceState::dismissed_resolution(),
        ));
        assert!(drained(&mut chat).is_empty());
        assert!(resolve_surface(
            &mut chat,
            &mut loop_state,
            &id,
            &bcode_session_models::ToolExchangeResolution::Responded {
                payload: serde_json::json!({"status":"answered","questions":[{"question_index":0,"selected":["yes"]}]}),
            },
        ));
        let effects = drained(&mut chat);
        assert!(matches!(
            &effects[..],
            [TuiEffect::RespondManagedInteraction {
                value: bcode_harness_models::HarnessResponseValue::Answer { choice_id: Some(choice), .. },
                ..
            }] if choice == "yes"
        ));
        assert!(!resolve_surface(
            &mut chat,
            &mut loop_state,
            "ordinary",
            &bcode_session_models::ToolExchangeResolution::Cancelled,
        ));
        // Interactions answered elsewhere withdraw queued presentation.
        chat.app
            .set_pending_external_interactions_for_test(Vec::new());
        let _ = reconcile_interactions(&mut chat, &mut loop_state);
        assert!(loop_state.next_surface_open_request().is_none());
        drop(chat);
    }

    #[test]
    fn managed_outcomes_route_to_native_settings_models_and_navigation() {
        let owner = bcode_session_models::SessionId::new();
        let mut chat = chat(owner);
        let mut loop_state = ChatLoopState::new_access(None, None, None, false);
        loop_state.set_managed_generation_for_test(7);
        assert!(
            intercept_slash_outcome(&mut chat, &mut loop_state, SlashCommandOutcome::PickModel)
                .is_none()
        );
        assert!(drained(&mut chat).is_empty());
        // The switcher opens over the managed conversation; nothing is handed off yet.
        assert!(loop_state.has_model_picker());
        assert!(loop_state.execution_picker.is_none());
        loop_state.model_switcher = None;
        // Tab cycles Bcode's own agents; the read-only one runs as the native plan mode.
        with_agents(&mut chat);
        cycle_native_agent(&mut chat, &mut loop_state);
        assert_eq!(chat.app.current_agent_id(), "plan");
        let mut effects = drained(&mut chat);
        // The agent is saved with the session, so the daemon decides native approvals with it.
        let saved = effects
            .iter()
            .position(|effect| matches!(effect, TuiEffect::SetSessionAgent { agent_id, .. } if agent_id == "plan"))
            .expect("agent saved");
        effects.remove(saved);
        let [
            TuiEffect::ConfigureManagedSession {
                generation: 7,
                mutation_id,
                setting,
            },
        ] = &effects[..]
        else {
            panic!("expected configure");
        };
        assert_eq!(
            setting,
            &HarnessSessionSetting::PermissionMode {
                mode: bcode_harness_models::HarnessPermissionMode::Plan
            }
        );
        // A second change waits for the first outcome instead of racing it.
        start_configure(
            &mut chat,
            &mut loop_state,
            HarnessSessionSetting::Model {
                model_id: "opus".into(),
            },
        );
        assert!(drained(&mut chat).is_empty());
        apply_configure_result(
            &mut chat,
            &mut loop_state,
            mutation_id,
            setting,
            &Ok(HarnessServiceResponse {
                schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
                request_id: mutation_id.clone(),
                operation: bcode_harness_models::HarnessOperation::Configure,
                kind: HarnessServiceResponseKind::Acknowledged {
                    mutation_id: mutation_id.clone(),
                },
            }),
        );
        assert_eq!(
            chat.app.managed_settings().permission_mode,
            Some(bcode_harness_models::HarnessPermissionMode::Plan)
        );
        cycle_native_agent(&mut chat, &mut loop_state);
        assert_eq!(chat.app.current_agent_id(), "build");
        assert!(matches!(
            &drained(&mut chat)
                .into_iter()
                .filter(|effect| !matches!(effect, TuiEffect::SetSessionAgent { .. }))
                .collect::<Vec<_>>()[..],
            [TuiEffect::ConfigureManagedSession {
                setting: HarnessSessionSetting::PermissionMode {
                    mode: bcode_harness_models::HarnessPermissionMode::Default
                },
                ..
            }]
        ));
        assert!(
            intercept_slash_outcome(
                &mut chat,
                &mut loop_state,
                SlashCommandOutcome::NewDraftSession
            )
            .is_none()
        );
        assert_eq!(
            chat.app.managed_exit_request(),
            Some(ManagedExitRequest::Navigate(None))
        );
        assert!(
            intercept_slash_outcome(
                &mut chat,
                &mut loop_state,
                SlashCommandOutcome::OpenAppearancePicker(
                    crate::appearance_picker::AppearanceAxis::Palette
                )
            )
            .is_some()
        );
        drop(chat);
    }
}
