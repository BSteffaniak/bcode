//! Deterministic real producer for native/portable frontend acceptance.
use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, FocusedTextFieldAdapter, FocusedTextFieldEdit,
    FocusedTextFieldEditResult, FocusedTextFieldSnapshot, PluginTuiAction, PluginTuiEventResult,
    PluginTuiFocusedTextFieldEditResult, PluginTuiHost, PluginTuiHostError, PluginTuiRegistry,
    PluginTuiSurface, PluginTuiSurfaceFactory, PluginTuiSurfaceFuture, PluginTuiSurfaceOpenRequest,
    PluginWorkflowLookup, PluginWorkflowLookupFuture,
};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::TextEditBuffer;
use bmux_tui::{
    event::Event,
    geometry::Rect,
    hit::HitRegion,
    paint::{LocalRect, PaintCx},
    selection::{SelectionContentId, SelectionFragment, SelectionScope},
    text::{Line, line_viewport},
    text_width::display_width,
};
use std::collections::BTreeMap;

const SOURCE: &str = "H界e\u{301}";

pub(super) fn registry() -> PluginTuiRegistry {
    let mut registry = PluginTuiRegistry::default();
    registry.register_factory(Box::new(ProbeFactory));
    registry
}
struct ProbeFactory;
impl PluginTuiSurfaceFactory for ProbeFactory {
    fn surface_kind(&self) -> &str {
        "hello-probe"
    }
    fn open(&self, _request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        Box::pin(async { Ok(Box::new(Probe::new()) as BoxedPluginTuiSurface) })
    }
}
struct Callback {
    sequence: u64,
    future: PluginWorkflowLookupFuture,
}
struct Probe {
    editor: TextEditBuffer,
    field: FocusedTextFieldAdapter,
    focused: bool,
    sequence: u64,
    callback: Option<Callback>,
}
impl Probe {
    fn new() -> Self {
        Self {
            editor: TextEditBuffer::from_text("@src"),
            field: FocusedTextFieldAdapter::new("hello-probe", "hello.input"),
            focused: true,
            sequence: 0,
            callback: None,
        }
    }
    const fn next_sequence(&mut self) -> Option<u64> {
        let Some(next) = self.sequence.checked_add(1) else {
            return None;
        };
        self.sequence = next;
        Some(next)
    }
    fn paint(&mut self, offset: u16, area: Rect, frame: &mut PaintCx<'_, '_>) {
        // Only these three logical rows exist. BMUX owns all text containment,
        // grapheme boundaries and measured caret/source geometry.
        let editor_visible =
            area.width >= 2 && area.height > 0 && offset <= 1 && 1 - offset < area.height;
        self.field
            .observe(&self.editor, self.focused && editor_visible);
        if area.width == 0 || area.height == 0 {
            return;
        }
        frame.set_focus_scope(Some("hello.modal".into()));
        if offset == 0 {
            let source = TextEditBuffer::from_text(SOURCE);
            let layout = source.wrapped_layout(usize::from(area.width));
            if let (Some(text), Some(range)) = (layout.lines.first(), layout.line_ranges.first()) {
                let line = line_viewport(&Line::from(text.as_str()), 0, usize::from(area.width));
                frame.write_line(LocalRect::new(0, 0, area.width, 1), &line);
                let width = u16::try_from(display_width(text)).unwrap_or(u16::MAX);
                // An oversized indivisible grapheme is clipped by BMUX; never
                // register a misleading full-source fragment for clipped text.
                if width > 0 && width <= area.width {
                    let rect = Rect::new(0, 0, width, 1);
                    frame.push_selection_scope(SelectionScope::new("hello.source", rect));
                    frame.push_selection_fragment(SelectionFragment::new(
                        "hello.source",
                        "hello.source",
                        rect,
                        0,
                        range.clone(),
                    ));
                }
            }
        }
        if offset <= 1 && 1 - offset < area.height && area.width >= 2 {
            let y = 1 - offset;
            let viewport = self.editor.line_viewport(usize::from(area.width - 1));
            frame.write_line(
                LocalRect::new(0, i64::from(y), area.width - 1, 1),
                &Line::from(viewport.text),
            );
            let caret = u16::try_from(viewport.cursor_col)
                .unwrap_or(area.width - 1)
                .min(area.width - 1);
            frame.push_hit(
                HitRegion::new("hello.edit", Rect::new(0, y, area.width, 1)).focusable(true),
            );
            if self.focused {
                frame.set_cursor_local(caret, y, true);
                self.field.observe(&self.editor, true);
                self.field.painted(
                    &self.editor,
                    Rect::new(area.x, area.y.saturating_add(y), area.width, 1),
                    Rect::new(area.x.saturating_add(caret), area.y.saturating_add(y), 1, 1),
                );
            }
        }
        if offset <= 2 && 2 - offset < area.height {
            frame.write_line(
                LocalRect::new(0, i64::from(2 - offset), area.width, 1),
                &line_viewport(
                    &Line::from("e effect / w callback / f focus"),
                    0,
                    usize::from(area.width),
                ),
            );
        }
    }
    fn input(&mut self, event: &Event, host: &dyn PluginTuiHost) -> PluginTuiAction {
        if matches!(event, Event::Tick) {
            return PluginTuiAction::Redraw;
        }
        let Event::Key(KeyStroke {
            key: KeyCode::Char(key),
            modifiers,
        }) = event
        else {
            return PluginTuiAction::None;
        };
        if !modifiers.is_empty() {
            return PluginTuiAction::None;
        }
        if *key == 'f' {
            self.focused = !self.focused;
            self.field.observe(&self.editor, self.focused);
            return PluginTuiAction::Redraw;
        }
        if !matches!(*key, 'e' | 'w') {
            return PluginTuiAction::None;
        }
        // One admitted operation at a time. Repeated keys while pending are
        // consumed without admitting effects or replacing the retained future.
        if self.callback.is_some() {
            return PluginTuiAction::Redraw;
        }
        let Some(sequence) = self.next_sequence() else {
            return diagnostic("sequence_exhausted", self.sequence);
        };
        if *key == 'e' {
            return PluginTuiAction::Close {
                outcome: Some(serde_json::json!({
                    "kind": "hello.effect", "sequence": sequence,
                })),
            };
        }
        self.callback = Some(Callback {
            sequence,
            future: host.associated_workflow(PluginWorkflowLookup {
                owner_plugin_id: "example.hello".to_owned(),
                workflow_kind: "hello.probe".to_owned(),
                scope_key: "parity".to_owned(),
            }),
        });
        PluginTuiAction::Redraw
    }
}
fn diagnostic(status: &str, sequence: u64) -> PluginTuiAction {
    PluginTuiAction::Close {
        outcome: Some(serde_json::json!({
            "kind": "hello.callback", "sequence": sequence, "status": status,
        })),
    }
}
impl PluginTuiSurface for Probe {
    fn id(&self) -> &str {
        "hello-probe"
    }
    fn title(&self) -> &str {
        "Hello portable probe"
    }
    fn preferred_height(&mut self, _width: u16) -> u16 {
        3
    }
    fn focused_row_range(&mut self, width: u16) -> Option<std::ops::Range<u16>> {
        (self.focused && width >= 2).then_some(1..2)
    }
    fn render(&mut self, area: Rect, frame: &mut PaintCx<'_, '_>) {
        self.paint(0, area, frame);
    }
    fn render_slice(
        &mut self,
        _logical_height: u16,
        offset: u16,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
    ) {
        self.paint(offset, area, frame);
    }
    fn retained_selection_text(&self) -> BTreeMap<SelectionContentId, String> {
        BTreeMap::from([("hello.source".into(), SOURCE.to_owned())])
    }
    fn retained_modal_scopes(&self) -> Vec<bmux_tui::focus::FocusScopeId> {
        vec!["hello.modal".into()]
    }
    fn focused_text_field(&self) -> Option<FocusedTextFieldSnapshot> {
        self.field.snapshot(&self.editor)
    }
    fn apply_focused_text_field_edit_result(
        &mut self,
        edit: &FocusedTextFieldEdit,
        _host: &dyn PluginTuiHost,
    ) -> PluginTuiFocusedTextFieldEditResult {
        let edit_result = self.field.apply(&mut self.editor, edit);
        let event = if matches!(edit_result, FocusedTextFieldEditResult::Applied) {
            PluginTuiEventResult::from_legacy(PluginTuiAction::Redraw)
        } else {
            PluginTuiEventResult::consumed()
        };
        PluginTuiFocusedTextFieldEditResult { edit_result, event }
    }
    fn handle_event(&mut self, event: &Event, host: &dyn PluginTuiHost) -> PluginTuiAction {
        self.input(event, host)
    }
    fn drain_effects<'a>(
        &'a mut self,
        _host: &'a dyn PluginTuiHost,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PluginTuiAction> + Send + 'a>> {
        Box::pin(async move {
            let Some(callback) = self.callback.as_mut() else {
                return PluginTuiAction::None;
            };
            // Await by mutable reference: cancelling this observer does not drop
            // the admitted future or permit a second effect. Host retirement must
            // retain the surface until author work has actually settled.
            let result = callback.future.as_mut().await;
            let sequence = callback.sequence;
            self.callback = None;
            let status = match result {
                Ok(Some(_)) => "found",
                Ok(None) => "not_found",
                Err(PluginTuiHostError::PermissionDenied(_)) => "permission_denied",
                Err(PluginTuiHostError::Unsupported(_)) => "unsupported",
                Err(PluginTuiHostError::InvalidRequest(_)) => "invalid_request",
                Err(PluginTuiHostError::Internal(_)) => "internal",
            };
            diagnostic(status, sequence)
        })
    }
}
