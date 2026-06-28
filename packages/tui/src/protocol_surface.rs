//! Generic host for BMUX protocol component-tree surfaces.

use std::collections::BTreeMap;

use bcode_session_models::{InteractiveToolAbortReason, InteractiveToolResolution};
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui_component_protocol::event::{ComponentEvent, ComponentEventKind};
use bmux_tui_component_protocol::ids::ComponentId;
use bmux_tui_component_protocol::model::{ComponentKind, ComponentNode, ComponentTree};
use bmux_tui_component_protocol::state::ComponentRuntimeState;
use bmux_tui_component_protocol::value::ComponentValue;
use bmux_tui_components::protocol::{
    ProtocolBindings, ProtocolRuntime, ProtocolTree, bmux_component_bindings,
};
use serde_json::{Value, json};

/// Generic interactive BMUX protocol surface kind.
pub(crate) const BMUX_PROTOCOL_INLINE_SURFACE: &str = "bmux.protocol.inline";
/// Generic read-only BMUX protocol surface kind.
pub(crate) const BMUX_PROTOCOL_INLINE_READONLY_SURFACE: &str = "bmux.protocol.inline.readonly";
/// Number of transcript rows before an inline protocol surface starts.
pub(crate) const INLINE_PROTOCOL_SURFACE_ROW_OFFSET: usize = 1;
/// Default transcript rows reserved for inline protocol surfaces.
pub(crate) const INLINE_PROTOCOL_SURFACE_HEIGHT: u16 = 12;

/// Runtime state for one generic BMUX protocol surface.
pub(crate) struct ProtocolSurfaceState {
    interaction_id: String,
    tree: ComponentTree,
    runtime: ProtocolRuntime,
    bindings: ProtocolBindings,
}

impl ProtocolSurfaceState {
    /// Create a generic surface from a serialized component-tree request.
    #[must_use]
    pub(crate) fn from_request(
        interaction_id: impl Into<String>,
        request_json: &str,
    ) -> Option<Self> {
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
    pub(crate) fn interaction_id(&self) -> &str {
        &self.interaction_id
    }

    /// Return a user-dismissed resolution for host-level cancellation.
    #[must_use]
    pub(crate) fn dismissed_resolution() -> InteractiveToolResolution {
        user_dismissed()
    }

    /// Render the component tree.
    pub(crate) fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        ProtocolTree::new(&self.tree, &self.bindings).render_runtime(
            area,
            &mut self.runtime,
            frame,
        );
    }

    /// Handle an input event and return a close resolution when submitted or cancelled.
    pub(crate) fn handle_event(
        &mut self,
        area: Rect,
        event: &Event,
    ) -> Option<InteractiveToolResolution> {
        let events = ProtocolTree::new(&self.tree, &self.bindings).handle_event_runtime(
            area,
            &mut self.runtime,
            event,
        );
        resolution_from_events(&self.tree, &events, &self.runtime)
            .or_else(|| escape_resolution(event))
    }
}

/// Read-only rendering state for a resolved generic protocol surface.
pub(crate) struct ResolvedProtocolSurface {
    tree: ComponentTree,
    runtime: ProtocolRuntime,
    bindings: ProtocolBindings,
}

impl ResolvedProtocolSurface {
    /// Create a read-only presentation from a serialized resolution JSON payload.
    #[must_use]
    pub(crate) fn from_resolution_json(resolution_json: &str) -> Option<Self> {
        let resolution = serde_json::from_str::<Value>(resolution_json).ok()?;
        let payload = resolution.get("payload")?;
        let presentation = payload.get("presentation").unwrap_or(payload);
        let surface_kind = presentation.get("surface_kind")?.as_str()?;
        if surface_kind != BMUX_PROTOCOL_INLINE_READONLY_SURFACE {
            return None;
        }
        let mut tree =
            serde_json::from_value::<ComponentTree>(presentation.get("request")?.clone()).ok()?;
        make_readonly(&mut tree.root);
        let values = presentation
            .get("values")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let state = ComponentRuntimeState {
            focus: Default::default(),
            values: serde_json::from_value::<BTreeMap<ComponentId, ComponentValue>>(values).ok()?,
            expanded: Default::default(),
            selected: Default::default(),
        };
        Some(Self {
            tree,
            runtime: ProtocolRuntime::from_state(state),
            bindings: bmux_component_bindings(),
        })
    }

    /// Render the resolved protocol tree.
    pub(crate) fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        ProtocolTree::new(&self.tree, &self.bindings).render_runtime(
            area,
            &mut self.runtime,
            frame,
        );
    }
}

/// Return whether a resolution contains a read-only protocol presentation.
#[must_use]
pub(crate) fn has_readonly_protocol_presentation(resolution_json: &str) -> bool {
    ResolvedProtocolSurface::from_resolution_json(resolution_json).is_some()
}

fn resolution_from_events(
    tree: &ComponentTree,
    events: &[ComponentEvent],
    runtime: &ProtocolRuntime,
) -> Option<InteractiveToolResolution> {
    for event in events {
        match &event.kind {
            ComponentEventKind::Action { action } if action.as_str() == "submit" => {
                return Some(protocol_submitted(tree, events, runtime));
            }
            ComponentEventKind::Action { action } if action.as_str() == "cancel" => {
                return Some(user_dismissed());
            }
            ComponentEventKind::Submit => return Some(protocol_submitted(tree, events, runtime)),
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
    tree: &ComponentTree,
    events: &[ComponentEvent],
    runtime: &ProtocolRuntime,
) -> InteractiveToolResolution {
    InteractiveToolResolution::Submitted {
        payload: json!({
            "status": "submitted",
            "events": events,
            "values": runtime.state().values,
            "presentation": {
                "surface_kind": BMUX_PROTOCOL_INLINE_READONLY_SURFACE,
                "request": tree,
                "values": runtime.state().values,
            },
        }),
    }
}

fn user_dismissed() -> InteractiveToolResolution {
    InteractiveToolResolution::Aborted {
        reason: InteractiveToolAbortReason::UserDismissed,
        message: None,
    }
}

fn make_readonly(node: &mut ComponentNode) {
    node.children.retain(|child| !is_action_row(child));
    for child in &mut node.children {
        make_readonly(child);
    }
    match &mut node.kind {
        ComponentKind::TextInput { disabled, .. }
        | ComponentKind::TextArea { disabled, .. }
        | ComponentKind::RadioGroup { disabled, .. }
        | ComponentKind::CheckboxGroup { disabled, .. }
        | ComponentKind::Select { disabled, .. }
        | ComponentKind::Button { disabled, .. } => *disabled = true,
        ComponentKind::Component { props, .. }
        | ComponentKind::Extension { payload: props, .. } => {
            if let ComponentValue::Map(map) = props {
                map.insert("disabled".to_owned(), ComponentValue::Bool(true));
            }
        }
        ComponentKind::Text { .. }
        | ComponentKind::Markdown { .. }
        | ComponentKind::Stack { .. }
        | ComponentKind::Panel { .. }
        | ComponentKind::Divider
        | ComponentKind::Spacer { .. }
        | ComponentKind::Form { .. }
        | ComponentKind::Status { .. } => {}
    }
}

fn is_action_row(node: &ComponentNode) -> bool {
    match &node.kind {
        ComponentKind::Component { type_id, .. } => type_id.as_str() == "bmux.action_row",
        ComponentKind::Extension { kind, .. } => kind == "bmux.action_row",
        ComponentKind::Button { .. }
        | ComponentKind::Text { .. }
        | ComponentKind::Markdown { .. }
        | ComponentKind::Stack { .. }
        | ComponentKind::Panel { .. }
        | ComponentKind::Divider
        | ComponentKind::Spacer { .. }
        | ComponentKind::TextInput { .. }
        | ComponentKind::TextArea { .. }
        | ComponentKind::RadioGroup { .. }
        | ComponentKind::CheckboxGroup { .. }
        | ComponentKind::Select { .. }
        | ComponentKind::Form { .. }
        | ComponentKind::Status { .. } => false,
    }
}
