//! Renderer-neutral Vim edit playback interaction controller.

use bcode_plugin_sdk::interaction::PluginInteraction;
use bcode_tool::{
    InteractionControlId, InteractionInput, InteractionNavigation, InteractionOutput,
    InteractionValue,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::VIM_EDIT_PLAYBACK_INTERACTION_KIND;

/// Focus target for Vim edit playback controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum VimEditPlaybackFocusTarget {
    /// First frame control.
    First,
    /// Previous changed frame control.
    PreviousChanged,
    /// Previous frame control.
    Previous,
    /// Play/pause control.
    PlayPause,
    /// Next frame control.
    Next,
    /// Next changed frame control.
    NextChanged,
    /// Last frame control.
    Last,
    /// Toggle timeline control.
    Timeline,
    /// Toggle diff control.
    Diff,
    /// Request applying a preview.
    ApplyRequested,
    /// Close playback control.
    Close,
}

impl VimEditPlaybackFocusTarget {
    /// Return stable control id.
    #[must_use]
    pub fn control_id(self) -> InteractionControlId {
        InteractionControlId::new(match self {
            Self::First => "first",
            Self::PreviousChanged => "previous_changed",
            Self::Previous => "previous",
            Self::PlayPause => "play_pause",
            Self::Next => "next",
            Self::NextChanged => "next_changed",
            Self::Last => "last",
            Self::Timeline => "timeline",
            Self::Diff => "diff",
            Self::ApplyRequested => "apply_requested",
            Self::Close => "close",
        })
    }
}

/// Request for an interactive Vim edit playback session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VimEditPlaybackRequest {
    /// Final playback artifact payload.
    pub playback: Value,
}

/// Renderer-neutral playback snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VimEditPlaybackSnapshot {
    /// Final playback artifact payload.
    pub playback: Value,
    /// Selected frame index.
    pub selected_frame: usize,
    /// Whether timeline rows are visible.
    pub show_timeline: bool,
    /// Whether diff rows are visible.
    pub show_diff: bool,
    /// Whether autoplay is active.
    pub playing: bool,
    /// Autoplay interval in milliseconds.
    pub playback_interval_ms: u64,
    /// Current focus target.
    pub focus: VimEditPlaybackFocusTarget,
    /// Current focused control id.
    pub focused_control_id: InteractionControlId,
}

/// Renderer-neutral Vim edit playback controller.
pub struct VimEditPlaybackInteractionController {
    request: VimEditPlaybackRequest,
    selected_frame: usize,
    show_timeline: bool,
    show_diff: bool,
    playing: bool,
    focus: VimEditPlaybackFocusTarget,
}

impl VimEditPlaybackInteractionController {
    /// Create a playback controller.
    #[must_use]
    pub const fn new(request: VimEditPlaybackRequest) -> Self {
        Self {
            request,
            selected_frame: 0,
            show_timeline: true,
            show_diff: true,
            playing: false,
            focus: VimEditPlaybackFocusTarget::Next,
        }
    }

    fn frames(&self) -> Option<&Vec<Value>> {
        self.request
            .playback
            .get("frames")
            .or_else(|| self.request.playback.get("events"))
            .and_then(Value::as_array)
    }

    fn frame_count(&self) -> usize {
        self.frames().map_or(0, Vec::len)
    }

    const fn select_previous(&mut self) {
        self.selected_frame = self.selected_frame.saturating_sub(1);
    }

    fn select_next(&mut self) {
        let count = self.frame_count();
        if count > 0 {
            self.selected_frame = self.selected_frame.saturating_add(1).min(count - 1);
        }
    }

    const fn select_first(&mut self) {
        self.selected_frame = 0;
    }

    fn select_last(&mut self) {
        self.selected_frame = self.frame_count().saturating_sub(1);
    }

    fn select_changed(&mut self, direction: ChangeDirection) {
        let Some(frames) = self.frames() else { return };
        let range: Box<dyn Iterator<Item = usize>> = match direction {
            ChangeDirection::Previous => Box::new((0..self.selected_frame).rev()),
            ChangeDirection::Next => Box::new(self.selected_frame.saturating_add(1)..frames.len()),
        };
        if let Some(index) = range.into_iter().find(|index| {
            frames
                .get(*index)
                .and_then(|frame| frame.get("changed"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        }) {
            self.selected_frame = index;
        }
    }

    fn activate(&mut self, control_id: &InteractionControlId) -> InteractionOutput {
        match control_id.as_str() {
            "first" => self.select_first(),
            "previous_changed" => self.select_changed(ChangeDirection::Previous),
            "previous" => self.select_previous(),
            "play_pause" => self.playing = !self.playing,
            "next" => self.select_next(),
            "next_changed" => self.select_changed(ChangeDirection::Next),
            "last" => self.select_last(),
            "timeline" => self.show_timeline = !self.show_timeline,
            "diff" => self.show_diff = !self.show_diff,
            "apply_requested" => {
                return InteractionOutput::Submitted {
                    payload: serde_json::json!({
                        "action": "apply_requested",
                        "tool_name": "vim_edit.apply",
                        "arguments": self.request.playback.get("original_arguments").cloned().unwrap_or(Value::Null),
                    }),
                };
            }
            "close" => {
                return InteractionOutput::Submitted {
                    payload: serde_json::json!({ "action": "closed" }),
                };
            }
            _ => return InteractionOutput::None,
        }
        InteractionOutput::Redraw
    }

    const fn navigate(&mut self, direction: InteractionNavigation) {
        self.focus = match direction {
            InteractionNavigation::Next => next_focus(self.focus),
            InteractionNavigation::Previous => previous_focus(self.focus),
        };
    }

    fn focus_control(&mut self, control_id: &InteractionControlId) {
        self.focus = match control_id.as_str() {
            "first" => VimEditPlaybackFocusTarget::First,
            "previous_changed" => VimEditPlaybackFocusTarget::PreviousChanged,
            "previous" => VimEditPlaybackFocusTarget::Previous,
            "play_pause" => VimEditPlaybackFocusTarget::PlayPause,
            "next" => VimEditPlaybackFocusTarget::Next,
            "next_changed" => VimEditPlaybackFocusTarget::NextChanged,
            "last" => VimEditPlaybackFocusTarget::Last,
            "timeline" => VimEditPlaybackFocusTarget::Timeline,
            "diff" => VimEditPlaybackFocusTarget::Diff,
            "apply_requested" => VimEditPlaybackFocusTarget::ApplyRequested,
            "close" => VimEditPlaybackFocusTarget::Close,
            _ => self.focus,
        };
    }

    fn change(
        &mut self,
        control_id: &InteractionControlId,
        value: &InteractionValue,
    ) -> InteractionOutput {
        if control_id.as_str() != "selected_frame" {
            return InteractionOutput::None;
        }
        let index = match value {
            InteractionValue::Number(value) => usize::try_from(*value).ok(),
            InteractionValue::String(value) => value.parse::<usize>().ok(),
            InteractionValue::Null
            | InteractionValue::Bool(_)
            | InteractionValue::List(_)
            | InteractionValue::Object(_) => None,
        };
        let Some(index) = index else {
            return InteractionOutput::None;
        };
        self.selected_frame = index.min(self.frame_count().saturating_sub(1));
        InteractionOutput::Redraw
    }

    fn snapshot(&self) -> VimEditPlaybackSnapshot {
        VimEditPlaybackSnapshot {
            playback: self.request.playback.clone(),
            selected_frame: self.selected_frame,
            show_timeline: self.show_timeline,
            show_diff: self.show_diff,
            playing: self.playing,
            playback_interval_ms: 150,
            focus: self.focus,
            focused_control_id: self.focus.control_id(),
        }
    }

    fn handle_input(&mut self, input: InteractionInput) -> InteractionOutput {
        match input {
            InteractionInput::Activate { control_id } => self.activate(&control_id),
            InteractionInput::Focus { control_id } => {
                self.focus_control(&control_id);
                InteractionOutput::Redraw
            }
            InteractionInput::Navigate { direction } => {
                self.navigate(direction);
                InteractionOutput::Redraw
            }
            InteractionInput::Submit => self.activate(&self.focus.control_id()),
            InteractionInput::Cancel => InteractionOutput::Cancelled,
            InteractionInput::Tick if self.playing => {
                self.select_next();
                InteractionOutput::Redraw
            }
            InteractionInput::Tick | InteractionInput::Blur { .. } => InteractionOutput::None,
            InteractionInput::Change { control_id, value } => self.change(&control_id, &value),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ChangeDirection {
    Previous,
    Next,
}

const fn next_focus(focus: VimEditPlaybackFocusTarget) -> VimEditPlaybackFocusTarget {
    match focus {
        VimEditPlaybackFocusTarget::First => VimEditPlaybackFocusTarget::PreviousChanged,
        VimEditPlaybackFocusTarget::PreviousChanged => VimEditPlaybackFocusTarget::Previous,
        VimEditPlaybackFocusTarget::Previous => VimEditPlaybackFocusTarget::PlayPause,
        VimEditPlaybackFocusTarget::PlayPause => VimEditPlaybackFocusTarget::Next,
        VimEditPlaybackFocusTarget::Next => VimEditPlaybackFocusTarget::NextChanged,
        VimEditPlaybackFocusTarget::NextChanged => VimEditPlaybackFocusTarget::Last,
        VimEditPlaybackFocusTarget::Last => VimEditPlaybackFocusTarget::Timeline,
        VimEditPlaybackFocusTarget::Timeline => VimEditPlaybackFocusTarget::Diff,
        VimEditPlaybackFocusTarget::Diff => VimEditPlaybackFocusTarget::ApplyRequested,
        VimEditPlaybackFocusTarget::ApplyRequested | VimEditPlaybackFocusTarget::Close => {
            VimEditPlaybackFocusTarget::Close
        }
    }
}

const fn previous_focus(focus: VimEditPlaybackFocusTarget) -> VimEditPlaybackFocusTarget {
    match focus {
        VimEditPlaybackFocusTarget::Close => VimEditPlaybackFocusTarget::ApplyRequested,
        VimEditPlaybackFocusTarget::ApplyRequested => VimEditPlaybackFocusTarget::Diff,
        VimEditPlaybackFocusTarget::Diff => VimEditPlaybackFocusTarget::Timeline,
        VimEditPlaybackFocusTarget::Timeline => VimEditPlaybackFocusTarget::Last,
        VimEditPlaybackFocusTarget::Last => VimEditPlaybackFocusTarget::NextChanged,
        VimEditPlaybackFocusTarget::NextChanged => VimEditPlaybackFocusTarget::Next,
        VimEditPlaybackFocusTarget::Next => VimEditPlaybackFocusTarget::PlayPause,
        VimEditPlaybackFocusTarget::PlayPause => VimEditPlaybackFocusTarget::Previous,
        VimEditPlaybackFocusTarget::Previous => VimEditPlaybackFocusTarget::PreviousChanged,
        VimEditPlaybackFocusTarget::PreviousChanged | VimEditPlaybackFocusTarget::First => {
            VimEditPlaybackFocusTarget::First
        }
    }
}

impl PluginInteraction for VimEditPlaybackInteractionController {
    const KIND: &'static str = VIM_EDIT_PLAYBACK_INTERACTION_KIND;

    type Request = VimEditPlaybackRequest;
    type Snapshot = VimEditPlaybackSnapshot;

    fn new(request: Self::Request) -> Self {
        Self::new(request)
    }

    fn snapshot(&self) -> Self::Snapshot {
        self.snapshot()
    }

    fn handle_input(&mut self, input: InteractionInput) -> InteractionOutput {
        self.handle_input(input)
    }
}
