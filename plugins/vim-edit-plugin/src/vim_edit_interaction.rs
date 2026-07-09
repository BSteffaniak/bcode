//! Renderer-neutral Vim edit playback interaction controller.

use bcode_plugin_sdk::interaction::PluginInteraction;
use bcode_tool::{
    InteractionControlId, InteractionInput, InteractionNavigation, InteractionOutput,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::VIM_EDIT_PLAYBACK_INTERACTION_KIND;

/// Focus target for Vim edit playback controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum VimEditPlaybackFocusTarget {
    /// Previous frame control.
    Previous,
    /// Next frame control.
    Next,
    /// Toggle timeline control.
    Timeline,
    /// Toggle diff control.
    Diff,
    /// Close playback control.
    Close,
}

impl VimEditPlaybackFocusTarget {
    /// Return stable control id.
    #[must_use]
    pub fn control_id(self) -> InteractionControlId {
        match self {
            Self::Previous => InteractionControlId::new("previous"),
            Self::Next => InteractionControlId::new("next"),
            Self::Timeline => InteractionControlId::new("timeline"),
            Self::Diff => InteractionControlId::new("diff"),
            Self::Close => InteractionControlId::new("close"),
        }
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
            focus: VimEditPlaybackFocusTarget::Next,
        }
    }

    fn frame_count(&self) -> usize {
        self.request
            .playback
            .get("events")
            .or_else(|| self.request.playback.get("frames"))
            .and_then(Value::as_array)
            .map_or(0, Vec::len)
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

    fn activate(&mut self, control_id: &InteractionControlId) -> InteractionOutput {
        match control_id.as_str() {
            "previous" => {
                self.select_previous();
                InteractionOutput::Redraw
            }
            "next" => {
                self.select_next();
                InteractionOutput::Redraw
            }
            "timeline" => {
                self.show_timeline = !self.show_timeline;
                InteractionOutput::Redraw
            }
            "diff" => {
                self.show_diff = !self.show_diff;
                InteractionOutput::Redraw
            }
            "close" => InteractionOutput::Submitted {
                payload: serde_json::json!({ "action": "closed" }),
            },
            _ => InteractionOutput::None,
        }
    }

    const fn navigate(&mut self, direction: InteractionNavigation) {
        self.focus = match direction {
            InteractionNavigation::Next => match self.focus {
                VimEditPlaybackFocusTarget::Previous => VimEditPlaybackFocusTarget::Next,
                VimEditPlaybackFocusTarget::Next => VimEditPlaybackFocusTarget::Timeline,
                VimEditPlaybackFocusTarget::Timeline => VimEditPlaybackFocusTarget::Diff,
                VimEditPlaybackFocusTarget::Diff | VimEditPlaybackFocusTarget::Close => {
                    VimEditPlaybackFocusTarget::Close
                }
            },
            InteractionNavigation::Previous => match self.focus {
                VimEditPlaybackFocusTarget::Close => VimEditPlaybackFocusTarget::Diff,
                VimEditPlaybackFocusTarget::Diff => VimEditPlaybackFocusTarget::Timeline,
                VimEditPlaybackFocusTarget::Timeline => VimEditPlaybackFocusTarget::Next,
                VimEditPlaybackFocusTarget::Next | VimEditPlaybackFocusTarget::Previous => {
                    VimEditPlaybackFocusTarget::Previous
                }
            },
        };
    }

    fn snapshot(&self) -> VimEditPlaybackSnapshot {
        VimEditPlaybackSnapshot {
            playback: self.request.playback.clone(),
            selected_frame: self.selected_frame,
            show_timeline: self.show_timeline,
            show_diff: self.show_diff,
            focus: self.focus,
            focused_control_id: self.focus.control_id(),
        }
    }

    fn handle_input(&mut self, input: InteractionInput) -> InteractionOutput {
        match input {
            InteractionInput::Activate { control_id } => self.activate(&control_id),
            InteractionInput::Focus { control_id } => {
                self.focus = match control_id.as_str() {
                    "previous" => VimEditPlaybackFocusTarget::Previous,
                    "next" => VimEditPlaybackFocusTarget::Next,
                    "timeline" => VimEditPlaybackFocusTarget::Timeline,
                    "diff" => VimEditPlaybackFocusTarget::Diff,
                    "close" => VimEditPlaybackFocusTarget::Close,
                    _ => self.focus,
                };
                InteractionOutput::Redraw
            }
            InteractionInput::Navigate { direction } => {
                self.navigate(direction);
                InteractionOutput::Redraw
            }
            InteractionInput::Submit => self.activate(&self.focus.control_id()),
            InteractionInput::Cancel => InteractionOutput::Cancelled,
            InteractionInput::Change { .. } | InteractionInput::Blur { .. } => {
                InteractionOutput::None
            }
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
