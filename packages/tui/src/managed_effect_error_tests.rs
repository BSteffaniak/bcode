//! Error-path and input-policy coverage: no native launch, submission or reconnection is executed.
use super::*;
use crate::{app::BmuxApp, input, keymap};

#[test]
fn managed_exit_clears_draft_then_exits_without_requesting_interruption() {
    let owner = SessionId::new();
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.set_managed_execution(owner);
    app.paste_composer_text("unsent managed draft");
    let cleared = input::handle_chat_action(&mut app, Some(keymap::BmuxAction::AppExit)).unwrap();
    assert_eq!(cleared.request, input::KeyRequest::None);
    assert!(app.composer().is_empty());
    assert!(!app.should_exit());
    assert_eq!(app.managed_execution_session(), Some(owner));

    let exited = input::handle_chat_action(&mut app, Some(keymap::BmuxAction::AppExit)).unwrap();
    assert_eq!(exited.request, input::KeyRequest::None);
    assert!(app.should_exit());
    assert_eq!(app.managed_execution_session(), Some(owner));
}

#[test]
fn managed_interrupt_is_explicit_and_does_not_exit_or_resubmit_draft() {
    let owner = SessionId::new();
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.set_managed_execution(owner);
    app.paste_composer_text("unsent managed draft");
    let interrupted =
        input::handle_chat_action(&mut app, Some(keymap::BmuxAction::AppInterrupt)).unwrap();
    assert_eq!(interrupted.request, input::KeyRequest::Interrupt);
    assert!(!app.should_exit());
    assert_eq!(app.composer().text(), "unsent managed draft");
    assert_eq!(app.managed_execution_session(), Some(owner));
}

fn rejected() -> ClientError {
    ClientError::Server {
        code: "managed_session_stale_owner".into(),
        message: "Reattach the exact managed session; no native action performed".into(),
    }
}

#[test]
fn background_history_rejection_preserves_exact_request_and_is_not_daemon_loss() {
    let session_id = SessionId::new();
    let cursor = SessionHistoryCursor { sequence: 360 };
    for older in [true, false] {
        let effect = if older {
            TuiEffect::LoadOlderHistory {
                session_id,
                cursor,
                generation: 7,
                width_columns: 80,
            }
        } else {
            TuiEffect::LoadNewerHistory {
                session_id,
                cursor,
                generation: 7,
                width_columns: 80,
            }
        };
        let result = effect.application_access_failed(rejected());
        assert!(matches!(
            result.daemon_observation(),
            DaemonObservation::Failed(_)
        ));
        match result {
            TuiEffectResult::OlderHistoryLoaded {
                session_id: actual,
                cursor: boundary,
                generation,
                result,
            } if older => {
                assert_eq!(actual, session_id);
                assert_eq!(boundary, cursor);
                assert_eq!(generation, 7);
                assert!(
                    matches!(result, Err(ClientError::Server { code, .. }) if code == "managed_session_stale_owner")
                );
            }
            TuiEffectResult::NewerHistoryLoaded {
                session_id: actual,
                cursor: boundary,
                generation,
                result,
            } if !older => {
                assert_eq!(actual, session_id);
                assert_eq!(boundary, cursor);
                assert_eq!(generation, 7);
                assert!(
                    matches!(result, Err(ClientError::Server { code, .. }) if code == "managed_session_stale_owner")
                );
            }
            _ => panic!("history failure lost typed correlation"),
        }
    }
}

#[test]
fn managed_unavailable_background_operations_return_their_own_error_without_panic() {
    assert!(matches!(
        TuiEffect::ListPermissions.application_access_failed(rejected()),
        TuiEffectResult::PermissionList { result: Err(_) }
    ));
    assert!(matches!(
        TuiEffect::LoadCommandPalette.application_access_failed(rejected()),
        TuiEffectResult::CommandPaletteLoaded { result: Err(_) }
    ));
    let result = TuiEffect::SaveDraft {
        scope: ComposerDraftScope::Session {
            session_id: SessionId::new(),
        },
        text: "unsent draft".into(),
    }
    .application_access_failed(rejected());
    assert!(
        matches!(result, TuiEffectResult::SaveDraft { text, result: Err(_) } if text == "unsent draft")
    );
}

#[test]
fn rejected_status_hydration_does_not_turn_connected_host_into_offline_state() {
    let session_id = SessionId::new();
    let result = TuiEffect::LoadSessionStatus { session_id }.application_access_failed(rejected());
    assert!(matches!(
        result.daemon_observation(),
        DaemonObservation::Success
    ));
    assert!(matches!(result, TuiEffectResult::SessionStatusLoaded {
        session_id: actual, daemon_connected: true, hydration,
    } if actual == session_id && hydration.error.is_some()));
}

#[test]
fn failed_managed_interrupt_does_not_claim_delivery_or_settlement() {
    let session_id = SessionId::new();
    let result = TuiEffect::CancelTurn { session_id }.application_access_failed(rejected());
    assert!(matches!(
        result.daemon_observation(),
        DaemonObservation::Failed(_)
    ));
    assert!(
        matches!(result, TuiEffectResult::CancelTurn { session_id: actual, result: Err(_) } if actual == session_id)
    );
}

#[test]
fn transport_failure_remains_distinct_from_generation_or_policy_rejection() {
    let result = TuiEffect::LoadNewerHistory {
        session_id: SessionId::new(),
        cursor: SessionHistoryCursor { sequence: 361 },
        generation: 7,
        width_columns: 80,
    }
    .application_access_failed(ClientError::Transport(bcode_ipc::IpcTransportError::Io(
        std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "fixture transport lost",
        ),
    )));
    assert!(matches!(
        result.daemon_observation(),
        DaemonObservation::Unavailable(_)
    ));
}
