//! Session navigation for the Bcode `HyperChad` application.

use super::theme::{color, radius, space, surface, typeface};
use crate::context::PresentationContext;
use bcode_plugin_sdk::path::display_from_current_dir;
use bcode_session_models::SessionSummary;
use bcode_session_view_models::SessionCatalogViewStatus;
use hyperchad::template::{Containers, container};

use super::components::navigation_panel;
use super::semantic_dom_id;

pub(super) fn session_navigation(
    sessions: &[SessionSummary],
    selected_session_id: Option<bcode_session_models::SessionId>,
    catalog_status: &SessionCatalogViewStatus,
    context: &impl PresentationContext,
) -> Containers {
    navigation_panel(&container! {
            @match catalog_status {
                SessionCatalogViewStatus::Loading => {
                    div color=(color::INFO) font-size=((typeface::DETAIL)) margin-bottom=((space::S10)) { "Updating session list…" }
                }
                SessionCatalogViewStatus::Degraded(_) => {
                    div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-bottom=((space::S10)) { "Session list is incomplete." }
                }
                SessionCatalogViewStatus::Failed(_) => {
                    div color=(color::ERROR) font-size=((typeface::DETAIL)) margin-bottom=((space::S10)) { "Session list is unavailable." }
                }
                SessionCatalogViewStatus::NotStarted | SessionCatalogViewStatus::Loaded => {}
            }
            @if sessions.is_empty() {
                div color=(color::MUTED) font-size=((typeface::LABEL)) { "No sessions loaded yet." }
            } @else {
                @for session in sessions {
                    @let selected = selected_session_id == Some(session.id);
                    @let activity = if session.client_count > 0 { "active" } else { "idle" };
                    @let item_id = semantic_dom_id("session", &session.id.to_string());
                    anchor href=(context.session_target(session.id)) text-decoration="none" {
                        div id=(item_id) background=(if selected { surface::SELECTED } else { surface::APP }) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) padding=((space::S9)) margin-bottom=((space::SM)) {
                            div justify-content=space-between gap=((space::SM)) {
                                div color=(color::STRONG) font-size=((typeface::BODY)) { (session.title().unwrap_or("Untitled session")) }
                                span color=(if session.client_count > 0 { color::SUCCESS } else { color::MUTED }) font-size=((typeface::CAPTION)) {
                                    @if selected { "selected · " }
                                    (activity)
                                }
                            }
                            div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::S3)) { (display_from_current_dir(&session.working_directory).to_string()) }
                            div color=(color::MUTED) font-size=((typeface::CAPTION)) margin-top=((space::S3)) {
                                (session.client_count.to_string()) " connected · updated " (session.updated_at_ms.to_string())
                            }
                        }
                    }
                }
            }
    })
}
