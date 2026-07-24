//! Top-level Bcode `HyperChad` application shell.

use bcode_plugin_sdk::path::display_from_current_dir;
use bcode_session_models::SessionSummary;
use bcode_session_view_models::SessionViewSnapshot;
use hyperchad::actions::logic::if_responsive;
use hyperchad::template::{Containers, container};
use hyperchad::transformer::models::{AlignItems, LayoutDirection};

use super::activity::{
    active_invocations_section, runtime_state_section, runtime_work_section,
    unrepresented_active_invocations, unrepresented_runtime_work,
};
use super::composer::composer;
use super::interactions::interaction_request;
use super::navigation::session_navigation;
use super::permissions::permission_request;
use super::transcript::transcript_section;

/// Render the Bcode `HyperChad` application shell.
#[must_use]
pub fn home(
    snapshot: &SessionViewSnapshot,
    sessions: &[SessionSummary],
    access_token: &str,
) -> Containers {
    let title = snapshot
        .title
        .as_deref()
        .or_else(|| {
            snapshot
                .session_summary
                .as_ref()
                .and_then(SessionSummary::title)
        })
        .unwrap_or("No session attached");
    let status = if snapshot.session_id.is_some() {
        "daemon connected · session attached"
    } else {
        "daemon connected · no active session"
    };
    let unrepresented_invocations = unrepresented_active_invocations(snapshot);
    let unrepresented_work = unrepresented_runtime_work(snapshot);

    container! {
        div #bcode-web-shell padding=(if_responsive("narrow").then::<i32>(12).or_else(24)) background="#0d1117" color="#c9d1d9" font-family="system-ui, sans-serif" {
            header #application-header direction=(if_responsive("narrow").then(LayoutDirection::Column).or_else(LayoutDirection::Row)) justify-content=space-between align-items=(if_responsive("narrow").then(AlignItems::Start).or_else(AlignItems::Center)) gap=12 margin-bottom=24 {
                div {
                    h1 color="#7ee787" font-size=28 margin-bottom=4 { "bcode web" }
                    div color="#8b949e" font-size=13 { "renderer-neutral session view powered by HyperChad" }
                }
                div background="#161b22" border="1, #30363d" border-radius=999 padding="6, 12" color="#7ee787" font-size=12 {
                    (status)
                }
            }

            div #application-layout direction=(if_responsive("tablet").then(LayoutDirection::Column).or_else(LayoutDirection::Row)) gap=18 align-items=start {
                (session_navigation(sessions, snapshot.session_id, access_token))

                main #conversation-main flex=1 min-width=0 max-width=(if_responsive("tablet").then::<i32>(10_000).or_else(960)) {
                    section background="#161b22" border="1, #30363d" border-radius=10 padding=16 margin-bottom=18 {
                        div justify-content=space-between gap=12 align-items=start {
                            div {
                                h2 color="#f0f6fc" font-size=20 margin-bottom=4 { (title) }
                                details {
                                    summary color="#8b949e" font-size=11 { "session details" }
                                    div color="#8b949e" font-size=11 margin-top=4 {
                                        "revision " (snapshot.revision.to_string())
                                        @if let Some(sequence) = snapshot.latest_sequence {
                                            " · latest event " (sequence.to_string())
                                        }
                                    }
                                }
                            }
                            div color="#8b949e" font-size=12 {
                                (snapshot.working_directory.as_ref().map_or_else(|| "—".to_string(), |path| display_from_current_dir(path).to_string()))
                            }
                        }
                    }

                    @if !unrepresented_invocations.is_empty() {
                        (active_invocations_section(&unrepresented_invocations))
                    }
                    @if !unrepresented_work.is_empty() {
                        (runtime_work_section(&unrepresented_work))
                    }

                    (runtime_state_section(snapshot))

                    (transcript_section(snapshot, access_token))

                    @if !snapshot.interactions.is_empty() {
                        section background="#161b22" border="1, #30363d" border-radius=10 padding=16 margin-bottom=18 {
                            h2 color="#f0f6fc" font-size=16 margin-bottom=14 { "interactions" }
                            @for interaction in &snapshot.interactions {
                                (interaction_request(interaction, snapshot.session_id, access_token))
                            }
                        }
                    }

                    @if !snapshot.permissions.is_empty() {
                        section background="#161b22" border="1, #30363d" border-radius=10 padding=16 margin-bottom=18 {
                            h2 color="#f0f6fc" font-size=16 margin-bottom=14 { "permissions" }
                            @for permission in &snapshot.permissions {
                                (permission_request(permission, snapshot.session_id, access_token))
                            }
                        }
                    }

                    section background="#161b22" border="1, #30363d" border-radius=10 padding=16 {
                        h2 color="#f0f6fc" font-size=16 margin-bottom=12 { "composer" }
                        (composer(snapshot, access_token))
                    }
                }
            }
        }
    }
}
