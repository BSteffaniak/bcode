//! Session navigation for the Bcode `HyperChad` application.

use bcode_plugin_sdk::path::display_from_current_dir;
use bcode_session_models::SessionSummary;
use hyperchad::actions::logic::if_responsive;
use hyperchad::template::{Containers, container};

use super::semantic_dom_id;

pub(super) fn session_navigation(
    sessions: &[SessionSummary],
    selected_session_id: Option<bcode_session_models::SessionId>,
    access_token: &str,
) -> Containers {
    container! {
        aside #session-navigation width=(if_responsive("tablet").then::<i32>(10_000).or_else(280)) max-width=100% background="#161b22" border="1, #30363d" border-radius=10 padding=14 {
            h2 font-size=14 color="#f0f6fc" margin-bottom=12 { "sessions" }
            @if sessions.is_empty() {
                div color="#8b949e" font-size=12 { "No sessions loaded yet." }
            } @else {
                @for session in sessions {
                    @let selected = selected_session_id == Some(session.id);
                    @let activity = if session.client_count > 0 { "active" } else { "idle" };
                    @let item_id = semantic_dom_id("session", &session.id.to_string());
                    anchor href=(format!("/session/{}?token={access_token}&hyperchad-event-scope={access_token}:{}", session.id, session.id)) text-decoration="none" {
                        div id=(item_id) background=(if selected { "#1f2937" } else { "#0d1117" }) border="1, #30363d" border-radius=6 padding=9 margin-bottom=8 {
                            div justify-content=space-between gap=8 {
                                div color="#f0f6fc" font-size=13 { (session.title().unwrap_or("Untitled session")) }
                                span color=(if session.client_count > 0 { "#7ee787" } else { "#8b949e" }) font-size=10 {
                                    @if selected { "selected · " }
                                    (activity)
                                }
                            }
                            div color="#8b949e" font-size=11 margin-top=3 { (display_from_current_dir(&session.working_directory).to_string()) }
                            div color="#8b949e" font-size=10 margin-top=3 {
                                (session.client_count.to_string()) " connected · updated " (session.updated_at_ms.to_string())
                            }
                        }
                    }
                }
            }
        }
    }
}
