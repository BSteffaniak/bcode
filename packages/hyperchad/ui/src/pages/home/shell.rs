//! Top-level Bcode `HyperChad` application shell.

use super::theme::{color, radius, space, surface, typeface};
use crate::context::PresentationContext;
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
use super::components::{
    StatusTone, application_shell, composer_panel, section_panel, status_badge, status_notice,
};
use super::composer::composer;
use super::interactions::interaction_request;
use super::navigation::session_navigation;
use super::permissions::permission_request;
use super::transcript::transcript_section;

const fn connection_notice(
    status: &bcode_session_view_models::SessionConnectionViewStatus,
) -> (&str, Option<&str>, StatusTone) {
    match status {
        bcode_session_view_models::SessionConnectionViewStatus::Disconnected => (
            "Disconnected",
            Some("Live session updates are unavailable."),
            StatusTone::Error,
        ),
        bcode_session_view_models::SessionConnectionViewStatus::Connected => (
            "Connected · no active session",
            Some("Select a session or send a message to begin."),
            StatusTone::Neutral,
        ),
        bcode_session_view_models::SessionConnectionViewStatus::Attached => {
            ("Connected · session attached", None, StatusTone::Success)
        }
        bcode_session_view_models::SessionConnectionViewStatus::Reconnecting => (
            "Reconnecting to session…",
            Some("Showing the last available session view while live updates reconnect."),
            StatusTone::Warning,
        ),
        bcode_session_view_models::SessionConnectionViewStatus::Resyncing => (
            "Refreshing session state…",
            Some("A complete authoritative session view is being requested."),
            StatusTone::Info,
        ),
        bcode_session_view_models::SessionConnectionViewStatus::Error(_) => (
            "Session unavailable",
            Some(
                "The session could not be refreshed. Try reconnecting or restart Bcode if the problem continues.",
            ),
            StatusTone::Error,
        ),
    }
}

const fn catalog_notice(
    status: &bcode_session_view_models::SessionCatalogViewStatus,
) -> Option<(&str, StatusTone)> {
    match status {
        bcode_session_view_models::SessionCatalogViewStatus::NotStarted => {
            Some(("Session discovery has not started.", StatusTone::Neutral))
        }
        bcode_session_view_models::SessionCatalogViewStatus::Loading => {
            Some(("Loading available sessions…", StatusTone::Info))
        }
        bcode_session_view_models::SessionCatalogViewStatus::Loaded => None,
        bcode_session_view_models::SessionCatalogViewStatus::Degraded(_) => Some((
            "Some sessions could not be loaded. Available sessions remain usable; repair damaged sessions to restore the full list.",
            StatusTone::Warning,
        )),
        bcode_session_view_models::SessionCatalogViewStatus::Failed(_) => Some((
            "Sessions could not be loaded. Restart Bcode or run session repair before trying again.",
            StatusTone::Error,
        )),
    }
}

/// Render the Bcode `HyperChad` application shell.
#[must_use]
pub fn home(
    snapshot: &SessionViewSnapshot,
    sessions: &[SessionSummary],
    context: &impl PresentationContext,
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
    let (status, status_detail, status_tone) = connection_notice(&snapshot.connection_status);
    let catalog_status = catalog_notice(&snapshot.catalog_status);
    let unrepresented_invocations = unrepresented_active_invocations(snapshot);
    let unrepresented_work = unrepresented_runtime_work(snapshot);

    let header = container! {
        header #application-header direction=(if_responsive("narrow").then(LayoutDirection::Column).or_else(LayoutDirection::Row)) justify-content=space-between align-items=(if_responsive("narrow").then(AlignItems::Start).or_else(AlignItems::Center)) gap=((space::MD)) margin-bottom=((space::XL)) {
            div {
                h1 color=(color::SUCCESS) font-size=(typeface::TITLE) margin-bottom=((space::XS)) { "bcode web" }
                div color=(color::MUTED) font-size=(typeface::BODY) { "renderer-neutral session view powered by HyperChad" }
            }
            (status_badge(status, status_tone))
        }
    };
    let notices = container! {
        @if status_detail.is_some() {
            (status_notice(status, status_detail, status_tone))
        }
        @if let Some((message, tone)) = catalog_status {
            (status_notice("Session catalog", Some(message), tone))
        }
        @if let Some(notice) = &snapshot.notice {
            (status_notice(
                match notice.level {
                    bcode_session_view_models::SessionViewNoticeLevel::Info => "Status",
                    bcode_session_view_models::SessionViewNoticeLevel::Warning => "Attention",
                    bcode_session_view_models::SessionViewNoticeLevel::Error => "Action failed",
                },
                Some(&notice.message),
                match notice.level {
                    bcode_session_view_models::SessionViewNoticeLevel::Info => StatusTone::Info,
                    bcode_session_view_models::SessionViewNoticeLevel::Warning => StatusTone::Warning,
                    bcode_session_view_models::SessionViewNoticeLevel::Error => StatusTone::Error,
                },
            ))
        }
    };
    let body = container! {
        section background=(surface::PANEL) border=((1, surface::BORDER)) border-radius=((radius::PANEL)) padding=((space::LG)) margin-bottom=((space::S18)) {
            div justify-content=space-between gap=((space::MD)) align-items=start {
                div {
                    h2 color=(color::STRONG) font-size=((typeface::HEADING)) margin-bottom=((space::XS)) { (title) }
                    details {
                        summary color=(color::MUTED) font-size=((typeface::DETAIL)) { "session details" }
                        div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::XS)) {
                            "revision " (snapshot.revision.to_string())
                            @if let Some(sequence) = snapshot.latest_sequence {
                                " · latest event " (sequence.to_string())
                            }
                        }
                    }
                }
                div color=(color::MUTED) font-size=((typeface::LABEL)) {
                    (snapshot.working_directory.as_ref().map_or_else(|| "—".to_string(), |path| display_from_current_dir(path).to_string()))
                }
            }
        }
        @if !unrepresented_invocations.is_empty() { (active_invocations_section(&unrepresented_invocations)) }
        @if !unrepresented_work.is_empty() { (runtime_work_section(&unrepresented_work)) }
        (runtime_state_section(snapshot))
        (transcript_section(snapshot, context))
        @if !snapshot.interactions.is_empty() {
            (section_panel("interactions", &container! {
                @for interaction in &snapshot.interactions {
                    (interaction_request(interaction, snapshot.session_id, context))
                }
            }, true))
        }
        @if !snapshot.permissions.is_empty() {
            (section_panel("permissions", &container! {
                @for permission in &snapshot.permissions {
                    (permission_request(permission, snapshot.session_id, context))
                }
            }, true))
        }
        (composer_panel(&composer(snapshot, context)))
    };
    application_shell(
        &header,
        &notices,
        &session_navigation(
            sessions,
            snapshot.session_id,
            &snapshot.catalog_status,
            context,
        ),
        &body,
    )
}
