//! Reusable semantic presentation primitives for application states.

use hyperchad::actions::logic::if_responsive;
use hyperchad::template::{Containers, container};
use hyperchad::transformer::models::LayoutDirection;

use super::theme::{color, radius, space, surface, typeface, width};

/// Semantic visual tone independent of any backend transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StatusTone {
    Neutral,
    Info,
    Success,
    Warning,
    Error,
}

const fn tone_color(tone: StatusTone) -> &'static str {
    match tone {
        StatusTone::Neutral => color::MUTED,
        StatusTone::Info => color::INFO,
        StatusTone::Success => color::SUCCESS,
        StatusTone::Warning => color::WARNING,
        StatusTone::Error => color::ERROR,
    }
}

pub(super) fn status_badge(label: &str, tone: StatusTone) -> Containers {
    container! {
        div data-status-tone=(format!("{tone:?}").to_ascii_lowercase()) background=(surface::PANEL) border=((1, surface::BORDER)) border-radius=((radius::PILL)) padding="6, 12" color=(tone_color(tone)) font-size=((typeface::LABEL)) {
            (label)
        }
    }
}

pub(super) fn status_notice(title: &str, detail: Option<&str>, tone: StatusTone) -> Containers {
    container! {
        aside data-notice-tone=(format!("{tone:?}").to_ascii_lowercase()) color=(tone_color(tone)) background=(surface::PANEL) border=((1, surface::BORDER)) border-radius=(radius::CARD) padding=((space::MD - 2)) margin-bottom=(space::MD) {
            div font-weight=bold { (title) }
            @if let Some(detail) = detail {
                div color=(color::TEXT) font-size=((typeface::LABEL)) margin-top=(space::XS) white-space="preserve-wrap" { (detail) }
            }
        }
    }
}

/// Render the reusable top-level application shell.
pub(super) fn application_shell(
    header: &Containers,
    notices: &Containers,
    navigation: &Containers,
    content: &Containers,
) -> Containers {
    container! {
        div #bcode-web-shell padding=(if_responsive("narrow").then::<i32>(space::MD).or_else(space::XL)) background=(surface::APP) color=(color::TEXT) font-family=(typeface::UI) {
            (header)
            (notices)
            div #application-layout direction=(if_responsive("tablet").then(LayoutDirection::Column).or_else(LayoutDirection::Row)) gap=(space::S18) align-items=start {
                (navigation)
                main #conversation-main flex=1 min-width=0 max-width=(if_responsive("tablet").then::<i32>(width::FLUID).or_else(width::CONTENT)) {
                    (content)
                }
            }
        }
    }
}

/// Render a reusable timeline region with history controls and semantic items.
pub(super) fn conversation_timeline(content: &Containers) -> Containers {
    container! {
        section #conversation-timeline background=(surface::PANEL) border=((1, surface::BORDER)) border-radius=(radius::PANEL) padding=(space::LG) margin-bottom=(space::S18) {
            h2 color=(color::STRONG) font-size=(typeface::SECTION) margin-bottom=(space::S14) { "transcript" }
            (content)
        }
    }
}

/// Semantic article presentation independent of transcript transport metadata.
pub(super) struct MessageArticle<'a> {
    pub(super) id: &'a str,
    pub(super) label: &'a str,
    pub(super) streaming: bool,
    pub(super) background: &'a str,
    pub(super) accent: &'a str,
    pub(super) margins: (i32, i32),
    pub(super) content: &'a Containers,
    pub(super) developer_detail: &'a Containers,
}

/// Render a reusable semantic message article.
pub(super) fn message_article(article: &MessageArticle<'_>) -> Containers {
    container! {
        section id=(article.id) data-content-kind="message-article" background=(article.background) border-left=((2, surface::BORDER)) border-radius=(radius::CARD) padding=(space::MD) margin-left=(article.margins.0) margin-right=(article.margins.1) margin-bottom=(space::S10) {
            header justify-content=space-between margin-bottom=(space::SM) color=(article.accent) font-size=(typeface::DETAIL) {
                span { (article.label) }
                @if article.streaming { span { "live" } }
            }
            (article.content)
            (article.developer_detail)
        }
    }
}

/// Render a reusable session navigation region.
pub(super) fn navigation_panel(content: &Containers) -> Containers {
    container! {
        aside #session-navigation width=(if_responsive("tablet").then::<i32>(width::FLUID).or_else(width::NAVIGATION)) max-width=100% background=(surface::PANEL) border=((1, surface::BORDER)) border-radius=(radius::PANEL) padding=(space::S14) {
            h2 font-size=(typeface::SUBHEADING) color=(color::STRONG) margin-bottom=(space::MD) { "sessions" }
            (content)
        }
    }
}

/// Render a reusable composer region.
pub(super) fn composer_panel(content: &Containers) -> Containers {
    section_panel("composer", content, false)
}

/// Render a reusable titled panel for major application regions.
pub(super) fn section_panel(title: &str, content: &Containers, margin_bottom: bool) -> Containers {
    container! {
        section background=(surface::PANEL) border=((1, surface::BORDER)) border-radius=(radius::PANEL) padding=(space::LG) margin-bottom=(if margin_bottom { space::S18 } else { 0 }) {
            h2 color=(color::STRONG) font-size=(typeface::SECTION) margin-bottom=(space::MD) { (title) }
            (content)
        }
    }
}

/// Render a reusable disclosure with secondary semantic detail.
pub(super) fn disclosure(summary: &str, content: &Containers) -> Containers {
    container! {
        details margin-top=(space::SM) {
            summary color=(color::MUTED) font-size=(typeface::DETAIL) { (summary) }
            div margin-top=(space::S6) { (content) }
        }
    }
}

/// Render reusable preformatted code or output content.
pub(super) fn code_output(content: &str, tone: StatusTone) -> Containers {
    container! {
        div data-code-tone=(format!("{tone:?}").to_ascii_lowercase()) white-space="preserve-wrap" font-family="monospace" background=(surface::INSET) border=((1, surface::BORDER)) border-radius=(radius::CONTROL) padding=(space::SM) color=(tone_color(tone)) {
            (content)
        }
    }
}

/// Render a reusable lifecycle card for a tool operation.
pub(super) fn tool_card(
    title: &str,
    subtitle: Option<&str>,
    status: &str,
    tone: StatusTone,
    content: &Containers,
) -> Containers {
    container! {
        section background=(surface::APP) border=((1, surface::BORDER)) border-radius=(radius::CARD) padding=(space::MD) {
            div justify-content=space-between gap=(space::MD) margin-bottom=(space::SM) {
                div {
                    div color=(color::STRONG) { (title) }
                    @if let Some(subtitle) = subtitle {
                        div color=(color::MUTED) font-size=(typeface::DETAIL) margin-top=(space::S3) { (subtitle) }
                    }
                }
                div color=(tone_color(tone)) font-size=(typeface::LABEL) { (status) }
            }
            (content)
        }
    }
}

/// Render a reusable permission request or history card.
pub(super) fn permission_card(
    title: &str,
    status: &str,
    tone: StatusTone,
    content: &Containers,
    active: bool,
) -> Containers {
    container! {
        aside data-permission-active=(active.to_string()) border=((1, if active { color::WARNING } else { surface::BORDER })) border-radius=(radius::CARD) padding=(space::S10) {
            div justify-content=space-between gap=(space::MD) margin-bottom=(space::S6) {
                div color=(tone_color(tone)) { (title) }
                div color=(tone_color(tone)) font-size=(typeface::LABEL) { (status) }
            }
            (content)
        }
    }
}

/// Render a reusable interaction lifecycle card.
pub(super) fn interaction_card(
    title: &str,
    status: &str,
    tone: StatusTone,
    content: &Containers,
) -> Containers {
    container! {
        div data-interaction-status=(status) border=((1, color::INFO)) border-radius=(radius::CARD) padding=(space::S10) {
            div justify-content=space-between gap=(space::SM) margin-bottom=(space::S6) {
                div color=(color::INFO) { (title) }
                div color=(tone_color(tone)) font-size=(typeface::LABEL) { (status) }
            }
            (content)
        }
    }
}

/// Render semantic determinate or indeterminate progress.
pub(super) fn progress_status(
    label: &str,
    completed: Option<u64>,
    total: Option<u64>,
) -> Containers {
    let normalized = completed.zip(total).filter(|(_, total)| *total > 0);
    let (status, detail, percent) = normalized.map_or_else(
        || ("indeterminate", "In progress".to_owned(), None),
        |(completed, total)| {
            let bounded = completed.min(total);
            let percent = bounded.saturating_mul(100) / total;
            (
                "determinate",
                format!("{bounded} of {total} complete ({percent}%)"),
                Some(percent),
            )
        },
    );
    container! {
        div data-progress-state=(status) color=(color::INFO) font-size=((typeface::DETAIL)) margin-top=(space::XS) {
            span font-weight=bold { (label) ": " }
            span { (detail) }
            @if let Some(percent) = percent {
                div data-progress-percent=(percent.to_string()) background=(surface::INSET) border=((1, surface::BORDER)) border-radius=(radius::PILL) padding=(space::S2) margin-top=(space::XS) {
                    div background=(color::INFO) border-radius=(radius::PILL) height=(space::XS) width=(format!("{percent}%"));
                }
            }
        }
    }
}

pub(super) fn empty_state(message: &str) -> Containers {
    container! {
        div data-content-state="empty" color=(color::MUTED) font-size=(typeface::BODY) { (message) }
    }
}

pub(super) fn truncation_notice(message: &str) -> Containers {
    container! {
        div data-content-state="truncated" color=(color::WARNING) font-size=(typeface::DETAIL) margin-top=(space::SM) { (message) }
    }
}

pub(super) fn unsupported_content(message: &str) -> Containers {
    container! {
        aside data-content-state="unsupported" color=(color::WARNING) background=(surface::PANEL) border=((1, surface::BORDER)) border-radius=(radius::CONTROL) padding=(space::SM) margin-top=(space::SM) {
            div font-size=((typeface::LABEL)) { "Unsupported content" }
            div color=(color::TEXT) font-size=(typeface::DETAIL) margin-top=(space::XS) white-space="preserve-wrap" { (message) }
        }
    }
}
