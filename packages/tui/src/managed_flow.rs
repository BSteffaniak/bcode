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
pub fn cycle_permission_mode(chat: &mut ActiveChat, loop_state: &mut ChatLoopState) {
    let current = chat
        .app
        .managed_settings()
        .permission_mode
        .unwrap_or(bcode_harness_models::HarnessPermissionMode::Default);
    start_configure(
        chat,
        loop_state,
        HarnessSessionSetting::PermissionMode {
            mode: managed_controls::next_permission_mode(current),
        },
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
        cycle_permission_mode(&mut chat, &mut loop_state);
        let effects = drained(&mut chat);
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
                mode: bcode_harness_models::HarnessPermissionMode::AcceptEdits
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
            Some(bcode_harness_models::HarnessPermissionMode::AcceptEdits)
        );
        assert!(
            chat.app
                .execution_mode_indicator()
                .unwrap()
                .contains("accept-edits")
        );
        cycle_permission_mode(&mut chat, &mut loop_state);
        assert!(matches!(
            &drained(&mut chat)[..],
            [TuiEffect::ConfigureManagedSession {
                setting: HarnessSessionSetting::PermissionMode {
                    mode: bcode_harness_models::HarnessPermissionMode::Plan
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
