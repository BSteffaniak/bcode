//! The model switcher inside a conversation, drawn over the transcript.
//!
//! Opening it is instant; discovery then reports each source as it is checked. A choice that
//! only changes the model of this conversation's Bcode agent applies in place, like `/model
//! <id>`, with its reasoning effort applied once the model change lands. A choice that needs a
//! native tool or a new conversation, and native tool setup, hand the terminal to the shell,
//! which carries them out and returns to the conversation.

use crate::chat_loop::ChatLoopState;
use crate::effects::TuiEffect;
use crate::execution_flow::{ExecutionPickerRequest, PendingSwitch};
use crate::model_switcher::inventory::{Access, CurrentChoice, SourceUpdate};
use crate::model_switcher::{Choice, ModelSwitcher, Notice, Outcome};
use crate::root_program::BcodeRuntimeMessage;
use crate::session_flow::ActiveChat;
use bcode_model::execution::ExecutionRoute;
use bcode_session_models::SessionId;
use bmux_tui::event::Event;

/// The open switcher and the discovery run that feeds it.
pub struct ChatModelSwitcher {
    /// Fences messages from discovery runs of a switcher that has since closed.
    pub generation: u64,
    /// Switcher state.
    pub switcher: ModelSwitcher,
}

/// Background work the root runtime runs for the switcher.
#[derive(Debug)]
pub enum Work {
    /// Discover every source.
    Discover,
    /// Approve credentials, then rediscover.
    Approve(Box<bcode_provider_auth_models::external::ExternalAuthCandidate>),
    /// Persist recent and favorite choices.
    Persist(bcode_config::model_choices::ModelChoices),
    /// Advance the loading animation.
    Tick,
}

/// Results of switcher work, fenced by generation.
#[derive(Debug)]
pub enum Message {
    /// One source reported.
    Source(Box<SourceUpdate>),
    /// Every source has reported.
    Discovered,
    /// Approving credentials finished.
    Approved(Result<(), String>),
    /// Time to redraw the loading animation.
    Tick,
}

/// Open the switcher over the conversation.
pub fn open(chat: &ActiveChat, state: &mut ChatLoopState) {
    let current = CurrentChoice {
        route: chat
            .app
            .managed_execution_session()
            .is_none()
            .then(|| chat.app.selected_provider_plugin_id())
            .flatten()
            .map(|provider| ExecutionRoute::ModelProvider {
                provider_plugin_id: provider.to_owned(),
            }),
        model_id: chat.app.selected_model_id().map(ToOwned::to_owned),
    };
    let choices =
        bcode_config::model_choices::load_from(&bcode_config::model_choices::default_path())
            .unwrap_or_default();
    let keymap = crate::keymap::BmuxKeyMap::from_registry(chat.app.shared_effective_keymap());
    let mut switcher = ModelSwitcher::new(current, choices, keymap);
    switcher.set_current_effort(chat.app.reasoning_effort().map(ToOwned::to_owned));
    state.model_switcher_generation = state.model_switcher_generation.wrapping_add(1);
    state.model_switcher = Some(ChatModelSwitcher {
        generation: state.model_switcher_generation,
        switcher,
    });
    state.model_switcher_work.push(Work::Discover);
    state.model_switcher_work.push(Work::Tick);
}

/// Route terminal input to the open switcher.
pub fn event(chat: &mut ActiveChat, state: &mut ChatLoopState, event: &Event) {
    let theme = crate::model_switcher::SwitcherTheme::from_presented(&chat.app.presented_theme());
    let Some(open) = state.model_switcher.as_mut() else {
        return;
    };
    let outcome = match event {
        Event::Key(stroke) => open.switcher.key(*stroke),
        Event::Paste(text) => open.switcher.paste(text),
        Event::Mouse(_) => open.switcher.mouse(event, &theme),
        Event::Resize(_) | Event::Focus(_) | Event::Tick | Event::User(_) => Outcome::Ignored,
    };
    act(chat, state, outcome);
}

fn act(chat: &mut ActiveChat, state: &mut ChatLoopState, outcome: Outcome) {
    match outcome {
        Outcome::Ignored | Outcome::Redraw => {}
        Outcome::Close => state.model_switcher = None,
        Outcome::SaveChoices(choices) => state.model_switcher_work.push(Work::Persist(choices)),
        Outcome::Connect(candidate) => {
            if let Some(open) = state.model_switcher.as_mut() {
                open.switcher.notify(Notice {
                    text: format!("Connecting {}…", candidate.application),
                    error: false,
                });
            }
            state.model_switcher_work.push(Work::Approve(candidate));
        }
        Outcome::Setup => hand_off(chat, state, PendingSwitch::Setup),
        Outcome::Choose(choice) => choose(chat, state, choice),
    }
}

fn choose(chat: &mut ActiveChat, state: &mut ChatLoopState, choice: Box<Choice>) {
    if let Some(open) = state.model_switcher.as_mut() {
        let choices = open.switcher.record_choice(&choice.entry.key);
        state.model_switcher_work.push(Work::Persist(choices));
    }
    let model = &choice.entry.model;
    let in_place = chat.app.managed_execution_session().is_none();
    match (&model.route, in_place, chat.attached_session_id()) {
        (ExecutionRoute::ModelProvider { provider_plugin_id }, true, Some(session_id)) => {
            let external_auth_binding = match &choice.entry.access {
                Access::Connection { binding_id, .. } => Some(binding_id.clone()),
                Access::Direct | Access::Connect(_) | Access::Setup { .. } => None,
            };
            chat.start_effect(TuiEffect::SetSessionModel {
                session_id,
                provider_plugin_id: Some(provider_plugin_id.clone()),
                model_id: model.model.model_id.clone(),
                external_auth_binding,
            });
            state.pending_switch_effort = choice
                .effort
                .clone()
                .filter(|effort| Some(effort.as_str()) != chat.app.reasoning_effort())
                .map(|effort| (session_id, effort));
            chat.app
                .set_status(format!("Switching to {}…", choice.entry.title()));
            state.model_switcher = None;
        }
        (ExecutionRoute::ModelProvider { provider_plugin_id }, true, None) => {
            chat.app.apply_local_model_selection(
                Some(provider_plugin_id.clone()),
                &model.model.model_id,
            );
            state.model_switcher = None;
        }
        _ => hand_off(chat, state, PendingSwitch::Apply(choice)),
    }
}

/// Apply the effort chosen with a model once that model change has landed.
pub fn model_set(
    chat: &mut ActiveChat,
    state: &mut ChatLoopState,
    session_id: SessionId,
    ok: bool,
) {
    let Some((pending, effort)) = state.pending_switch_effort.take() else {
        return;
    };
    if !ok || pending != session_id {
        return;
    }
    chat.start_effect(TuiEffect::SetSessionReasoning {
        session_id,
        effort: Some(effort.clone()),
        summary: None,
        effort_generation: None,
        status: format!("Reasoning effort set to {effort}"),
    });
}

/// Give the terminal to the shell to carry out `pending`, then return to this conversation.
fn hand_off(chat: &ActiveChat, state: &mut ChatLoopState, pending: PendingSwitch) {
    let Ok(working_directory) = chat
        .app
        .working_directory()
        .map(std::path::Path::to_path_buf)
        .map_or_else(std::env::current_dir, Ok)
    else {
        if let Some(open) = state.model_switcher.as_mut() {
            open.switcher.notify(Notice {
                text: "The workspace is unavailable, so a new conversation cannot start here."
                    .into(),
                error: true,
            });
        }
        return;
    };
    let (current, current_effort) = state.model_switcher.take().map_or_else(
        || (CurrentChoice::default(), None),
        |open| {
            (
                open.switcher.current().clone(),
                chat.app.reasoning_effort().map(ToOwned::to_owned),
            )
        },
    );
    state.execution_picker = Some(ExecutionPickerRequest {
        session_id: chat.attached_session_id(),
        draft: chat.app.composer().text().to_owned(),
        working_directory,
        current,
        current_effort,
        pending: Some(pending),
    });
}

/// Apply a message from switcher work.
pub fn message(state: &mut ChatLoopState, generation: u64, message: Message) {
    let Some(open) = state
        .model_switcher
        .as_mut()
        .filter(|open| open.generation == generation)
    else {
        return;
    };
    match message {
        Message::Source(update) => open.switcher.apply(*update),
        Message::Discovered => {}
        Message::Approved(result) => {
            open.switcher.notify(match result {
                Ok(()) => Notice {
                    text: "Connected. Its models are listed below.".into(),
                    error: false,
                },
                Err(_) => Notice {
                    text: "Could not connect those credentials. Nothing was changed.".into(),
                    error: true,
                },
            });
            state.model_switcher_work.push(Work::Discover);
            state.model_switcher_work.push(Work::Tick);
        }
        Message::Tick => {
            if open.switcher.loading() {
                state.model_switcher_work.push(Work::Tick);
            }
        }
    }
}

/// Runtime commands for pending switcher work.
pub fn commands(
    state: &mut ChatLoopState,
    handle: &bmux_tui_runtime::RuntimeHandle<BcodeRuntimeMessage>,
) -> Vec<bmux_tui_runtime::Command<BcodeRuntimeMessage>> {
    let generation = state.model_switcher_generation;
    let wrap = move |message| {
        Some(BcodeRuntimeMessage::ModelSwitcher {
            generation,
            message,
        })
    };
    std::mem::take(&mut state.model_switcher_work)
        .into_iter()
        .map(|work| match work {
            Work::Discover => {
                let handle = handle.clone();
                bmux_tui_runtime::Command::replace(
                    bmux_tui_runtime::CommandKey::new("bcode.model_switcher.discovery"),
                    async move {
                        let (updates, mut received) = tokio::sync::mpsc::channel(16);
                        let discover =
                            tokio::spawn(crate::model_switcher::discovery::discover(updates));
                        while let Some(update) = received.recv().await {
                            let message = BcodeRuntimeMessage::ModelSwitcher {
                                generation,
                                message: Message::Source(Box::new(update)),
                            };
                            if handle.send(message).await.is_err() {
                                discover.abort();
                                return None;
                            }
                        }
                        wrap(Message::Discovered)
                    },
                )
            }
            Work::Approve(candidate) => bmux_tui_runtime::Command::replace(
                bmux_tui_runtime::CommandKey::new("bcode.model_switcher.approve"),
                async move {
                    use bcode_provider_auth_models::external::ExternalAuthApplication as _;
                    let result = bcode_client::BcodeClient::default_endpoint()
                        .approve_external_auth(*candidate)
                        .await
                        .map(|_| ())
                        .map_err(|error| error.to_string());
                    wrap(Message::Approved(result))
                },
            ),
            Work::Persist(choices) => bmux_tui_runtime::Command::queue_latest(
                bmux_tui_runtime::CommandKey::new("bcode.model_switcher.persist"),
                async move {
                    let _ = tokio::task::spawn_blocking(move || {
                        bcode_config::model_choices::update_at(
                            &bcode_config::model_choices::default_path(),
                            |stored| *stored = choices,
                        )
                    })
                    .await;
                    None
                },
            ),
            Work::Tick => bmux_tui_runtime::Command::replace(
                bmux_tui_runtime::CommandKey::new("bcode.model_switcher.tick"),
                async move {
                    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                    wrap(Message::Tick)
                },
            ),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_switcher::inventory::{Entry, Readiness, Source, SourceKey, SourceState};
    use bcode_model::execution::{
        ExecutionBillingRoute, ExecutionModel, ExecutionModelProvenance, ExecutionModelReadiness,
    };

    fn chat(session_id: SessionId) -> ActiveChat {
        let (event_sender, event_receiver) = crate::history_flow::session_stream_channel();
        let mut app = crate::app::BmuxApp::new_with_history(Some(session_id), &[], &[]);
        let keymap =
            crate::keymap::BmuxKeyMap::from_config(&bcode_config::BcodeConfig::default().tui);
        app.set_effective_keymap(std::sync::Arc::new(keymap.registry().clone()));
        ActiveChat {
            app,
            agents: crate::session_flow::AgentCatalog::default(),
            attachment: crate::session_flow::ChatSessionAttachment::Attached { session_id },
            event_sender,
            event_receiver,
            event_task: None,
            opening_session_progress: None,
            pending_effects: crate::effects::TuiEffectQueue::default(),
        }
    }

    fn model(route: ExecutionRoute) -> ExecutionModel {
        ExecutionModel {
            model: bcode_model::ModelInfo {
                model_id: "model".into(),
                display_name: "Model".into(),
                is_default: false,
                context_window: None,
                max_output_tokens: None,
                max_image_input_base64_bytes: None,
                capabilities: std::collections::BTreeSet::new(),
                feature_support: bcode_model::ModelFeatureSupport::default(),
                reasoning: Some(bcode_model::ModelReasoningInfo {
                    effort_values: vec!["low".into(), "high".into()],
                    default_effort: Some("low".into()),
                    ..bcode_model::ModelReasoningInfo::default()
                }),
                cache: bcode_model::ModelCacheInfo::default(),
                metadata_source: None,
                pricing: None,
                api_surface: None,
                visibility: bcode_model::ModelVisibility::Visible,
            },
            native_model_id: "model".into(),
            route,
            account_reference: None,
            billing_route: ExecutionBillingRoute::ProviderApi,
            confirmed_model_id: None,
            provenance: ExecutionModelProvenance::ProviderCatalog,
            readiness: ExecutionModelReadiness::Ready,
        }
    }

    fn open_with(
        chat: &mut ActiveChat,
        key: SourceKey,
        model: ExecutionModel,
        access: Access,
    ) -> ChatLoopState {
        let mut state = ChatLoopState::new_access(None, None, None, false);
        crate::chat_loop::request_execution_picker(chat, &mut state);
        assert!(state.has_model_picker());
        assert!(matches!(
            state.model_switcher_work.as_slice(),
            [Work::Discover, Work::Tick]
        ));
        state.model_switcher_work.clear();
        let generation = state.model_switcher_generation;
        message(
            &mut state,
            generation,
            Message::Source(Box::new(SourceUpdate {
                source: Source {
                    key: key.clone(),
                    title: "Source".into(),
                    detail: String::new(),
                    state: SourceState::Ready,
                },
                entries: vec![Entry::new(key, model, access, Readiness::Confirmed)],
            })),
        );
        state
    }

    fn press(chat: &mut ActiveChat, state: &mut ChatLoopState, code: bmux_keyboard::KeyCode) {
        event(
            chat,
            state,
            &Event::Key(bmux_keyboard::KeyStroke::simple(code)),
        );
    }

    #[test]
    fn provider_choices_apply_in_place_with_their_connection_then_effort() {
        let session_id = SessionId::new();
        let mut chat = chat(session_id);
        let route = ExecutionRoute::ModelProvider {
            provider_plugin_id: "openai".into(),
        };
        let mut state = open_with(
            &mut chat,
            SourceKey::Connection {
                binding_id: "binding".into(),
            },
            model(route),
            Access::Connection {
                binding_id: "binding".into(),
                application: "Codex".into(),
                target: bcode_provider_auth_models::external::ExternalAuthTarget {
                    plugin_id: "openai".into(),
                    provider_id: "openai".into(),
                    method_id: "chatgpt".into(),
                    source_id: "codex".into(),
                },
            },
        );
        press(&mut chat, &mut state, bmux_keyboard::KeyCode::Right);
        press(&mut chat, &mut state, bmux_keyboard::KeyCode::Enter);
        assert!(!state.has_model_picker());
        assert!(state.execution_picker.is_none());
        let effects: Vec<_> = chat
            .pending_effects
            .drain_runtime()
            .0
            .into_iter()
            .map(|(_, effect)| effect)
            .collect();
        assert!(matches!(
            effects.as_slice(),
            [TuiEffect::SetSessionModel { external_auth_binding: Some(binding), .. }]
                if binding == "binding"
        ));
        assert!(matches!(
            state.model_switcher_work.as_slice(),
            [Work::Persist(_)]
        ));

        // The effort follows once the model change lands, and only then.
        model_set(&mut chat, &mut state, session_id, true);
        let effects: Vec<_> = chat
            .pending_effects
            .drain_runtime()
            .0
            .into_iter()
            .map(|(_, effect)| effect)
            .collect();
        assert!(matches!(
            effects.as_slice(),
            [TuiEffect::SetSessionReasoning { effort: Some(effort), .. }] if effort == "high"
        ));
        drop(chat);
    }

    #[test]
    fn native_choices_and_setup_hand_off_to_the_shell() {
        let session_id = SessionId::new();
        let mut chat = chat(session_id);
        let mut state = open_with(
            &mut chat,
            SourceKey::Native {
                instance_id: "claude".into(),
            },
            model(ExecutionRoute::ManagedHarness {
                instance_id: "claude".into(),
                plugin_id: "bcode.claude-code".into(),
            }),
            Access::Direct,
        );
        press(&mut chat, &mut state, bmux_keyboard::KeyCode::Enter);
        assert!(!state.has_model_picker());
        let request = state.execution_picker.take().expect("handed off");
        assert_eq!(request.session_id, Some(session_id));
        assert!(matches!(
            request.pending,
            Some(PendingSwitch::Apply(choice)) if choice.entry.model.model.model_id == "model"
        ));
        drop(state);

        let mut state = open_with(
            &mut chat,
            SourceKey::Native {
                instance_id: "claude".into(),
            },
            model(ExecutionRoute::ManagedHarness {
                instance_id: "claude".into(),
                plugin_id: "bcode.claude-code".into(),
            }),
            Access::Direct,
        );
        press(&mut chat, &mut state, bmux_keyboard::KeyCode::F(2));
        assert!(matches!(
            state
                .execution_picker
                .take()
                .and_then(|request| request.pending),
            Some(PendingSwitch::Setup)
        ));
        drop(chat);
    }

    #[test]
    fn stale_discovery_reports_are_ignored() {
        let mut chat = chat(SessionId::new());
        let mut state = open_with(
            &mut chat,
            SourceKey::Provider {
                plugin_id: "openai".into(),
            },
            model(ExecutionRoute::ModelProvider {
                provider_plugin_id: "openai".into(),
            }),
            Access::Direct,
        );
        let previous_generation = state.model_switcher_generation.wrapping_sub(1);
        message(&mut state, previous_generation, Message::Approved(Ok(())));
        assert!(state.model_switcher_work.is_empty());
        drop(chat);
    }
}
