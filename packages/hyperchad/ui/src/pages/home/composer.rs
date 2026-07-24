//! Session composer and turn controls.

use super::theme::{accent, color, radius, space, surface, typeface};
use crate::context::{PresentationAction, PresentationContext};
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

pub(super) fn composer(
    snapshot: &SessionViewSnapshot,
    context: &impl PresentationContext,
) -> Containers {
    let action = context.action_target(PresentationAction::SubmitMessage);
    let presentation_state = composer_presentation_state(snapshot);
    let (state_label, state_color) = match presentation_state {
        ComposerPresentationState::Ready => ("Ready", color::SUCCESS),
        ComposerPresentationState::Pending => ("Pending", color::WARNING),
        ComposerPresentationState::Error => ("Error", color::ERROR),
        ComposerPresentationState::Disabled => ("Disabled", color::MUTED),
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
            form hx-post=(action) hx-target="#bcode-web-shell" hx-swap=this background=(surface::APP) border=((1, surface::BORDER)) border-radius=((radius::CARD)) padding=((space::MD)) {
                @if let Some(session_id) = snapshot.session_id {
                    input type=hidden name="session_id" value=(session_id.to_string());
                }
                div data-composer-state=(state_label.to_ascii_lowercase()) color=(state_color) font-size=((typeface::DETAIL)) margin-bottom=((space::SM)) {
                    (state_label) ": " (state_detail)
                }
                div #composer-message-label color=(color::MUTED) font-size=((typeface::DETAIL)) margin-bottom=((space::XS)) { "Message" }
                @if let Some(session_id) = snapshot.session_id {
                    textarea #composer-message name="text" rows="5" data-label-id="composer-message-label" placeholder="Send a message to this session" hx-post=(context.action_target(PresentationAction::UpdateDraft { session_id })) hx-trigger="change" hx-target="#bcode-web-shell" hx-swap=this width=100% padding=((space::S10)) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) color=(color::TEXT) {
                        (snapshot.composer.draft)
                    }
                } @else {
                    textarea #composer-message name="text" rows="5" data-label-id="composer-message-label" placeholder="Send a message to start a session" width=100% padding=((space::S10)) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) color=(color::TEXT) {
                        (snapshot.composer.draft)
                    }
                }
                div direction=row justify-content=space-between align-items=center gap=((space::S10)) margin-top=((space::S10)) {
                    div {
                        div #message-placement-label color=(color::MUTED) font-size=((typeface::DETAIL)) margin-bottom=((space::XS)) { "Message placement" }
                        select #message-placement name="placement" selected="steering" data-label-id="message-placement-label" padding=((space::S7)) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) color=(color::TEXT) {
                            option value="steering" { "Steer the active turn" }
                            option value="follow_up" { "Queue as a follow-up" }
                        }
                    }
                    button type=submit disabled=[!snapshot.composer.can_submit] background=(if snapshot.composer.can_submit { accent::POSITIVE } else { surface::DISABLED }) color=(color::ON_ACCENT) border-radius=((radius::CONTROL)) padding="8, 14" {
                        (if snapshot.composer.can_submit { "Send message" } else { "Sending unavailable" })
                    }
                }
            }
            @if let Some(session_id) = snapshot.session_id {
                form hx-post=(context.action_target(PresentationAction::CancelTurn)) hx-target="#bcode-web-shell" hx-swap=this margin-top=((space::S10)) {
                    input type=hidden name="session_id" value=(session_id.to_string());
                    input type=hidden name="clear_queue" value="true";
                    button type=submit background=(accent::DESTRUCTIVE) color=(color::ON_ACCENT) border-radius=((radius::CONTROL)) padding="8, 14" {
                        "Cancel active turn"
                    }
                }
            }
        }
    }
}
