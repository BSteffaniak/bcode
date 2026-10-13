//! Workflow-owned prose editor composed from the shared input adapter and BMUX.

use bcode_plugin_sdk::tui::{
    FocusedTextFieldAdapter, FocusedTextFieldEdit, FocusedTextFieldEditResult,
    FocusedTextFieldSnapshot,
};
use bmux_text_edit::TextEditBuffer;
use bmux_tui::{
    component::{Component, Constraints, LayoutCx},
    event::Event,
    geometry::Rect,
    paint::{LocalRect, PaintCx},
    style::Style,
};
use bmux_tui_components::text_input::{
    TextInputComponent, TextInputControl, TextInputOutcome, TextInputPolicy, TextInputState,
};
use std::cell::RefCell;

#[derive(Debug, PartialEq, Eq)]
/// Ephemeral workflow prose input retaining BMUX editor state and SDK fences.
pub struct ProseEditor {
    pub(super) state: TextInputState,
    adapter: FocusedTextFieldAdapter,
}

impl ProseEditor {
    pub(super) fn new(field: impl Into<String>, text: impl Into<String>) -> Self {
        let state = TextInputState::new(TextEditBuffer::from_text(text));
        let mut adapter = FocusedTextFieldAdapter::new("bcode.workflow", field);
        adapter.observe_input(&state, true);
        Self { state, adapter }
    }

    pub(super) fn text(&self) -> &str {
        self.state.buffer().text()
    }

    pub(super) fn observe(&mut self, eligible: bool) {
        self.adapter.observe_input(&self.state, eligible);
    }

    pub(super) fn snapshot(&self) -> Option<FocusedTextFieldSnapshot> {
        self.adapter.snapshot_input(&self.state)
    }

    pub(super) fn apply(&mut self, edit: &FocusedTextFieldEdit) -> FocusedTextFieldEditResult {
        self.adapter.apply_input(&mut self.state, edit)
    }

    pub(super) fn event(&mut self, event: &Event) -> TextInputOutcome {
        let result = TextInputControl::new(&TextInputPolicy::chat_composer())
            .handle_event(&mut self.state, event);
        self.observe(true);
        result
    }

    pub(super) fn paint(
        &mut self,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
        style: Style,
        selection: Style,
    ) {
        let retained = RefCell::new(std::mem::take(&mut self.state));
        let policy = TextInputPolicy::chat_composer();
        let component = TextInputComponent::new("workflow.prose", &retained, &policy)
            .focused(true)
            .style(style)
            .selection_style(selection);
        let node = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
        frame.with_child(
            i32::from(area.x),
            i64::from(area.y),
            LocalRect::new(0, 0, area.width, area.height),
            |frame| component.paint(&node, frame),
        );
        let content = area.intersection(frame.clip());
        let anchor = frame
            .cursor()
            .filter(|cursor| cursor.visible)
            .map_or_else(Rect::default, |cursor| {
                Rect::new(cursor.position.x, cursor.position.y, 1, 1)
            });
        self.state = retained.into_inner();
        self.state.set_content_area(content, &policy);
        let visible =
            !content.is_empty() && !anchor.is_empty() && content.intersection(anchor) == anchor;
        self.observe(visible);
        self.adapter.painted_input(&self.state, content, anchor);
    }
}
