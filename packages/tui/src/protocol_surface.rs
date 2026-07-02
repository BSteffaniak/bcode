//! Generic host for BMUX protocol component-tree surfaces.

use bcode_session_models::{InteractiveToolAbortReason, InteractiveToolResolution};
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui_component_protocol::event::{ComponentEvent, ComponentEventKind};
use bmux_tui_component_protocol::model::ComponentTree;
use bmux_tui_components::protocol::{
    ProtocolBindings, ProtocolRuntime, ProtocolTree, bmux_component_bindings,
};
use serde_json::json;

/// Generic interactive BMUX protocol surface kind.
pub const BMUX_PROTOCOL_INLINE_SURFACE: &str = "bmux.protocol.inline";
/// Number of transcript rows before an inline protocol surface starts.
pub const INLINE_PROTOCOL_SURFACE_ROW_OFFSET: usize = 1;

/// Runtime state for one generic BMUX protocol surface.
pub struct ProtocolSurfaceState {
    interaction_id: String,
    tree: ComponentTree,
    runtime: ProtocolRuntime,
    bindings: ProtocolBindings,
}

impl ProtocolSurfaceState {
    /// Create a generic surface from a serialized component-tree request.
    #[must_use]
    pub fn from_request(interaction_id: impl Into<String>, request_json: &str) -> Option<Self> {
        let tree = serde_json::from_str::<ComponentTree>(request_json).ok()?;
        Some(Self {
            interaction_id: interaction_id.into(),
            tree,
            runtime: ProtocolRuntime::new(),
            bindings: bmux_component_bindings(),
        })
    }

    /// Return the interaction id associated with this surface.
    #[must_use]
    pub fn interaction_id(&self) -> &str {
        &self.interaction_id
    }

    /// Return a user-dismissed resolution for host-level cancellation.
    #[must_use]
    pub const fn dismissed_resolution() -> InteractiveToolResolution {
        user_dismissed()
    }

    /// Render the component tree.
    pub fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        ProtocolTree::new(&self.tree, &self.bindings).render_runtime(
            area,
            &mut self.runtime,
            frame,
        );
    }

    /// Handle an input event and return a close resolution when submitted or cancelled.
    pub fn handle_event(&mut self, area: Rect, event: &Event) -> Option<InteractiveToolResolution> {
        let events = ProtocolTree::new(&self.tree, &self.bindings).handle_event_runtime(
            area,
            &mut self.runtime,
            event,
        );
        resolution_from_events(&events, &self.runtime).or_else(|| escape_resolution(event))
    }
}

/// Estimate a serialized protocol tree's rendered height.
#[must_use]
pub fn measure_tree_json_height(tree_json: &str, width: u16) -> u16 {
    serde_json::from_str::<ComponentTree>(tree_json)
        .ok()
        .map_or(1, |tree| {
            ProtocolTree::new(&tree, &bmux_component_bindings()).measure_height(width)
        })
}

fn resolution_from_events(
    events: &[ComponentEvent],
    runtime: &ProtocolRuntime,
) -> Option<InteractiveToolResolution> {
    for event in events {
        match &event.kind {
            ComponentEventKind::Action { action } if action.as_str() == "submit" => {
                return Some(protocol_submitted(events, runtime));
            }
            ComponentEventKind::Action { action } if action.as_str() == "cancel" => {
                return Some(user_dismissed());
            }
            ComponentEventKind::Submit => return Some(protocol_submitted(events, runtime)),
            ComponentEventKind::Cancel => return Some(user_dismissed()),
            ComponentEventKind::ValueChanged { .. }
            | ComponentEventKind::FocusChanged { .. }
            | ComponentEventKind::Action { .. }
            | ComponentEventKind::Extension { .. } => {}
        }
    }
    None
}

fn escape_resolution(event: &Event) -> Option<InteractiveToolResolution> {
    let Event::Key(stroke) = event else {
        return None;
    };
    (stroke.key == bmux_keyboard::KeyCode::Escape).then(user_dismissed)
}

fn protocol_submitted(
    events: &[ComponentEvent],
    runtime: &ProtocolRuntime,
) -> InteractiveToolResolution {
    InteractiveToolResolution::Submitted {
        payload: json!({
            "status": "submitted",
            "events": events,
            "values": runtime.state().values,
        }),
    }
}

const fn user_dismissed() -> InteractiveToolResolution {
    InteractiveToolResolution::Aborted {
        reason: InteractiveToolAbortReason::UserDismissed,
        message: None,
    }
}
