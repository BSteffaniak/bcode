//! Bounded live setup presentation built from shared semantic session snapshots.
use std::cell::Cell;
use std::time::Instant;

use bcode_plugin_sdk::tui::{
    PluginSessionViewSubscription, PluginSessionViewSubscriptionRequest, PluginSessionViewUpdate,
    PluginStructuredGenerationControl, PluginTuiHost,
};
use bcode_session_models::{
    ProjectionWindowAnchor, ProjectionWindowDirection, ProjectionWindowLimits,
    ProjectionWindowRequest, ProjectionWindowTarget, SessionProjectionKind,
};
use bcode_session_view_models::{SessionViewSnapshot, TranscriptViewItemKind};
use bmux_tui::prelude::Line;
use bmux_tui_components::scroll_view::ScrollViewState;

pub struct GenerationView {
    pub control: PluginStructuredGenerationControl,
    subscription: Option<PluginSessionViewSubscription>,
    started: Instant,
    updated: Instant,
    revision: Option<u64>,
    pub lines: Vec<Line>,
    pub scroll: Cell<ScrollViewState>,
    pub layout: Option<(bmux_tui::geometry::Rect, bmux_tui::component::LayoutNode)>,
    pub collapsed: bool,
    pub title: &'static str,
    pub footer: &'static str,
    pub detail: String,
}

impl Default for GenerationView {
    fn default() -> Self {
        Self {
            control: PluginStructuredGenerationControl::default(),
            subscription: None,
            started: Instant::now(),
            updated: Instant::now(),
            revision: None,
            lines: Vec::new(),
            scroll: Cell::new({
                let mut state = ScrollViewState::new();
                state.set_follow_bottom(true);
                state
            }),
            layout: None,
            collapsed: false,
            title: "Goal · Generating instructions",
            footer: "Draft output · h opens live session (return to setup) · Tab folds output · Esc cancels",
            detail: "Preparing source context".into(),
        }
    }
}

impl GenerationView {
    pub fn poll(&mut self, host: &dyn PluginTuiHost) {
        if self.subscription.is_none()
            && let Some(session_id) = self.control.session_id()
        {
            match host.subscribe_session_view(PluginSessionViewSubscriptionRequest {
                session_id,
                projection: ProjectionWindowRequest {
                    projection: SessionProjectionKind::Transcript,
                    anchor: ProjectionWindowAnchor::Latest,
                    direction: ProjectionWindowDirection::Backward,
                    target: ProjectionWindowTarget {
                        min_items: Some(16),
                        min_estimated_rows: None,
                        min_bytes: None,
                        width_columns: Some(100),
                    },
                    limits: ProjectionWindowLimits {
                        max_items: 32,
                        max_events_scanned: 256,
                        max_bytes: 65_536,
                    },
                },
                reasoning_policy: bcode_session_view_models::ReasoningPresentationPolicy::default(),
                buffer: 1,
            }) {
                Ok(subscription) => self.subscription = Some(subscription),
                Err(error) => self.detail = format!("Live observation unavailable: {error}"),
            }
        }
        if let Some(subscription) = &mut self.subscription {
            // Drain only the bounded channel; latest authoritative snapshot wins.
            for _ in 0..8 {
                let Ok(update) = subscription.receiver.try_recv() else {
                    break;
                };
                match update {
                    PluginSessionViewUpdate::Snapshot(snapshot) => {
                        if self.revision != Some(snapshot.revision) {
                            self.updated = Instant::now();
                            self.revision = Some(snapshot.revision);
                            self.lines = snapshot_lines(&snapshot);
                            self.layout = None;
                        }
                        self.detail = snapshot.runtime.provider_progress.as_ref().map_or_else(
                            || {
                                if snapshot.runtime.active_turn_id.is_some() {
                                    "Waiting for model output".into()
                                } else {
                                    "Preparing or finalizing generation".into()
                                }
                            },
                            |progress| progress.detail.clone(),
                        );
                    }
                    PluginSessionViewUpdate::Disconnected { message } => {
                        self.detail = format!("Observation disconnected: {message}");
                    }
                }
            }
        }
    }

    pub fn mark_updated(&mut self) {
        self.updated = Instant::now();
    }

    pub fn status(&self) -> String {
        format!(
            "{} · {}s elapsed · last update {}s ago\n{}",
            if self.control.is_cancelled() {
                "Cancelling generation (awaiting outcome)"
            } else {
                &self.detail
            },
            self.started.elapsed().as_secs(),
            self.updated.elapsed().as_secs(),
            self.footer
        )
    }
}

fn snapshot_lines(snapshot: &SessionViewSnapshot) -> Vec<Line> {
    let mut text = String::new();
    for item in &snapshot.transcript.items {
        match &item.kind {
            TranscriptViewItemKind::AssistantMessage { message } => {
                append(
                    &mut text,
                    &bcode_session_view::presentation::model_output_text(&message.text),
                );
            }
            TranscriptViewItemKind::ReasoningMessage { message } => {
                append(&mut text, &message.text);
            }
            TranscriptViewItemKind::ReasoningActivity { activity } => {
                for part in &activity.parts {
                    append(&mut text, &part.text);
                }
                if activity.parts.is_empty() {
                    append(&mut text, "Reasoning activity (no displayable text)");
                }
            }
            _ => {}
        }
    }
    text.lines()
        .map(|line| Line::from(line.to_owned()))
        .collect()
}

fn append(text: &mut String, value: &str) {
    let mut end = value.len().min(65_536_usize.saturating_sub(text.len()));
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    text.push_str(&value[..end]);
    if text.len() < 65_536 {
        text.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preview_is_utf8_safe_and_bounded() {
        let mut text = String::new();
        append(&mut text, &"界🙂".repeat(20_000));
        assert!(text.len() <= 65_536);
        assert!(text.is_char_boundary(text.len()));
        append(&mut text, "more");
        assert!(text.len() <= 65_536);
        assert!(text.is_char_boundary(text.len()));
    }
}
