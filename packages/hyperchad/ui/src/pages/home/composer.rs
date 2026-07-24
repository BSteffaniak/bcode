//! Session composer and turn controls.

use bcode_session_view_models::SessionViewSnapshot;
use hyperchad::template::{Containers, container};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposerPresentationState {
    Ready,
    Pending,
    Error,
    Disabled,
}

fn composer_presentation_state(snapshot: &SessionViewSnapshot) -> ComposerPresentationState {
    if snapshot.composer.can_submit {
        return ComposerPresentationState::Ready;
    }
    let reason = snapshot
        .composer
        .disabled_reason
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ["error", "failed", "invalid", "cannot", "unavailable"]
        .iter()
        .any(|term| reason.contains(term))
    {
        ComposerPresentationState::Error
    } else if ["pending", "sending", "submitting", "waiting", "queued"]
        .iter()
        .any(|term| reason.contains(term))
    {
        ComposerPresentationState::Pending
    } else {
        ComposerPresentationState::Disabled
    }
}

pub(super) fn composer(snapshot: &SessionViewSnapshot, access_token: &str) -> Containers {
    let action = format!("/actions/submit-message?token={access_token}");
    let presentation_state = composer_presentation_state(snapshot);
    let (state_label, state_color) = match presentation_state {
        ComposerPresentationState::Ready => ("Ready", "#7ee787"),
        ComposerPresentationState::Pending => ("Pending", "#f2cc60"),
        ComposerPresentationState::Error => ("Error", "#f85149"),
        ComposerPresentationState::Disabled => ("Disabled", "#8b949e"),
    };
    let state_detail = snapshot.composer.disabled_reason.as_deref().unwrap_or({
        if snapshot.composer.can_submit {
            "Ready to send"
        } else {
            "Message submission is unavailable"
        }
    });
    container! {
        div #composer-region {
            form hx-post=(action) hx-target="#bcode-web-shell" hx-swap=this background="#0d1117" border="1, #30363d" border-radius=8 padding=12 {
                @if let Some(session_id) = snapshot.session_id {
                    input type=hidden name="session_id" value=(session_id.to_string());
                }
                div data-composer-state=(state_label.to_ascii_lowercase()) color=(state_color) font-size=11 margin-bottom=8 {
                    (state_label) ": " (state_detail)
                }
                @if let Some(session_id) = snapshot.session_id {
                    textarea name="text" rows="5" placeholder="Send a message to this session" hx-post=(format!("/actions/update-draft/{session_id}?token={access_token}")) hx-trigger="change" hx-target="#bcode-web-shell" hx-swap=this width=100% padding=10 border="1, #30363d" border-radius=6 background="#010409" color="#c9d1d9" {
                        (snapshot.composer.draft)
                    }
                } @else {
                    textarea name="text" rows="5" placeholder="Send a message to start a session" width=100% padding=10 border="1, #30363d" border-radius=6 background="#010409" color="#c9d1d9" {
                        (snapshot.composer.draft)
                    }
                }
                div direction=row justify-content=space-between align-items=center gap=10 margin-top=10 {
                    div {
                        div color="#8b949e" font-size=11 margin-bottom=4 { "Message placement" }
                        select name="placement" selected="steering" padding=7 border="1, #30363d" border-radius=6 background="#010409" color="#c9d1d9" {
                            option value="steering" { "Steer the active turn" }
                            option value="follow_up" { "Queue as a follow-up" }
                        }
                    }
                    button type=submit disabled=[!snapshot.composer.can_submit] background=(if snapshot.composer.can_submit { "#238636" } else { "#30363d" }) color=white border-radius=6 padding="8, 14" {
                        (if snapshot.composer.can_submit { "Send message" } else { "Sending unavailable" })
                    }
                }
            }
            @if let Some(session_id) = snapshot.session_id {
                form hx-post=(format!("/actions/cancel-turn?token={access_token}")) hx-target="#bcode-web-shell" hx-swap=this margin-top=10 {
                    input type=hidden name="session_id" value=(session_id.to_string());
                    input type=hidden name="clear_queue" value="true";
                    button type=submit background="#da3633" color=white border-radius=6 padding="8, 14" {
                        "Cancel active turn"
                    }
                }
            }
        }
    }
}
