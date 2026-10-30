//! Native TUI renderer for the interactive question tool.

#[cfg(feature = "static-bundled")]
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::presentation::{QuestionInputPort, QuestionInputResult as TerminalInteractionInput};
#[cfg(all(test, feature = "static-bundled"))]
use bcode_plugin_sdk::composer::ComposerTextRange;
#[cfg(feature = "static-bundled")]
use bcode_plugin_sdk::composer::ComposerValidationError;
#[cfg(feature = "static-bundled")]
use bcode_plugin_sdk::tui::{
    FocusedTextFieldAdapter, FocusedTextFieldEdit, FocusedTextFieldEditResult,
    FocusedTextFieldSnapshot, PluginTuiTheme, TerminalFocusedTextFieldEditResult,
    TerminalInteractionRenderer,
};

use bcode_tool::{InteractionControlId, InteractionInput, InteractionNavigation, InteractionValue};
use bmux_keyboard::KeyCode;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::event::{Event, MouseButton, MouseEventKind};
#[cfg(all(test, feature = "static-bundled"))]
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui::text_width::wrap_text_with_continuation;
use bmux_tui_components::action_row::{
    ActionButton, ActionRow, ActionRowComponent, ActionRowState, ActionRowStyles,
};
use bmux_tui_components::key_hint_bar::{KeyHint, KeyHintBarComponent, KeyHintBarStyles};
use bmux_tui_components::text_input::{TextInputControl, TextInputPolicy, TextInputState};
use bmux_tui_components::text_input_box::{TextInputBoxComponent, TextInputBoxPolicy};

use super::QuestionSelectionMode;
use super::question_interaction::{
    QuestionFocusTarget, QuestionSnapshot, custom_control_id, option_control_id,
};
#[cfg(feature = "static-bundled")]
use super::{QUESTION_INLINE_SURFACE, question_interaction::QuestionInteractionController};

const QUESTION_SCOPE: &str = "question";

#[cfg(feature = "static-bundled")]
pub fn register_keymap(registry: &mut bcode_plugin_sdk::tui::PluginTuiRegistry) {
    use bcode_plugin_sdk::tui::keymap::KeymapAction;
    for (action, description, defaults) in [
        ("next", "Next question control", vec!["tab"]),
        ("previous", "Previous question control", vec!["shift+tab"]),
        ("up", "Move to previous row", vec!["up"]),
        ("down", "Move to next row", vec!["down"]),
        ("left", "Move to previous option", vec!["left"]),
        ("right", "Move to next option", vec!["right"]),
        ("activate", "Activate question control", vec!["enter"]),
        ("toggle", "Toggle option", vec!["space"]),
        ("cancel", "Dismiss question", vec!["esc"]),
        ("submit", "Submit answers", vec!["ctrl+enter"]),
    ] {
        registry.register_keymap_action(KeymapAction {
            id: format!("question.{action}"),
            scope: QUESTION_SCOPE.into(),
            description: description.into(),
            defaults: defaults.into_iter().map(str::to_owned).collect(),
        });
    }
    for (action, description, defaults) in [
        ("undo", "Undo custom answer edit", vec!["ctrl+z"]),
        (
            "redo",
            "Redo custom answer edit",
            vec!["ctrl+y", "ctrl+shift+z"],
        ),
    ] {
        registry.register_keymap_action(KeymapAction {
            id: format!("question.text.{action}"),
            scope: "question.text".into(),
            description: description.into(),
            defaults: defaults.into_iter().map(str::to_owned).collect(),
        });
    }
    registry.register_keymap_action(
        bcode_plugin_sdk::tui::keymap::controls::ControlAction::Newline.definition("question.text"),
    );
    for action in bcode_plugin_sdk::tui::keymap::controls::ControlAction::TEXT {
        registry.register_keymap_action(action.definition("question.text"));
    }
    for index in 0..10 {
        registry.register_keymap_action(KeymapAction {
            id: format!("question.option.{index}"),
            scope: QUESTION_SCOPE.into(),
            description: format!("Select option {}", index + 1),
            defaults: vec![if index == 9 {
                "0".into()
            } else {
                (index + 1).to_string()
            }],
        });
    }
}

const DESCRIPTION_INDENT: &str = "        ";

/// Terminal renderer for the question interaction.
#[derive(Default)]
pub struct QuestionTerminalRenderer {
    option_keys: Vec<String>,
    action_keys: Vec<String>,
    last_area: Rect,
    controls: Vec<ControlRegion>,
    logical_origin: u16,
    custom_inputs: BTreeMap<usize, TextInputState>,
    #[cfg(feature = "static-bundled")]
    custom_fields: RefCell<BTreeMap<usize, FocusedTextFieldAdapter>>,
    custom_areas: BTreeMap<usize, Rect>,
    visible_custom_carets: BTreeSet<usize>,
    pending_custom_mouse_focus: Option<usize>,
    theme: QuestionSurfaceTheme,
    measurement: Option<QuestionMeasurement>,
    #[cfg(all(test, feature = "static-bundled"))]
    rendered_questions: usize,
    #[cfg(all(test, feature = "static-bundled"))]
    visited_question_spans: usize,
    wrapped_rows_visited: std::cell::Cell<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QuestionMeasurement {
    layout_revision: u64,
    focus: QuestionFocusTarget,
    width: u16,
    height: u16,
    focused_rows: std::ops::Range<u16>,
    title_rows: std::ops::Range<u16>,
    global_validation_rows: Option<std::ops::Range<u16>>,
    questions: Vec<QuestionGeometry>,
    action_rows: std::ops::Range<u16>,
    hint_rows: std::ops::Range<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QuestionLayout {
    height: u16,
    title_rows: std::ops::Range<u16>,
    global_validation_rows: Option<std::ops::Range<u16>>,
    questions: Vec<QuestionGeometry>,
    action_rows: std::ops::Range<u16>,
    hint_rows: std::ops::Range<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QuestionGeometry {
    rows: std::ops::Range<u16>,
    prompt: WrappedText,
    options: Vec<OptionGeometry>,
    custom_rows: Option<std::ops::Range<u16>>,
    validation_rows: Option<std::ops::Range<u16>>,
    spacer_rows: std::ops::Range<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OptionGeometry {
    rows: std::ops::Range<u16>,
    label: WrappedText,
    description: Option<WrappedText>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WrappedText {
    chunks: Arc<[String]>,
}

#[derive(Debug, Clone, Copy)]
struct QuestionSurfaceTheme {
    text: Style,
    muted: Style,
    focused: Style,
    selection: Style,
    error: Style,
    canvas: Style,
}

impl Default for QuestionSurfaceTheme {
    fn default() -> Self {
        Self {
            text: Style::new(),
            muted: Style::new().fg(Color::BrightBlack),
            focused: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            selection: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            error: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            canvas: Style::new(),
        }
    }
}

impl QuestionSurfaceTheme {
    fn from_component_theme(theme: Option<bmux_tui_components::theme::ComponentTheme>) -> Self {
        theme.map_or_else(Self::default, |theme| Self {
            text: theme.text,
            muted: theme.muted,
            focused: theme.focused,
            selection: theme.selected,
            error: theme.error,
            canvas: theme.canvas,
        })
    }
    #[cfg(feature = "static-bundled")]
    fn resolve(theme: Option<&PluginTuiTheme>) -> Self {
        Self::from_component_theme(theme.and_then(PluginTuiTheme::component_theme))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ControlRegion {
    area: Rect,
    control_id: InteractionControlId,
}

impl QuestionTerminalRenderer {
    pub(crate) fn set_component_theme(
        &mut self,
        theme: Option<bmux_tui_components::theme::ComponentTheme>,
    ) {
        self.theme = QuestionSurfaceTheme::from_component_theme(theme);
    }
    pub(crate) fn height(&mut self, snapshot: &QuestionSnapshot, width: u16) -> u16 {
        self.measurement(snapshot, width).height
    }
    pub(crate) fn focus_rows(
        &mut self,
        snapshot: &QuestionSnapshot,
        width: u16,
    ) -> std::ops::Range<u16> {
        self.measurement(snapshot, width).focused_rows.clone()
    }
    pub(crate) fn paint_slice(
        &mut self,
        snapshot: &QuestionSnapshot,
        offset: u16,
        area: Rect,
        cx: &mut PaintCx<'_, '_>,
    ) {
        self.logical_origin = offset;
        self.render_snapshot(snapshot, area, cx);
        self.logical_origin = 0;
    }
    fn sync_custom_input(&mut self, question_index: usize, value: &str) -> &mut TextInputState {
        if self
            .custom_inputs
            .get(&question_index)
            .is_none_or(|state| state.buffer().text() != value)
        {
            let mut state = TextInputState::new(TextEditBuffer::from_text(value));
            state
                .buffer_mut()
                .move_cursor(bmux_text_edit::TextMotion::End);
            self.custom_inputs.insert(question_index, state);
            #[cfg(feature = "static-bundled")]
            {
                let fields = self.custom_fields.get_mut();
                if let Some(field) = fields.get_mut(&question_index) {
                    field.reset();
                } else {
                    fields.insert(
                        question_index,
                        FocusedTextFieldAdapter::new(
                            QUESTION_INLINE_SURFACE,
                            format!("answer.{question_index}"),
                        ),
                    );
                }
            }
        }
        self.custom_inputs
            .get_mut(&question_index)
            .expect("custom editor synchronized")
    }

    #[cfg(not(feature = "static-bundled"))]
    const fn observe_field(&self, _snapshot: &QuestionSnapshot) {
        // Native caret observation exists only in the static-bundled adapter.
        let _ = self;
    }

    #[cfg(feature = "static-bundled")]
    fn observe_field(&self, snapshot: &QuestionSnapshot) {
        for (question_index, field) in self.custom_fields.borrow_mut().iter_mut() {
            if let Some(state) = self.custom_inputs.get(question_index) {
                let eligible = snapshot.focus
                    == (QuestionFocusTarget::Custom {
                        question_index: *question_index,
                    })
                    && snapshot
                        .request
                        .questions
                        .get(*question_index)
                        .is_some_and(|question| question.options.is_empty() || question.custom)
                    && self.visible_custom_carets.contains(question_index)
                    && snapshot.answers.get(*question_index).is_some_and(|answer| {
                        answer.custom.as_deref().unwrap_or_default() == state.buffer().text()
                    });
                field.observe_input(state, eligible);
            }
        }
    }

    pub(crate) fn refresh_keymap(&mut self, host: &dyn QuestionInputPort) -> bool {
        let option_keys = (0..10)
            .map(|index| {
                host.keymap_keys(QUESTION_SCOPE, &format!("question.option.{index}"))
                    .join("/")
            })
            .collect::<Vec<_>>();
        let action_keys: Vec<String> = [
            vec!["next", "down", "right"],
            vec!["previous", "up", "left"],
            vec!["activate", "toggle"],
            vec!["cancel"],
        ]
        .map(|actions| {
            actions
                .into_iter()
                .flat_map(|action| host.keymap_keys(QUESTION_SCOPE, &format!("question.{action}")))
                .collect::<Vec<_>>()
                .join("/")
        })
        .into();
        let changed = self.option_keys != option_keys || self.action_keys != action_keys;
        if changed {
            self.option_keys = option_keys;
            self.action_keys = action_keys;
            self.measurement = None;
        }
        changed
    }

    fn option_shortcut_label(&self, option_index: usize) -> String {
        self.option_keys
            .get(option_index)
            .filter(|keys| !keys.is_empty())
            .cloned()
            .unwrap_or_else(|| "-".into())
    }

    fn measurement(&mut self, snapshot: &QuestionSnapshot, width: u16) -> &QuestionMeasurement {
        let width = width.max(1);
        let layout_changed = self.measurement.as_ref().is_none_or(|measurement| {
            measurement.layout_revision != snapshot.layout_revision || measurement.width != width
        });
        if layout_changed {
            let geometry = self.content_geometry(snapshot, width);
            let focused_rows = Self::focused_content_range(
                snapshot,
                &geometry.questions,
                geometry.action_rows.start,
            );
            self.measurement = Some(QuestionMeasurement {
                layout_revision: snapshot.layout_revision,
                focus: snapshot.focus,
                width,
                height: geometry.height,
                focused_rows,
                title_rows: geometry.title_rows,
                global_validation_rows: geometry.global_validation_rows,
                questions: geometry.questions,
                action_rows: geometry.action_rows,
                hint_rows: geometry.hint_rows,
            });
        } else if self
            .measurement
            .as_ref()
            .is_some_and(|measurement| measurement.focus != snapshot.focus)
        {
            let focused_rows = {
                let measurement = self.measurement.as_ref().expect("measured above");
                Self::focused_content_range(
                    snapshot,
                    &measurement.questions,
                    measurement.action_rows.start,
                )
            };
            let measurement = self.measurement.as_mut().expect("measured above");
            measurement.focus = snapshot.focus;
            measurement.focused_rows = focused_rows;
        }
        self.measurement.as_ref().expect("measurement initialized")
    }

    fn render_line(&self, frame: &mut PaintCx<'_, '_>, content_y: &mut u16, line: &Line) {
        if let Some(screen_y) = self.screen_y(*content_y) {
            frame.write_line(
                LocalRect::terminal(Rect::new(
                    self.last_area.x,
                    screen_y,
                    self.last_area.width,
                    1,
                )),
                line,
            );
        }
        *content_y = content_y.saturating_add(1);
    }

    fn screen_y(&self, content_y: u16) -> Option<u16> {
        let visible_y = content_y.checked_sub(self.logical_origin)?;
        (visible_y < self.last_area.height).then(|| self.last_area.y.saturating_add(visible_y))
    }

    fn control_area(&self, content_y: u16, height: u16) -> Option<Rect> {
        let first_y = content_y.max(self.logical_origin);
        let last_y = content_y
            .saturating_add(height)
            .min(self.logical_origin.saturating_add(self.last_area.height));
        (first_y < last_y).then(|| {
            Rect::new(
                self.last_area.x,
                self.last_area
                    .y
                    .saturating_add(first_y.saturating_sub(self.logical_origin)),
                self.last_area.width,
                last_y.saturating_sub(first_y),
            )
        })
    }

    fn render_wrapped(
        &self,
        frame: &mut PaintCx<'_, '_>,
        content_y: &mut u16,
        wrapped: &WrappedText,
        first_prefix: &str,
        continuation_prefix: &str,
        style: Style,
    ) {
        for (index, chunk) in wrapped.chunks.iter().enumerate() {
            self.wrapped_rows_visited
                .set(self.wrapped_rows_visited.get().saturating_add(1));
            let prefix = if index == 0 {
                first_prefix
            } else {
                continuation_prefix
            };
            self.render_line(
                frame,
                content_y,
                &Line::from_spans(vec![
                    Span::raw(prefix.to_owned()),
                    Span::styled(chunk.clone(), style),
                ]),
            );
        }
    }

    fn render_title(&self, frame: &mut PaintCx<'_, '_>, content_y: &mut u16) {
        self.render_line(
            frame,
            content_y,
            &Line::from_spans(vec![Span::styled(
                "Question",
                self.theme.focused.add_modifier(Modifier::BOLD),
            )]),
        );
    }

    fn render_question(
        &mut self,
        frame: &mut PaintCx<'_, '_>,
        content_y: &mut u16,
        snapshot: &QuestionSnapshot,
        question_index: usize,
        geometry: &QuestionGeometry,
    ) {
        #[cfg(all(test, feature = "static-bundled"))]
        {
            self.rendered_questions = self.rendered_questions.saturating_add(1);
        }
        let question = &snapshot.request.questions[question_index];
        debug_assert_eq!(
            geometry.validation_rows.is_some(),
            snapshot.invalid_question_index == Some(question_index)
        );
        debug_assert_eq!(
            geometry
                .spacer_rows
                .end
                .saturating_sub(geometry.spacer_rows.start),
            1
        );
        self.render_wrapped(frame, content_y, &geometry.prompt, "", "", self.theme.text);
        for option_index in 0..question.options.len() {
            let option_geometry = &geometry.options[option_index];
            let start_y = *content_y;
            let option_id = option_control_id(question_index, option_index);
            let selected = snapshot.selected_option_indices[question_index].contains(&option_index);
            let marker = if question.selection_mode == QuestionSelectionMode::Multiple {
                if selected { "[x]" } else { "[ ]" }
            } else if selected {
                "(*)"
            } else {
                "( )"
            };
            let shortcut = self.option_shortcut_label(option_index);
            let shortcut_width = bmux_tui::text_width::display_width(&shortcut);
            let focused = matches!(
                snapshot.focus,
                QuestionFocusTarget::Option {
                    question_index: focused_question,
                    option_index: focused_option,
                } if focused_question == question_index && focused_option == option_index
            );
            let focus_marker = if focused { '>' } else { ' ' };
            let prefix = format!("{focus_marker} {shortcut}. {marker} ");
            let continuation = " ".repeat(7_usize.saturating_add(shortcut_width));
            self.render_wrapped(
                frame,
                content_y,
                &option_geometry.label,
                &prefix,
                &continuation,
                option_style(&self.theme, focused, selected),
            );
            if let Some(description) = option_geometry.description.as_ref() {
                self.render_wrapped(
                    frame,
                    content_y,
                    description,
                    DESCRIPTION_INDENT,
                    DESCRIPTION_INDENT,
                    self.theme.muted,
                );
            }
            if let Some(area) = self.control_area(start_y, content_y.saturating_sub(start_y)) {
                self.controls.push(ControlRegion {
                    area,
                    control_id: option_id,
                });
            }
        }
        self.render_custom_answer(frame, content_y, snapshot, question_index);
        if snapshot.invalid_question_index == Some(question_index) {
            self.render_line(
                frame,
                content_y,
                &Line::from_spans(vec![
                    Span::raw("  "),
                    Span::styled(
                        "An answer is required.",
                        self.theme.error.add_modifier(Modifier::BOLD),
                    ),
                ]),
            );
        }
        self.render_line(frame, content_y, &Line::from(""));
    }

    fn custom_answer_height(&self, question_index: usize) -> u16 {
        self.measurement
            .as_ref()
            .and_then(|measurement| measurement.questions.get(question_index))
            .and_then(|geometry| geometry.custom_rows.as_ref())
            .map_or_else(
                || {
                    self.custom_inputs
                        .get(&question_index)
                        .map_or(3, |state| custom_input_height(state, self.last_area.width))
                },
                range_height,
            )
    }

    fn render_custom_answer(
        &mut self,
        frame: &mut PaintCx<'_, '_>,
        content_y: &mut u16,
        snapshot: &QuestionSnapshot,
        question_index: usize,
    ) {
        let question = &snapshot.request.questions[question_index];
        if !question.options.is_empty() && !question.custom {
            return;
        }
        let label = if question.options.is_empty() {
            "Answer"
        } else {
            "Custom answer"
        };
        let value = snapshot.answers[question_index]
            .custom
            .as_deref()
            .unwrap_or_default();
        let start_y = *content_y;
        let input_height = self.custom_answer_height(question_index);
        self.sync_custom_input(question_index, value);
        *content_y = content_y.saturating_add(input_height);
        let control_id = custom_control_id(question_index);
        if let Some(area) = self.control_area(start_y, input_height) {
            let state = self
                .custom_inputs
                .get_mut(&question_index)
                .expect("custom input initialized above");
            state.set_content_area(area, &TextInputPolicy::chat_composer());
            let retained = std::cell::RefCell::new(state.clone());
            let input_id = format!("question.answer.{question_index}");
            let input = TextInputBoxComponent::new(
                input_id.clone(),
                TextInputPolicy::chat_composer(),
                &retained,
            )
            .label(label)
            .policy(TextInputBoxPolicy {
                field_chrome: true,
                panel_chrome: false,
                background: false,
                cursor: true,
                focused: matches!(
                    snapshot.focus,
                    QuestionFocusTarget::Custom { question_index: focused }
                        if focused == question_index
                ),
                disabled: false,
                min_rows: area.height,
                max_rows: Some(area.height),
            })
            .styles(question_input_styles(&self.theme));
            let layout = input.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
            let content = layout.find_rect(&bmux_tui::component::LayoutId::new(format!(
                "{input_id}.input"
            )));
            frame.with_child(
                i32::from(area.x),
                i64::from(area.y),
                LocalRect::new(0, 0, area.width, area.height),
                |cx| input.paint(&layout, cx),
            );
            *state = retained.into_inner();
            let Some(local) = content else {
                return;
            };
            state.set_content_area(
                Rect::new(
                    area.x.saturating_add(local.x),
                    area.y.saturating_add(local.y),
                    local.width,
                    local.height,
                ),
                &TextInputPolicy::chat_composer(),
            );
            #[cfg(feature = "static-bundled")]
            if matches!(snapshot.focus, QuestionFocusTarget::Custom { question_index: focused } if focused == question_index)
                && let Some((x, y)) = frame.logical_cursor()
                && let (Ok(x), Ok(y)) = (u16::try_from(x), u16::try_from(y))
                && let Some(field) = self.custom_fields.get_mut().get_mut(&question_index)
            {
                field.observe_input(state, true);
                field.painted_input(
                    state,
                    state.content_area().intersection(area),
                    Rect::new(x, y, 1, 1),
                );
                if field.snapshot_input(state).is_some() {
                    self.visible_custom_carets.insert(question_index);
                }
            }
            self.custom_areas.insert(question_index, area);
            self.controls.push(ControlRegion { area, control_id });
        }
    }

    fn render_actions(
        &mut self,
        frame: &mut PaintCx<'_, '_>,
        content_y: &mut u16,
        snapshot: &QuestionSnapshot,
    ) {
        let actions = [
            ActionButton::new("submit", "Submit"),
            ActionButton::new("cancel", "Cancel"),
        ];
        let focused = usize::from(snapshot.focus == QuestionFocusTarget::Cancel);
        if let Some(area) = self.control_area(*content_y, 1) {
            let row = ActionRow::new(&actions).styles(question_action_styles(&self.theme));
            for (index, action_area) in row.action_areas(area).into_iter().enumerate() {
                self.controls.push(ControlRegion {
                    area: action_area,
                    control_id: InteractionControlId::new(actions[index].id.clone()),
                });
            }
            let mut state = ActionRowState::new();
            state.set_focused(Some(focused));
            let state = std::cell::Cell::new(state);
            let component = ActionRowComponent::new("question.actions", &actions, &state)
                .styles(question_action_styles(&self.theme));
            let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
            frame.with_child(
                i32::from(area.x),
                i64::from(area.y),
                LocalRect::new(0, 0, area.width, area.height),
                |cx| component.paint(&layout, cx),
            );
        }
        *content_y = content_y.saturating_add(1);
        let hints = self
            .action_keys
            .iter()
            .zip(["next", "previous", "select", "dismiss"])
            .filter(|(keys, _)| !keys.is_empty())
            .map(|(keys, label)| KeyHint::new(keys.as_str(), label))
            .collect::<Vec<_>>();
        if let Some(area) = self.control_area(*content_y, 1) {
            let hints = KeyHintBarComponent::new("question.hints", &hints)
                .styles(question_hint_styles(&self.theme));
            let layout = hints.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
            frame.with_child(
                i32::from(area.x),
                i64::from(area.y),
                LocalRect::new(0, 0, area.width, area.height),
                |cx| hints.paint(&layout, cx),
            );
        }
        *content_y = content_y.saturating_add(1);
    }

    pub(crate) fn render_snapshot(
        &mut self,
        snapshot: &QuestionSnapshot,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
    ) {
        if let Some(question_index) = self.pending_custom_mouse_focus
            && matches!(
                snapshot.focus,
                QuestionFocusTarget::Custom { question_index: focused }
                    if focused == question_index
            )
        {
            self.pending_custom_mouse_focus = None;
        }
        self.last_area = area;
        self.observe_field(snapshot);
        self.wrapped_rows_visited.set(0);
        self.controls.clear();
        self.custom_areas.clear();
        self.visible_custom_carets.clear();
        #[cfg(feature = "static-bundled")]
        for (question_index, field) in self.custom_fields.get_mut().iter_mut() {
            // Invalidate the previous presentation before a possibly hidden/clipped paint.
            if let Some(state) = self.custom_inputs.get(question_index) {
                field.painted_input(state, Rect::default(), Rect::default());
            }
        }
        #[cfg(all(test, feature = "static-bundled"))]
        {
            self.rendered_questions = 0;
            self.visited_question_spans = 0;
        }
        let mut content_y = 0;
        self.render_title(frame, &mut content_y);
        if let Some(error) = &snapshot.validation_error {
            self.render_line(
                frame,
                &mut content_y,
                &Line::from_spans(vec![Span::styled(error, self.theme.error)]),
            );
        }
        let visible_start = self.logical_origin;
        let visible_end = visible_start.saturating_add(area.height);
        let (first_question, questions, action_rows, hint_rows) = {
            let measurement = self.measurement(snapshot, area.width);
            debug_assert_eq!(measurement.title_rows, 0..1);
            debug_assert_eq!(
                measurement.global_validation_rows.is_some(),
                snapshot.validation_error.is_some()
            );
            let first = measurement
                .questions
                .partition_point(|geometry| geometry.rows.end <= visible_start);
            let count = measurement.questions[first..]
                .partition_point(|geometry| geometry.rows.start < visible_end);
            (
                first,
                measurement.questions[first..first.saturating_add(count)].to_vec(),
                measurement.action_rows.clone(),
                measurement.hint_rows.clone(),
            )
        };
        #[cfg(all(test, feature = "static-bundled"))]
        {
            self.visited_question_spans = questions.len();
        }
        let mut content_y = questions
            .first()
            .map_or(visible_start, |geometry| geometry.rows.start);
        for (relative_index, geometry) in questions.into_iter().enumerate() {
            let question_index = first_question.saturating_add(relative_index);
            self.render_question(frame, &mut content_y, snapshot, question_index, &geometry);
        }
        content_y = action_rows.start;
        if action_rows.end > visible_start
            && hint_rows.end > visible_start
            && action_rows.start < visible_end
        {
            self.render_actions(frame, &mut content_y, snapshot);
        }
        self.observe_field(snapshot);
    }

    fn custom_vertical_input(
        &mut self,
        key: KeyCode,
        snapshot: &QuestionSnapshot,
    ) -> TerminalInteractionInput {
        let QuestionFocusTarget::Custom { question_index } = snapshot.focus else {
            return TerminalInteractionInput::Ignored;
        };
        let Some(state) = self.custom_inputs.get_mut(&question_index) else {
            return TerminalInteractionInput::Semantic(InteractionInput::Navigate {
                direction: if key == KeyCode::Up {
                    InteractionNavigation::Previous
                } else {
                    InteractionNavigation::Next
                },
            });
        };
        let width = usize::from(state.content_area().width.max(1));
        let layout = state.buffer().wrapped_layout(width);
        let target_row = match key {
            KeyCode::Up if layout.cursor.row > 0 => Some(layout.cursor.row.saturating_sub(1)),
            KeyCode::Down if layout.cursor.row.saturating_add(1) < layout.lines.len() => {
                Some(layout.cursor.row.saturating_add(1))
            }
            KeyCode::Up | KeyCode::Down => None,
            _ => return TerminalInteractionInput::Ignored,
        };
        let Some(target_row) = target_row else {
            return TerminalInteractionInput::Semantic(InteractionInput::Navigate {
                direction: if key == KeyCode::Up {
                    InteractionNavigation::Previous
                } else {
                    InteractionNavigation::Next
                },
            });
        };
        state
            .buffer_mut()
            .move_cursor_to_wrapped_position(width, target_row, layout.cursor.col);
        state.sync_scroll_to_cursor(&TextInputPolicy::chat_composer());
        TerminalInteractionInput::Consumed
    }

    fn custom_input(
        &mut self,
        event: &Event,
        snapshot: &QuestionSnapshot,
        host: &dyn QuestionInputPort,
    ) -> TerminalInteractionInput {
        let QuestionFocusTarget::Custom { question_index } = snapshot.focus else {
            return TerminalInteractionInput::Ignored;
        };
        let value = snapshot.answers[question_index]
            .custom
            .as_deref()
            .unwrap_or_default();
        let state = self.sync_custom_input(question_index, value);
        let before = state.buffer().text().to_owned();
        match event {
            Event::Key(stroke) => {
                if !host.edit_key(state, *stroke) {
                    return TerminalInteractionInput::Ignored;
                }
            }
            Event::Paste(text) => {
                TextInputControl::new(&TextInputPolicy::chat_composer()).handle_paste(state, text);
            }
            Event::Mouse(mouse) => {
                TextInputControl::new(&TextInputPolicy::chat_composer())
                    .handle_mouse(state, *mouse);
            }
            Event::Resize(_) | Event::Focus(_) | Event::Tick | Event::User(_) => {
                return TerminalInteractionInput::Ignored;
            }
        }
        let text = state.buffer().text();
        if text == before {
            TerminalInteractionInput::Consumed
        } else {
            TerminalInteractionInput::Semantic(InteractionInput::Change {
                control_id: custom_control_id(question_index),
                value: InteractionValue::String(text.to_owned()),
            })
        }
    }

    fn mouse_input(&self, event: &bmux_tui::event::MouseEvent) -> Option<InteractionInput> {
        if !matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
            return None;
        }
        self.controls
            .iter()
            .find(|control| control.area.contains(event.position))
            .map(|control| InteractionInput::Activate {
                control_id: control.control_id.clone(),
            })
    }

    fn content_geometry(&mut self, snapshot: &QuestionSnapshot, width: u16) -> QuestionLayout {
        let width = usize::from(width.max(1));
        let title_rows = 0_u16..1;
        let mut height = title_rows.end;
        let global_validation_rows = if snapshot.validation_error.is_some() {
            let start = height;
            height = height.saturating_add(1);
            Some(start..height)
        } else {
            None
        };
        let mut questions = Vec::with_capacity(snapshot.request.questions.len());
        for (question_index, question) in snapshot.request.questions.iter().enumerate() {
            let question_start = height;
            let required = if question.required { " *" } else { "" };
            let prompt = question.header.as_ref().map_or_else(
                || format!("{}{required}", question.text),
                |header| format!("{header}{required}: {}", question.text),
            );
            let prompt = WrappedText::new(&prompt, width, width);
            height = height.saturating_add(prompt.height());
            let mut options = Vec::with_capacity(question.options.len());
            for (option_index, option) in question.options.iter().enumerate() {
                let option_start = height;
                let prefix_width = 7_usize.saturating_add(bmux_tui::text_width::display_width(
                    &self.option_shortcut_label(option_index),
                ));
                let available = width.saturating_sub(prefix_width).max(1);
                let label = WrappedText::new(&option.label, available, available);
                height = height.saturating_add(label.height());
                let description = option.description.as_deref().map(|description| {
                    let description_width = width
                        .saturating_sub(bmux_tui::text_width::display_width(DESCRIPTION_INDENT))
                        .max(1);
                    WrappedText::new(description, description_width, description_width)
                });
                if let Some(description) = &description {
                    height = height.saturating_add(description.height());
                }
                options.push(OptionGeometry {
                    rows: option_start..height,
                    label,
                    description,
                });
            }
            let custom_rows = if question.options.is_empty() || question.custom {
                let value = snapshot.answers[question_index]
                    .custom
                    .as_deref()
                    .unwrap_or_default();
                let state = self.sync_custom_input(question_index, value);
                let start = height;
                height = height.saturating_add(custom_input_height(
                    state,
                    u16::try_from(width).unwrap_or(u16::MAX),
                ));
                Some(start..height)
            } else {
                None
            };
            let validation_rows = if snapshot.invalid_question_index == Some(question_index) {
                let start = height;
                height = height.saturating_add(1);
                Some(start..height)
            } else {
                None
            };
            let spacer_rows = height..height.saturating_add(1);
            height = spacer_rows.end;
            questions.push(QuestionGeometry {
                rows: question_start..height,
                prompt,
                options,
                custom_rows,
                validation_rows,
                spacer_rows,
            });
        }
        let action_rows = height..height.saturating_add(1);
        let hint_rows = action_rows.end..action_rows.end.saturating_add(1);
        QuestionLayout {
            height: hint_rows.end,
            title_rows,
            global_validation_rows,
            questions,
            action_rows,
            hint_rows,
        }
    }

    fn focused_content_range(
        snapshot: &QuestionSnapshot,
        questions: &[QuestionGeometry],
        actions_row: u16,
    ) -> std::ops::Range<u16> {
        match snapshot.focus {
            QuestionFocusTarget::Option {
                question_index,
                option_index,
            } => questions
                .get(question_index)
                .and_then(|question| question.options.get(option_index))
                .map(|option| option.rows.clone())
                .unwrap_or(0..1),
            QuestionFocusTarget::Custom { question_index } => questions
                .get(question_index)
                .and_then(|question| question.custom_rows.clone())
                .unwrap_or(0..1),
            QuestionFocusTarget::Submit | QuestionFocusTarget::Cancel => {
                actions_row..actions_row.saturating_add(1)
            }
        }
    }
}

#[cfg(feature = "static-bundled")]
impl TerminalInteractionRenderer<QuestionInteractionController> for QuestionTerminalRenderer {
    const SURFACE_KIND: &'static str = QUESTION_INLINE_SURFACE;

    fn id(&self) -> &'static str {
        "question-inline"
    }

    fn title(&self) -> &'static str {
        "Question"
    }

    fn preferred_height(&mut self, snapshot: &QuestionSnapshot, width: u16) -> u16 {
        self.measurement(snapshot, width).height
    }

    fn render(&mut self, snapshot: &QuestionSnapshot, area: Rect, frame: &mut PaintCx<'_, '_>) {
        self.theme = QuestionSurfaceTheme::default();
        self.render_snapshot(snapshot, area, frame);
    }

    fn render_slice(
        &mut self,
        snapshot: &QuestionSnapshot,
        logical_height: u16,
        logical_row_offset: u16,
        destination: Rect,
        frame: &mut PaintCx<'_, '_>,
    ) {
        self.logical_origin = logical_row_offset.min(logical_height.saturating_sub(1));
        self.render(snapshot, destination, frame);
        self.logical_origin = 0;
    }

    fn focused_row_range(
        &mut self,
        snapshot: &QuestionSnapshot,
        width: u16,
    ) -> Option<std::ops::Range<u16>> {
        Some(self.measurement(snapshot, width).focused_rows.clone())
    }

    fn render_with_theme(
        &mut self,
        snapshot: &QuestionSnapshot,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
        theme: Option<&PluginTuiTheme>,
    ) {
        self.theme = QuestionSurfaceTheme::resolve(theme);
        self.render_snapshot(snapshot, area, frame);
    }

    fn work_shape(&self) -> bcode_plugin_sdk::tui::PluginTuiWorkShape {
        bcode_plugin_sdk::tui::PluginTuiWorkShape {
            wrapped_rows_visited: self.wrapped_rows_visited.get(),
            ..bcode_plugin_sdk::tui::PluginTuiWorkShape::default()
        }
    }

    fn prepare_keymap(&mut self, host: &dyn bcode_plugin_sdk::tui::PluginTuiHost) -> bool {
        self.refresh_keymap(&crate::presentation::NativeQuestionPort(host))
    }

    fn prepare_render(&mut self, host: &dyn bcode_plugin_sdk::tui::PluginTuiHost) {
        self.refresh_keymap(&crate::presentation::NativeQuestionPort(host));
    }

    fn input(
        &mut self,
        event: &Event,
        snapshot: &QuestionSnapshot,
        host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
    ) -> bcode_plugin_sdk::tui::TerminalInteractionInput {
        self.observe_field(snapshot);
        let result = self.route_input(
            event,
            snapshot,
            &crate::presentation::NativeQuestionPort(host),
            false,
        );
        self.observe_field(snapshot);
        result
    }

    fn focused_text_field_event(
        &mut self,
        event: &Event,
        snapshot: &QuestionSnapshot,
        host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
    ) -> bcode_plugin_sdk::tui::TerminalInteractionInput {
        self.observe_field(snapshot);
        if !matches!(snapshot.focus, QuestionFocusTarget::Custom { .. }) {
            return bcode_plugin_sdk::tui::TerminalInteractionInput::Ignored;
        }
        let result = self.route_input(
            event,
            snapshot,
            &crate::presentation::NativeQuestionPort(host),
            true,
        );
        self.observe_field(snapshot);
        result
    }

    fn focused_text_field(&self, snapshot: &QuestionSnapshot) -> Option<FocusedTextFieldSnapshot> {
        self.observe_field(snapshot);
        let QuestionFocusTarget::Custom { question_index } = snapshot.focus else {
            return None;
        };
        self.custom_fields
            .borrow()
            .get(&question_index)?
            .snapshot_input(self.custom_inputs.get(&question_index)?)
    }

    fn apply_focused_text_field_edit(
        &mut self,
        edit: &FocusedTextFieldEdit,
        snapshot: &QuestionSnapshot,
        host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
    ) -> bcode_plugin_sdk::tui::TerminalInteractionInput {
        self.apply_focused_text_field_edit_result(edit, snapshot, host)
            .input
    }

    fn apply_focused_text_field_edit_result(
        &mut self,
        edit: &FocusedTextFieldEdit,
        snapshot: &QuestionSnapshot,
        _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
    ) -> TerminalFocusedTextFieldEditResult {
        self.observe_field(snapshot);
        let rejected = || TerminalFocusedTextFieldEditResult {
            edit_result: FocusedTextFieldEditResult::Rejected(ComposerValidationError::Stale),
            input: bcode_plugin_sdk::tui::TerminalInteractionInput::Consumed,
        };
        let QuestionFocusTarget::Custom { question_index } = snapshot.focus else {
            return rejected();
        };
        let Some(state) = self.custom_inputs.get_mut(&question_index) else {
            return rejected();
        };
        let Some(field) = self.custom_fields.get_mut().get_mut(&question_index) else {
            return rejected();
        };
        let edit_result = field.apply_input(state, edit);
        let input = if edit_result == FocusedTextFieldEditResult::Applied {
            state.sync_scroll_to_cursor(&TextInputPolicy::chat_composer());
            bcode_plugin_sdk::tui::TerminalInteractionInput::Semantic(InteractionInput::Change {
                control_id: custom_control_id(question_index),
                value: InteractionValue::String(state.buffer().text().to_owned()),
            })
        } else {
            bcode_plugin_sdk::tui::TerminalInteractionInput::Consumed
        };
        TerminalFocusedTextFieldEditResult { edit_result, input }
    }
}

impl QuestionTerminalRenderer {
    pub(crate) fn route_input(
        &mut self,
        event: &Event,
        snapshot: &QuestionSnapshot,
        host: &dyn QuestionInputPort,
        route_editor: bool,
    ) -> TerminalInteractionInput {
        self.refresh_keymap(host);
        if matches!(snapshot.focus, QuestionFocusTarget::Custom { .. }) && !route_editor {
            return TerminalInteractionInput::Ignored;
        }
        if let Event::Mouse(mouse) = event
            && let Some(question_index) =
                self.custom_areas.iter().find_map(|(question_index, area)| {
                    area.contains(mouse.position).then_some(*question_index)
                })
            && snapshot.focus != (QuestionFocusTarget::Custom { question_index })
        {
            if let Some(state) = self.custom_inputs.get_mut(&question_index) {
                TextInputControl::new(&TextInputPolicy::chat_composer())
                    .handle_mouse(state, *mouse);
            }
            self.pending_custom_mouse_focus = Some(question_index);
            return TerminalInteractionInput::Semantic(InteractionInput::Focus {
                control_id: custom_control_id(question_index),
            });
        }
        if let Event::Key(stroke) = event
            && host.keymap_action(QUESTION_SCOPE, *stroke).as_deref() == Some("question.submit")
            && matches!(
                snapshot.focus,
                QuestionFocusTarget::Custom { .. } | QuestionFocusTarget::Submit
            )
        {
            return TerminalInteractionInput::Semantic(InteractionInput::Submit);
        }
        let action = match event {
            Event::Key(stroke) => host.keymap_action(QUESTION_SCOPE, *stroke),
            _ => None,
        };
        if matches!(snapshot.focus, QuestionFocusTarget::Custom { .. }) {
            if let Some(action) = action.as_deref() {
                if action == "question.down" || action == "question.up" {
                    return self.custom_vertical_input(
                        if action == "question.up" {
                            KeyCode::Up
                        } else {
                            KeyCode::Down
                        },
                        snapshot,
                    );
                }
                if action == "question.next" || action == "question.previous" {
                    return TerminalInteractionInput::Semantic(InteractionInput::Navigate {
                        direction: if action == "question.next" {
                            InteractionNavigation::Next
                        } else {
                            InteractionNavigation::Previous
                        },
                    });
                }
                if action == "question.activate" {
                    return TerminalInteractionInput::Semantic(InteractionInput::Submit);
                }
                if action == "question.cancel" {
                    return TerminalInteractionInput::Semantic(InteractionInput::Cancel);
                }
            }
            return self.custom_input(event, snapshot, host);
        }
        let input = match event {
            Event::Mouse(mouse) => self.mouse_input(mouse),
            Event::Key(_) => question_action_input(action.as_deref(), snapshot),
            _ => None,
        };
        input.map_or(
            TerminalInteractionInput::Ignored,
            TerminalInteractionInput::Semantic,
        )
    }
}

fn question_action_input(
    action: Option<&str>,
    snapshot: &QuestionSnapshot,
) -> Option<InteractionInput> {
    match action? {
        "question.next" | "question.down" | "question.right" => Some(InteractionInput::Navigate {
            direction: InteractionNavigation::Next,
        }),
        "question.previous" | "question.up" | "question.left" => Some(InteractionInput::Navigate {
            direction: InteractionNavigation::Previous,
        }),
        "question.activate" | "question.toggle" => Some(InteractionInput::Activate {
            control_id: snapshot.focused_control_id.clone(),
        }),
        "question.cancel" => Some(InteractionInput::Cancel),
        "question.submit" => Some(InteractionInput::Submit),
        action => {
            let index = action
                .strip_prefix("question.option.")?
                .parse::<usize>()
                .ok()?;
            let QuestionFocusTarget::Option { question_index, .. } = snapshot.focus else {
                return None;
            };
            (index < snapshot.request.questions[question_index].options.len()).then(|| {
                InteractionInput::Activate {
                    control_id: option_control_id(question_index, index),
                }
            })
        }
    }
}

fn custom_input_height(state: &TextInputState, width: u16) -> u16 {
    TextInputControl::new(&TextInputPolicy::chat_composer())
        .visible_rows_for_width(state, width.saturating_sub(2).max(1))
        .clamp(1, 6)
        .saturating_add(2)
}

impl WrappedText {
    fn new(text: &str, first_width: usize, continuation_width: usize) -> Self {
        Self {
            chunks: wrap_text_with_continuation(
                text,
                first_width.max(1),
                continuation_width.max(1),
            )
            .into(),
        }
    }

    fn height(&self) -> u16 {
        u16::try_from(self.chunks.len()).unwrap_or(u16::MAX)
    }
}

const fn range_height(range: &std::ops::Range<u16>) -> u16 {
    range.end.saturating_sub(range.start)
}

const fn option_style(theme: &QuestionSurfaceTheme, focused: bool, selected: bool) -> Style {
    let style = if selected {
        theme.selection.add_modifier(Modifier::BOLD)
    } else {
        theme.text
    };
    if focused {
        style.add_modifier(Modifier::REVERSED)
    } else {
        style
    }
}

const fn question_action_styles(theme: &QuestionSurfaceTheme) -> ActionRowStyles {
    ActionRowStyles {
        normal: theme.text,
        focused: theme.focused,
        hovered: theme.focused,
        pressed: theme.selection,
        disabled: theme.muted,
    }
}

const fn question_hint_styles(theme: &QuestionSurfaceTheme) -> KeyHintBarStyles {
    KeyHintBarStyles {
        key: theme.focused,
        label: theme.text,
        separator: theme.muted,
        disabled: theme.muted,
        background: theme.canvas,
    }
}

const fn question_input_styles(
    theme: &QuestionSurfaceTheme,
) -> bmux_tui_components::text_input_box::TextInputBoxStyles {
    bmux_tui_components::text_input_box::TextInputBoxStyles {
        text: theme.text,
        focused_text: theme.focused,
        disabled_text: theme.muted,
        placeholder: theme.muted,
        selection: theme.selection,
        border: theme.muted,
        focused_border: theme.focused,
        background: theme.canvas,
        focused_background: theme.canvas,
        disabled_background: theme.canvas,
    }
}

#[cfg(all(test, feature = "static-bundled"))]
mod tests {
    fn routed_surface_input(
        surface: &mut dyn bcode_plugin_sdk::tui::PluginTuiSurface,
        event: &Event,
    ) -> bcode_plugin_sdk::tui::PluginTuiEventResult {
        let result = surface.focused_text_field_event(event, &TEST_HOST);
        if result.outcome == bmux_tui::event::EventOutcome::Ignored {
            surface.handle_event_result(event, &TEST_HOST)
        } else {
            result
        }
    }

    fn routed_input(
        renderer: &mut QuestionTerminalRenderer,
        event: &Event,
        snapshot: &QuestionSnapshot,
        host: &dyn QuestionInputPort,
    ) -> TerminalInteractionInput {
        let input = renderer.focused_text_field_event(event, snapshot, host);
        if matches!(input, TerminalInteractionInput::Ignored) {
            renderer.input(event, snapshot, host)
        } else {
            input
        }
    }

    use super::*;
    use bcode_plugin_sdk::tui::keymap::{KeymapRegistry, controls::ControlAction};
    use bcode_plugin_sdk::tui::{
        PluginTask, PluginTuiDiffTheme, PluginTuiHost, PluginTuiSourceTheme, PluginTuiSyntaxColor,
        PluginTuiSyntaxTheme,
    };
    use bcode_tool::{InteractionInput, InteractionOutput};
    use bmux_keyboard::{KeyStroke, Modifiers};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::geometry::Point;

    use crate::{
        NormalizedQuestionRequest, Question, QuestionControl, QuestionCustomMode, QuestionOption,
    };

    #[derive(Debug, Default)]
    struct TestHost;

    impl PluginTuiHost for TestHost {
        fn spawn(&self, _task: PluginTask) {}

        fn spawn_blocking(&self, _task: Box<dyn FnOnce() + Send + 'static>) {}

        fn request_redraw(&self) {}

        fn keymap_action(&self, scope: &str, stroke: KeyStroke) -> Option<String> {
            test_registry()
                .action_for_key(scope, stroke)
                .map(str::to_owned)
        }
        fn keymap_keys(&self, scope: &str, action: &str) -> Vec<String> {
            test_registry()
                .keys_for_action(scope, action)
                .into_iter()
                .map(str::to_owned)
                .collect()
        }
    }

    struct RemappedHost(KeymapRegistry);
    impl PluginTuiHost for RemappedHost {
        fn spawn(&self, _task: PluginTask) {}
        fn spawn_blocking(&self, _task: Box<dyn FnOnce() + Send + 'static>) {}
        fn request_redraw(&self) {}
        fn keymap_action(&self, scope: &str, stroke: KeyStroke) -> Option<String> {
            self.0.action_for_key(scope, stroke).map(str::to_owned)
        }
        fn keymap_is_overridden(&self, scope: &str, stroke: KeyStroke) -> bool {
            self.0.is_overridden_key(scope, stroke)
        }
        fn keymap_keys(&self, scope: &str, action: &str) -> Vec<String> {
            self.0
                .keys_for_action(scope, action)
                .into_iter()
                .map(str::to_owned)
                .collect()
        }
    }

    #[test]
    fn remapping_and_unbinding_question_actions_changes_dispatch_and_labels() {
        let mut registry = test_registry();
        registry
            .configure(BTreeMap::from([(
                "question".into(),
                BTreeMap::from([
                    ("ctrl+j".into(), "question.down".into()),
                    ("x".into(), "question.option.0".into()),
                    ("esc".into(), String::new()),
                ]),
            )]))
            .unwrap();
        let host = RemappedHost(registry);
        let controller = QuestionInteractionController::new(NormalizedQuestionRequest {
            questions: vec![question("Pick", &[("A", None), ("B", None)], false, false)],
        });
        let snapshot = controller.snapshot();
        let mut renderer = QuestionTerminalRenderer::default();
        assert!(matches!(
            routed_input(&mut renderer, &key(KeyCode::Down), &snapshot, &host),
            TerminalInteractionInput::Ignored
        ));
        assert!(matches!(
            routed_input(&mut renderer, &key(KeyCode::Escape), &snapshot, &host),
            TerminalInteractionInput::Ignored
        ));
        let event = Event::Key(bmux_keyboard::parse_key_stroke("ctrl+j").unwrap());
        assert!(matches!(
            routed_input(&mut renderer, &event, &snapshot, &host),
            TerminalInteractionInput::Semantic(InteractionInput::Navigate {
                direction: InteractionNavigation::Next
            })
        ));
        assert!(matches!(
            routed_input(&mut renderer, &key(KeyCode::Char('1')), &snapshot, &host),
            TerminalInteractionInput::Ignored
        ));
        assert!(matches!(
            routed_input(&mut renderer, &key(KeyCode::Char('x')), &snapshot, &host),
            TerminalInteractionInput::Semantic(InteractionInput::Activate { .. })
        ));
        assert_eq!(renderer.option_shortcut_label(0), "x");
        assert_eq!(renderer.action_keys[3], "");
    }

    #[test]
    fn preparing_unchanged_bindings_retains_question_measurement() {
        let controller = QuestionInteractionController::new(NormalizedQuestionRequest {
            questions: vec![question("Pick", &[("An option label", None)], false, false)],
        });
        let snapshot = controller.snapshot();
        let mut renderer = QuestionTerminalRenderer::default();
        assert!(renderer.prepare_keymap(&TEST_HOST));
        let original_height = renderer.preferred_height(&snapshot, 24);
        let original_measurement = renderer.measurement.clone();
        assert!(!renderer.prepare_keymap(&TEST_HOST));
        assert_eq!(renderer.measurement, original_measurement);

        let mut registry = test_registry();
        registry
            .configure(BTreeMap::from([(
                "question".into(),
                BTreeMap::from([("ctrl+alt+shift+f12".into(), "question.option.0".into())]),
            )]))
            .unwrap();
        let host = RemappedHost(registry);
        assert!(renderer.prepare_keymap(&host));
        assert!(renderer.measurement.is_none());
        assert!(renderer.preferred_height(&snapshot, 24) > original_height);
        let remapped_measurement = renderer.measurement.clone();
        assert!(!renderer.prepare_keymap(&host));
        assert_eq!(renderer.measurement, remapped_measurement);
    }

    #[test]
    fn remapped_long_option_hint_measures_and_paints_narrow_geometry_consistently() {
        let mut registry = test_registry();
        registry
            .configure(BTreeMap::from([(
                "question".into(),
                BTreeMap::from([("ctrl+alt+shift+f12".into(), "question.option.0".into())]),
            )]))
            .unwrap();
        let host = RemappedHost(registry);
        let controller = QuestionInteractionController::new(NormalizedQuestionRequest {
            questions: vec![question(
                "Pick",
                &[("A long option label that wraps", None)],
                false,
                false,
            )],
        });
        let snapshot = controller.snapshot();
        let mut renderer = QuestionTerminalRenderer::default();
        renderer.prepare_render(&host);
        assert_eq!(renderer.option_shortcut_label(0), "ctrl+alt+shift+f12");
        for width in [0, 1, 8, 24, 40] {
            let height = renderer.preferred_height(&snapshot, width);
            let geometry = renderer.content_geometry(&snapshot, width);
            assert_eq!(height, geometry.height);
            assert!(geometry.questions[0].options[0].rows.end <= height);
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);
            let mut backend = Frame::new(&mut buffer);
            renderer.render_with_host(
                &snapshot,
                area,
                &mut PaintCx::new(&mut backend),
                None,
                &host,
            );
            assert!(
                renderer
                    .controls
                    .iter()
                    .all(|control| control.area.intersection(area) == control.area)
            );
        }
    }

    fn test_registry() -> KeymapRegistry {
        let mut definitions = bcode_plugin_sdk::tui::PluginTuiRegistry::default();
        register_keymap(&mut definitions);
        let mut registry = KeymapRegistry::default();
        for action in definitions.keymap_actions() {
            registry.register(action.clone()).unwrap();
        }
        for action in ControlAction::TEXT {
            registry
                .register(action.definition("question.text"))
                .unwrap();
        }
        registry
    }

    const TEST_HOST: TestHost = TestHost;

    fn question(
        text: &str,
        options: &[(&str, Option<&str>)],
        custom: bool,
        required: bool,
    ) -> Question {
        Question {
            header: None,
            text: text.to_owned(),
            options: options
                .iter()
                .map(|(label, description)| QuestionOption {
                    label: (*label).to_owned(),
                    value: Some((*label).to_owned()),
                    description: description.map(str::to_owned),
                })
                .collect(),
            control: QuestionControl::Radio,
            selection_mode: QuestionSelectionMode::Single,
            custom,
            custom_mode: QuestionCustomMode::Additional,
            required,
        }
    }

    pub(crate) fn render_snapshot(
        renderer: &mut QuestionTerminalRenderer,
        snapshot: &QuestionSnapshot,
        area: Rect,
    ) -> Buffer {
        let mut buffer = Buffer::empty(area);
        let mut backend = Frame::new(&mut buffer);
        let mut frame = PaintCx::new(&mut backend);
        renderer.prepare_render(&TEST_HOST);
        renderer.render(snapshot, area, &mut frame);
        buffer
    }

    fn render_snapshot_with_theme(
        renderer: &mut QuestionTerminalRenderer,
        snapshot: &QuestionSnapshot,
        area: Rect,
        theme: &PluginTuiTheme,
    ) -> Buffer {
        let mut buffer = Buffer::empty(area);
        let mut backend = Frame::new(&mut buffer);
        let mut frame = PaintCx::new(&mut backend);
        renderer.prepare_render(&TEST_HOST);
        renderer.render_with_theme(snapshot, area, &mut frame, Some(theme));
        buffer
    }

    fn rendered_text(buffer: &Buffer) -> String {
        (0..buffer.area().height)
            .filter_map(|row| buffer.row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn key(key: KeyCode) -> Event {
        Event::Key(KeyStroke {
            key,
            modifiers: Modifiers::NONE,
        })
    }

    fn shifted_character(character: char) -> Event {
        Event::Key(KeyStroke {
            key: KeyCode::Char(character),
            modifiers: Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
        })
    }

    fn shifted_key(key: KeyCode) -> Event {
        Event::Key(KeyStroke {
            key,
            modifiers: Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
        })
    }

    fn apply_event(
        renderer: &mut QuestionTerminalRenderer,
        controller: &mut QuestionInteractionController,
        event: &Event,
    ) -> InteractionOutput {
        let snapshot = controller.snapshot();
        match routed_input(renderer, event, &snapshot, &TEST_HOST) {
            TerminalInteractionInput::Ignored => InteractionOutput::None,
            TerminalInteractionInput::Consumed => InteractionOutput::Redraw,
            TerminalInteractionInput::Semantic(input) => controller.handle_input(input),
        }
    }

    fn render_then_apply_event(
        renderer: &mut QuestionTerminalRenderer,
        controller: &mut QuestionInteractionController,
        event: &Event,
    ) -> InteractionOutput {
        let snapshot = controller.snapshot();
        let _ = render_snapshot(renderer, &snapshot, Rect::new(0, 0, 48, 12));
        match routed_input(renderer, event, &snapshot, &TEST_HOST) {
            TerminalInteractionInput::Ignored => InteractionOutput::None,
            TerminalInteractionInput::Consumed => InteractionOutput::Redraw,
            TerminalInteractionInput::Semantic(input) => controller.handle_input(input),
        }
    }

    fn completion_edit(field: FocusedTextFieldSnapshot, replacement: &str) -> FocusedTextFieldEdit {
        FocusedTextFieldEdit {
            edit: bcode_plugin_sdk::composer::ComposerEdit {
                document_id: field.document.document_id.clone(),
                revision: field.document.revision,
                range: ComposerTextRange {
                    start: 0,
                    end: field.document.text.len(),
                },
                expected_text: field.document.text.clone(),
                replacement: replacement.into(),
                cursor: replacement.len(),
            },
            expected: field,
        }
    }

    #[test]
    fn typed_question_owner_completion_consumes_change_without_submitting() {
        use bcode_plugin_sdk::tui::{
            PluginTuiAction, PluginTuiSurfaceFactory, PluginTuiSurfaceOpenRequest,
            TypedTerminalInteractionSurfaceFactory,
        };
        use bmux_tui::event::EventOutcome;
        let request = NormalizedQuestionRequest {
            questions: vec![question("Answer", &[], true, true)],
        };
        let factory = TypedTerminalInteractionSurfaceFactory::<
            QuestionInteractionController,
            QuestionTerminalRenderer,
        >::new();
        let mut opening = factory.open(PluginTuiSurfaceOpenRequest {
            instance_id: "question-completion-test".into(),
            repo_path: None,
            session_id: None,
            target: None,
            options: serde_json::to_value(request).unwrap(),
        });
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        let std::task::Poll::Ready(result) = opening.as_mut().poll(&mut context) else {
            panic!("typed question factory opens synchronously");
        };
        let mut surface = result.unwrap();
        routed_surface_input(surface.as_mut(), &Event::Paste("@sr".into()));
        let area = Rect::new(0, 0, 48, 12);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        surface.render(area, &mut PaintCx::new(&mut frame));
        let field = surface.focused_text_field().unwrap();
        let result = surface
            .apply_focused_text_field_edit(&completion_edit(field, "src/main.rs"), &TEST_HOST);
        assert_ne!(result.outcome, EventOutcome::Ignored);
        assert!(matches!(result.action, PluginTuiAction::Redraw));
        let result = routed_surface_input(surface.as_mut(), &key(KeyCode::Enter));
        assert_ne!(result.outcome, EventOutcome::Ignored);
        let PluginTuiAction::Close {
            outcome: Some(payload),
        } = result.action
        else {
            panic!("actual Enter submits through the typed question owner");
        };
        assert_eq!(payload["questions"][0]["custom"], "src/main.rs");
    }

    #[test]
    fn public_question_surface_retains_logical_caret_under_nested_slice_and_clip() {
        use bcode_plugin_sdk::tui::{
            PluginTuiSurfaceFactory, PluginTuiSurfaceOpenRequest,
            TypedTerminalInteractionSurfaceFactory,
        };
        let factory = TypedTerminalInteractionSurfaceFactory::<
            QuestionInteractionController,
            QuestionTerminalRenderer,
        >::new();
        let mut opening = factory.open(PluginTuiSurfaceOpenRequest {
            instance_id: "transformed-question".into(),
            repo_path: None,
            session_id: None,
            target: None,
            options: serde_json::to_value(NormalizedQuestionRequest {
                questions: vec![question("Answer", &[], true, true)],
            })
            .unwrap(),
        });
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        let std::task::Poll::Ready(result) = opening.as_mut().poll(&mut context) else {
            panic!("typed question factory opens synchronously");
        };
        let mut surface = result.unwrap();
        routed_surface_input(surface.as_mut(), &Event::Paste("@src".into()));
        let extent = surface.preferred_height(48);
        let destination = Rect::new(0, 2, 48, 12);
        let mut baseline = Buffer::empty(Rect::new(0, 0, 80, 30));
        let mut baseline_frame = Frame::new(&mut baseline);
        surface.render_slice(
            extent,
            1,
            destination,
            &mut PaintCx::new(&mut baseline_frame),
        );
        let original = surface.focused_text_field().unwrap();

        let mut transformed = Buffer::empty(Rect::new(0, 0, 80, 30));
        let mut transformed_frame = Frame::new(&mut transformed);
        PaintCx::new(&mut transformed_frame).with_child(
            8,
            7,
            LocalRect::new(0, 0, 48, 20),
            |parent| {
                parent.with_child(0, -2, LocalRect::new(0, 2, 48, 12), |owner| {
                    surface.render_slice(extent, 1, destination, owner);
                });
            },
        );
        let field = surface
            .focused_text_field()
            .expect("painted field remains eligible under translation");
        assert_eq!(field.content_rect, original.content_rect);
        assert_eq!(field.anchor, original.anchor);
        let painted = transformed_frame.cursor().unwrap();
        assert!(painted.visible);
        assert_eq!(
            painted.position,
            Point::new(field.anchor.x + 8, field.anchor.y + 5)
        );

        let edit = completion_edit(field, "src/main.rs");
        let mut clipped = Buffer::empty(Rect::new(0, 0, 80, 30));
        let mut clipped_frame = Frame::new(&mut clipped);
        PaintCx::new(&mut clipped_frame).with_child(8, 7, LocalRect::new(0, 0, 48, 20), |parent| {
            parent.with_child(0, -2, LocalRect::new(0, 2, 48, 1), |owner| {
                surface.render_slice(extent, 1, destination, owner);
            });
        });
        assert!(
            surface.focused_text_field().is_none(),
            "clipped caret cannot expose a stale anchor"
        );
        assert!(clipped_frame.cursor().is_none());
        let rejected = surface.apply_focused_text_field_edit(&edit, &TEST_HOST);
        assert!(!matches!(
            rejected.action,
            bcode_plugin_sdk::tui::PluginTuiAction::Close { .. }
        ));

        surface.render_slice(
            extent,
            1,
            destination,
            &mut PaintCx::new(&mut baseline_frame),
        );
        let restored = surface.focused_text_field().unwrap();
        assert_eq!(restored.document.text, "@src");
        assert!(restored.generation > edit.expected.generation);
        let stale = surface.apply_focused_text_field_edit(&edit, &TEST_HOST);
        assert!(!matches!(
            stale.action,
            bcode_plugin_sdk::tui::PluginTuiAction::Redraw
        ));
        assert_eq!(
            surface.focused_text_field().unwrap().document,
            restored.document
        );
        let accepted = surface
            .apply_focused_text_field_edit(&completion_edit(restored, "src/main.rs"), &TEST_HOST);
        assert!(matches!(
            accepted.action,
            bcode_plugin_sdk::tui::PluginTuiAction::Redraw
        ));
    }

    #[test]
    fn completion_field_lifetimes_distinguish_retained_question_surfaces() {
        let controller = QuestionInteractionController::new(NormalizedQuestionRequest {
            questions: vec![question("Answer", &[], true, false)],
        });
        let snapshot = controller.snapshot();
        let mut first = QuestionTerminalRenderer::default();
        let mut second = QuestionTerminalRenderer::default();
        let area = Rect::new(0, 0, 48, 12);
        render_snapshot(&mut first, &snapshot, area);
        render_snapshot(&mut second, &snapshot, area);
        let originating = first.focused_text_field(&snapshot).unwrap();
        let current = second.focused_text_field(&snapshot).unwrap();
        assert_ne!(originating.identity.lifetime, current.identity.lifetime);
        let before = second.custom_inputs[&0].clone();
        assert!(matches!(
            second.apply_focused_text_field_edit(
                &completion_edit(originating, "path"),
                &snapshot,
                &TEST_HOST,
            ),
            TerminalInteractionInput::Consumed
        ));
        assert_eq!(second.custom_inputs[&0], before);
    }

    #[test]
    fn custom_completion_preserves_live_history_and_submits_inserted_answer() {
        let mut controller = QuestionInteractionController::new(NormalizedQuestionRequest {
            questions: vec![question("Answer", &[], true, true)],
        });
        let mut renderer = QuestionTerminalRenderer::default();
        apply_event(&mut renderer, &mut controller, &Event::Paste("@sr".into()));
        apply_event(&mut renderer, &mut controller, &shifted_key(KeyCode::Left));
        let selected_snapshot = controller.snapshot();
        render_snapshot(&mut renderer, &selected_snapshot, Rect::new(0, 0, 48, 12));
        let selected_field = renderer.focused_text_field(&selected_snapshot).unwrap();
        let selected_editor = renderer.custom_inputs[&0].clone();
        assert!(matches!(
            renderer.apply_focused_text_field_edit(
                &completion_edit(selected_field, "src/main.rs"),
                &selected_snapshot,
                &TEST_HOST,
            ),
            TerminalInteractionInput::Consumed
        ));
        assert_eq!(renderer.custom_inputs[&0], selected_editor);
        assert_eq!(controller.snapshot(), selected_snapshot);
        apply_event(&mut renderer, &mut controller, &shifted_key(KeyCode::Right));
        let before_completion = renderer.custom_inputs[&0].buffer().edit_state();
        let snapshot = controller.snapshot();
        render_snapshot(&mut renderer, &snapshot, Rect::new(0, 0, 48, 12));
        let field = renderer.focused_text_field(&snapshot).unwrap();
        let content = renderer.custom_inputs[&0].content_area();
        assert_eq!(field.content_rect, content);
        assert!(content.contains(bmux_tui::geometry::Point::new(
            field.anchor.x,
            field.anchor.y
        )));
        let edit = completion_edit(field, "src/main.rs");
        let TerminalInteractionInput::Semantic(change @ InteractionInput::Change { .. }) =
            renderer.apply_focused_text_field_edit(&edit, &snapshot, &TEST_HOST)
        else {
            panic!("completion must produce a change, never submit");
        };
        assert_eq!(controller.handle_input(change), InteractionOutput::Redraw);
        let inserted = renderer.custom_inputs[&0].buffer().edit_state();
        render_snapshot(
            &mut renderer,
            &controller.snapshot(),
            Rect::new(0, 0, 48, 12),
        );
        assert_eq!(renderer.custom_inputs[&0].buffer().edit_state(), inserted);
        let undo = Event::Key(bmux_keyboard::parse_key_stroke("ctrl+z").unwrap());
        apply_event(&mut renderer, &mut controller, &undo);
        assert_eq!(
            controller.snapshot().answers[0].custom.as_deref(),
            Some("@sr")
        );
        assert_eq!(
            renderer.custom_inputs[&0].buffer().edit_state(),
            before_completion
        );
        render_snapshot(
            &mut renderer,
            &controller.snapshot(),
            Rect::new(0, 0, 48, 12),
        );
        let redo = Event::Key(bmux_keyboard::parse_key_stroke("ctrl+y").unwrap());
        apply_event(&mut renderer, &mut controller, &redo);
        assert_eq!(renderer.custom_inputs[&0].buffer().edit_state(), inserted);
        let InteractionOutput::Submitted { payload } =
            apply_event(&mut renderer, &mut controller, &key(KeyCode::Enter))
        else {
            panic!("actual submit must produce the answer");
        };
        assert_eq!(payload["questions"][0]["custom"], "src/main.rs");
    }

    #[test]
    fn custom_completion_noop_invalid_and_focus_roundtrip_leave_editor_unchanged() {
        let mut controller = QuestionInteractionController::new(NormalizedQuestionRequest {
            questions: vec![
                question("First", &[], true, false),
                question("Second", &[], true, false),
            ],
        });
        let mut renderer = QuestionTerminalRenderer::default();
        apply_event(&mut renderer, &mut controller, &Event::Paste("@é".into()));
        let snapshot = controller.snapshot();
        render_snapshot(&mut renderer, &snapshot, Rect::new(0, 0, 48, 20));
        let field = renderer.focused_text_field(&snapshot).unwrap();
        let editor = renderer.custom_inputs[&0].clone();
        bcode_plugin_sdk::tui::focused_input::test_support::assert_noop_and_stale_preserve_field(
            &mut renderer,
            |renderer| renderer.focused_text_field(&snapshot),
            |renderer, edit| {
                let result =
                    renderer.apply_focused_text_field_edit_result(edit, &snapshot, &TEST_HOST);
                assert!(matches!(result.input, TerminalInteractionInput::Consumed));
                assert_eq!(renderer.custom_inputs[&0], editor);
                result.edit_result
            },
        );
        let mut noop = completion_edit(field.clone(), "@é");
        noop.edit.cursor = 0;
        assert_eq!(
            renderer
                .apply_focused_text_field_edit_result(&noop, &snapshot, &TEST_HOST)
                .edit_result,
            FocusedTextFieldEditResult::Noop
        );
        assert_eq!(renderer.custom_inputs[&0], editor);
        let mut invalid = completion_edit(field.clone(), "path");
        invalid.edit.range.end = 2;
        assert!(matches!(
            renderer.apply_focused_text_field_edit(&invalid, &snapshot, &TEST_HOST),
            TerminalInteractionInput::Consumed
        ));
        assert_eq!(renderer.custom_inputs[&0], editor);
        assert_eq!(controller.snapshot(), snapshot);
        controller.handle_input(InteractionInput::Focus {
            control_id: custom_control_id(1),
        });
        render_snapshot(
            &mut renderer,
            &controller.snapshot(),
            Rect::new(0, 0, 48, 20),
        );
        controller.handle_input(InteractionInput::Focus {
            control_id: custom_control_id(0),
        });
        render_snapshot(
            &mut renderer,
            &controller.snapshot(),
            Rect::new(0, 0, 48, 20),
        );
        let stale = completion_edit(field, "path");
        assert!(matches!(
            renderer.apply_focused_text_field_edit(&stale, &controller.snapshot(), &TEST_HOST),
            TerminalInteractionInput::Consumed
        ));
        assert_eq!(
            renderer.custom_inputs[&0].buffer().edit_state(),
            editor.buffer().edit_state()
        );
        controller.handle_input(InteractionInput::Focus {
            control_id: InteractionControlId::new("submit"),
        });
        assert!(
            renderer
                .focused_text_field(&controller.snapshot())
                .is_none()
        );
    }

    #[test]
    fn question_history_bindings_can_be_remapped_and_disabled_without_raw_fallback() {
        let mut registry = test_registry();
        registry
            .configure(BTreeMap::from([(
                "question.text".into(),
                BTreeMap::from([
                    ("ctrl+z".into(), String::new()),
                    ("alt+u".into(), "question.text.undo".into()),
                    ("ctrl+y".into(), String::new()),
                    ("ctrl+shift+z".into(), String::new()),
                ]),
            )]))
            .unwrap();
        let host = RemappedHost(registry);
        let mut controller = QuestionInteractionController::new(NormalizedQuestionRequest {
            questions: vec![question("Answer", &[], true, false)],
        });
        let mut renderer = QuestionTerminalRenderer::default();
        apply_event(&mut renderer, &mut controller, &Event::Paste("text".into()));
        let snapshot = controller.snapshot();
        let before = renderer.custom_inputs[&0].clone();
        for binding in ["ctrl+z", "ctrl+y", "ctrl+shift+z"] {
            assert!(matches!(
                routed_input(
                    &mut renderer,
                    &Event::Key(bmux_keyboard::parse_key_stroke(binding).unwrap()),
                    &snapshot,
                    &host
                ),
                TerminalInteractionInput::Ignored
            ));
            assert_eq!(renderer.custom_inputs[&0], before);
        }
        assert!(matches!(
            routed_input(
                &mut renderer,
                &Event::Key(bmux_keyboard::parse_key_stroke("alt+u").unwrap()),
                &snapshot,
                &host
            ),
            TerminalInteractionInput::Semantic(InteractionInput::Change { .. })
        ));
        assert_eq!(renderer.custom_inputs[&0].buffer().text(), "");
    }

    #[test]
    fn standard_keyboard_controls_select_radio_and_checkbox_options() {
        let mut radio = QuestionInteractionController::new(NormalizedQuestionRequest {
            questions: vec![question(
                "Radio",
                &[("One", None), ("Two", None)],
                false,
                true,
            )],
        });
        let mut renderer = QuestionTerminalRenderer::default();
        assert_eq!(
            apply_event(&mut renderer, &mut radio, &key(KeyCode::Down)),
            InteractionOutput::Redraw
        );
        assert_eq!(
            apply_event(&mut renderer, &mut radio, &key(KeyCode::Enter)),
            InteractionOutput::Redraw
        );
        assert_eq!(radio.snapshot().answers[0].selected, ["Two"]);

        let mut checkbox_question =
            question("Checkbox", &[("One", None), ("Two", None)], false, false);
        checkbox_question.control = QuestionControl::Checkbox;
        checkbox_question.selection_mode = QuestionSelectionMode::Multiple;
        let mut checkbox = QuestionInteractionController::new(NormalizedQuestionRequest {
            questions: vec![checkbox_question],
        });
        assert_eq!(
            apply_event(&mut renderer, &mut checkbox, &key(KeyCode::Space)),
            InteractionOutput::Redraw
        );
        assert_eq!(checkbox.snapshot().answers[0].selected, ["One"]);
        assert_eq!(
            apply_event(&mut renderer, &mut checkbox, &key(KeyCode::Space)),
            InteractionOutput::Redraw
        );
        assert!(checkbox.snapshot().answers[0].selected.is_empty());
        apply_event(&mut renderer, &mut checkbox, &key(KeyCode::Tab));
        apply_event(&mut renderer, &mut checkbox, &key(KeyCode::Space));
        assert_eq!(checkbox.snapshot().answers[0].selected, ["Two"]);
    }

    #[test]
    fn composer_grade_custom_input_supports_cursor_delete_unicode_multiline_and_paste() {
        let mut controller = QuestionInteractionController::new(NormalizedQuestionRequest {
            questions: vec![question("Explain", &[], true, true)],
        });
        let mut renderer = QuestionTerminalRenderer::default();

        for event in [
            shifted_character('A'),
            key(KeyCode::Char('b')),
            key(KeyCode::Char('é')),
            key(KeyCode::Left),
            key(KeyCode::Backspace),
            Event::Paste("🙂".to_owned()),
            shifted_key(KeyCode::Enter),
            key(KeyCode::Char('Z')),
        ] {
            let _ = render_then_apply_event(&mut renderer, &mut controller, &event);
        }

        assert_eq!(
            controller.snapshot().answers[0].custom.as_deref(),
            Some("A🙂\nZé")
        );
    }

    #[test]
    fn custom_input_accepts_shift_modified_capital_letters() {
        let mut controller = QuestionInteractionController::new(NormalizedQuestionRequest {
            questions: vec![question("Explain", &[], true, true)],
        });
        let mut renderer = QuestionTerminalRenderer::default();

        assert_eq!(
            apply_event(&mut renderer, &mut controller, &shifted_character('A')),
            InteractionOutput::Redraw
        );
        assert_eq!(
            controller.snapshot().answers[0].custom.as_deref(),
            Some("A")
        );
    }

    #[test]
    fn custom_input_reserves_left_right_home_end_and_delete_for_host_behavior() {
        let mut controller = QuestionInteractionController::new(NormalizedQuestionRequest {
            questions: vec![question("Explain", &[], true, true)],
        });
        controller.handle_input(InteractionInput::Change {
            control_id: custom_control_id(0),
            value: InteractionValue::String("answer".to_owned()),
        });
        let snapshot = controller.snapshot();
        let mut renderer = QuestionTerminalRenderer::default();
        for key in [
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::Delete,
        ] {
            assert_eq!(
                routed_input(
                    &mut renderer,
                    &Event::Key(KeyStroke {
                        key,
                        modifiers: Modifiers::NONE,
                    }),
                    &snapshot,
                    &TEST_HOST,
                ),
                TerminalInteractionInput::Consumed,
                "{key:?} must remain handled by the custom editor"
            );
        }
        assert_eq!(
            controller.snapshot().answers[0].custom.as_deref(),
            Some("answer")
        );
    }

    #[test]
    fn renderer_uses_host_theme_for_options_actions_and_hints() {
        let mut controller = QuestionInteractionController::new(NormalizedQuestionRequest {
            questions: vec![question("Choose", &[("Yes", None)], false, true)],
        });
        controller.handle_input(InteractionInput::Activate {
            control_id: option_control_id(0, 0),
        });
        let snapshot = controller.snapshot();
        let mut renderer = QuestionTerminalRenderer::default();
        let style = Style::new();
        let syntax_color = PluginTuiSyntaxColor::from_tui(Color::Default);
        let theme = PluginTuiTheme {
            component_theme_version: bcode_plugin_sdk::tui::PLUGIN_TUI_COMPONENT_THEME_VERSION,
            canvas: style,
            text: style.fg(Color::Blue),
            muted: style.fg(Color::BrightBlack),
            border: style,
            focused: style.fg(Color::Magenta),
            selection: style.fg(Color::Green),
            source: PluginTuiSourceTheme {
                source: style,
                border: style,
                gutter: style,
                truncated: style,
            },
            diff: PluginTuiDiffTheme {
                text: style,
                muted: style,
                title: style,
                label: style,
                added: style,
                removed: style,
                hunk: style,
                added_row: style,
                removed_row: style,
                added_emphasis: style,
                removed_emphasis: style,
            },
            syntax: PluginTuiSyntaxTheme {
                text: syntax_color,
                comment: syntax_color,
                keyword: syntax_color,
                function: syntax_color,
                variable: syntax_color,
                string: syntax_color,
                number: syntax_color,
                type_name: syntax_color,
                operator: syntax_color,
                punctuation: syntax_color,
                heading: syntax_color,
                link: syntax_color,
                raw: syntax_color,
            },
        };
        let buffer =
            render_snapshot_with_theme(&mut renderer, &snapshot, Rect::new(0, 0, 48, 12), &theme);
        let text = rendered_text(&buffer);

        assert!(text.contains("[ Submit ]"));
        assert!(text.contains("tab"));
        let selected_row = (0..buffer.area().height)
            .find(|row| {
                buffer
                    .row_symbols(*row)
                    .is_some_and(|line| line.contains("(*) Yes"))
            })
            .expect("selected option row");
        assert_eq!(
            buffer
                .get(Point::new(9, selected_row))
                .and_then(|cell| cell.style.fg),
            Some(Color::Green)
        );
    }

    #[test]
    fn renderer_shows_focus_selection_descriptions_and_validation() {
        let request = NormalizedQuestionRequest {
            questions: vec![question(
                "Choose carefully",
                &[("Yes", Some("Continue with the operation")), ("No", None)],
                false,
                true,
            )],
        };
        let mut controller = QuestionInteractionController::new(request);
        controller.handle_input(InteractionInput::Activate {
            control_id: option_control_id(0, 0),
        });
        let selected = controller.snapshot();
        let mut renderer = QuestionTerminalRenderer::default();
        let buffer = render_snapshot(&mut renderer, &selected, Rect::new(0, 0, 48, 12));
        let text = rendered_text(&buffer);

        assert!(text.contains("Choose carefully *"));
        assert!(text.contains("1. (*) Yes"));
        assert!(text.contains("Continue with the operation"));
        let selected_row = (0..buffer.area().height)
            .find(|row| {
                buffer
                    .row_symbols(*row)
                    .is_some_and(|line| line.contains("(*) Yes"))
            })
            .expect("selected option row");
        assert_eq!(
            buffer
                .get(Point::new(9, selected_row))
                .and_then(|cell| cell.style.fg),
            Some(Color::Cyan)
        );

        let mut required = QuestionInteractionController::new(NormalizedQuestionRequest {
            questions: vec![question("Required", &[("Answer", None)], false, true)],
        });
        let initial_height =
            QuestionTerminalRenderer::default().preferred_height(&required.snapshot(), 32);
        assert_eq!(
            required.handle_input(InteractionInput::Submit),
            InteractionOutput::Redraw
        );
        let validation_snapshot = required.snapshot();
        let mut validation_renderer = QuestionTerminalRenderer::default();
        let validation_height = validation_renderer.preferred_height(&validation_snapshot, 32);
        assert!(validation_height > initial_height);
        let validation = render_snapshot(
            &mut validation_renderer,
            &validation_snapshot,
            Rect::new(0, 0, 32, 10),
        );
        assert!(rendered_text(&validation).contains("An answer is required."));
    }

    #[test]
    fn renderer_wraps_content_and_remains_valid_at_tiny_widths() {
        let controller = QuestionInteractionController::new(NormalizedQuestionRequest {
            questions: vec![question(
                "A deliberately long question that must wrap",
                &[("A deliberately long option", Some("A long description too"))],
                true,
                false,
            )],
        });
        let snapshot = controller.snapshot();
        let mut renderer = QuestionTerminalRenderer::default();
        let height = renderer.preferred_height(&snapshot, 12);
        assert!(height > 8);
        let buffer = render_snapshot(&mut renderer, &snapshot, Rect::new(0, 0, 12, 8));
        assert_eq!(buffer.area(), Rect::new(0, 0, 12, 8));
        assert!(!rendered_text(&buffer).contains("more"));
        assert!(renderer.controls.iter().all(|control| {
            control.area.width > 0
                && control.area.height > 0
                && control.area.x >= buffer.area().x
                && control.area.right() <= buffer.area().right()
                && control.area.y >= buffer.area().y
                && control.area.bottom() <= buffer.area().bottom()
        }));
    }

    #[test]
    fn layout_cache_invalidation_tracks_extent_dependencies_without_retaining_history() {
        let mut controller = QuestionInteractionController::new(NormalizedQuestionRequest {
            questions: vec![question(
                "A deliberately long question that must wrap",
                &[("A deliberately long option", Some("A long description too"))],
                true,
                true,
            )],
        });
        let mut renderer = QuestionTerminalRenderer::default();
        let initial = controller.snapshot();
        let initial_height = renderer.preferred_height(&initial, 18);
        let initial_prompt = Arc::clone(
            &renderer.measurement.as_ref().unwrap().questions[0]
                .prompt
                .chunks,
        );

        controller.handle_input(InteractionInput::Navigate {
            direction: InteractionNavigation::Next,
        });
        let focused = controller.snapshot();
        assert_eq!(renderer.preferred_height(&focused, 18), initial_height);
        assert!(Arc::ptr_eq(
            &initial_prompt,
            &renderer.measurement.as_ref().unwrap().questions[0]
                .prompt
                .chunks
        ));

        let narrow_height = renderer.preferred_height(&focused, 9);
        let narrow_prompt = Arc::clone(
            &renderer.measurement.as_ref().unwrap().questions[0]
                .prompt
                .chunks,
        );
        assert!(narrow_height > initial_height);
        assert!(!Arc::ptr_eq(&initial_prompt, &narrow_prompt));
        assert_eq!(renderer.measurement.as_ref().unwrap().width, 9);

        controller.handle_input(InteractionInput::Change {
            control_id: custom_control_id(0),
            value: InteractionValue::String("line one\nline two\nline three".to_owned()),
        });
        let edited = controller.snapshot();
        let edited_height = renderer.preferred_height(&edited, 9);
        assert!(edited.layout_revision > focused.layout_revision);
        assert!(edited_height >= narrow_height);
        assert!(!Arc::ptr_eq(
            &narrow_prompt,
            &renderer.measurement.as_ref().unwrap().questions[0]
                .prompt
                .chunks
        ));
        assert_eq!(
            renderer.measurement.as_ref().unwrap().layout_revision,
            edited.layout_revision
        );
    }

    #[test]
    fn focus_navigation_reuses_wrapped_layout_geometry() {
        let mut controller = QuestionInteractionController::new(NormalizedQuestionRequest {
            questions: vec![question(
                "A deliberately long question that must wrap",
                &[("A deliberately long option", Some("A long description too"))],
                true,
                true,
            )],
        });
        let mut renderer = QuestionTerminalRenderer::default();
        let initial = controller.snapshot();
        let initial_height = renderer.preferred_height(&initial, 18);
        let prompt = Arc::clone(
            &renderer.measurement.as_ref().unwrap().questions[0]
                .prompt
                .chunks,
        );
        let label = Arc::clone(
            &renderer.measurement.as_ref().unwrap().questions[0].options[0]
                .label
                .chunks,
        );
        let description = Arc::clone(
            &renderer.measurement.as_ref().unwrap().questions[0].options[0]
                .description
                .as_ref()
                .unwrap()
                .chunks,
        );

        controller.handle_input(InteractionInput::Navigate {
            direction: InteractionNavigation::Next,
        });
        let focused = controller.snapshot();
        assert_eq!(renderer.preferred_height(&focused, 18), initial_height);
        let geometry = &renderer.measurement.as_ref().unwrap().questions[0];
        assert!(Arc::ptr_eq(&prompt, &geometry.prompt.chunks));
        assert!(Arc::ptr_eq(&label, &geometry.options[0].label.chunks));
        assert!(Arc::ptr_eq(
            &description,
            &geometry.options[0].description.as_ref().unwrap().chunks
        ));
    }

    #[test]
    fn maximum_valid_layout_is_bounded_and_slices_only_visible_questions() {
        let option_label = "o".repeat(crate::MAX_OPTION_TEXT_BYTES);
        let options = (0..crate::MAX_OPTIONS_PER_QUESTION)
            .map(|index| QuestionOption {
                label: option_label.clone(),
                value: Some(index.to_string()),
                description: Some(option_label.clone()),
            })
            .collect::<Vec<_>>();
        let questions = (0..crate::MAX_QUESTIONS)
            .map(|index| Question {
                header: Some(format!("Question {index}")),
                text: "q".repeat(crate::MAX_QUESTION_TEXT_BYTES),
                options: options.clone(),
                control: QuestionControl::Checkbox,
                selection_mode: QuestionSelectionMode::Multiple,
                custom: true,
                custom_mode: QuestionCustomMode::Additional,
                required: true,
            })
            .collect();
        let controller =
            QuestionInteractionController::new(NormalizedQuestionRequest { questions });
        let snapshot = controller.snapshot();
        let mut renderer = QuestionTerminalRenderer::default();
        let logical_height = renderer.preferred_height(&snapshot, 80);
        assert_eq!(
            renderer.measurement.as_ref().unwrap().questions.len(),
            crate::MAX_QUESTIONS
        );
        assert!(logical_height > 512);

        let area = Rect::new(0, 0, 80, 12);
        let mut buffer = Buffer::empty(area);
        let mut backend = Frame::new(&mut buffer);
        let mut frame = PaintCx::new(&mut backend);
        renderer.render_slice(&snapshot, logical_height, 4_000, area, &mut frame);
        assert!(renderer.rendered_questions <= 1);
        assert_eq!(renderer.visited_question_spans, renderer.rendered_questions);
    }

    #[test]
    fn duplicate_values_keep_distinct_rendered_hit_regions() {
        let duplicate = QuestionOption {
            label: "Same".to_owned(),
            value: Some("duplicate".to_owned()),
            description: None,
        };
        let controller = QuestionInteractionController::new(NormalizedQuestionRequest {
            questions: vec![Question {
                header: None,
                text: "Choose duplicates".to_owned(),
                options: vec![duplicate.clone(), duplicate],
                control: QuestionControl::Checkbox,
                selection_mode: QuestionSelectionMode::Multiple,
                custom: false,
                custom_mode: QuestionCustomMode::Additional,
                required: false,
            }],
        });
        let mut renderer = QuestionTerminalRenderer::default();
        let _buffer = render_snapshot(
            &mut renderer,
            &controller.snapshot(),
            Rect::new(0, 0, 32, 10),
        );
        assert_eq!(renderer.controls[0].control_id, option_control_id(0, 0));
        assert_eq!(renderer.controls[1].control_id, option_control_id(0, 1));
        assert_ne!(renderer.controls[0].area, renderer.controls[1].area);
    }

    #[test]
    fn sliced_render_visits_only_intersecting_questions() {
        let questions = (0..20)
            .map(|index| {
                question(
                    &format!("Question {index}"),
                    &[("Answer", None)],
                    false,
                    false,
                )
            })
            .collect();
        let controller =
            QuestionInteractionController::new(NormalizedQuestionRequest { questions });
        let snapshot = controller.snapshot();
        let mut renderer = QuestionTerminalRenderer::default();
        let width = 40;
        let logical_height = renderer.preferred_height(&snapshot, width);
        let area = Rect::new(0, 0, width, 4);
        let mut buffer = Buffer::empty(area);
        renderer.render_slice(
            &snapshot,
            logical_height,
            logical_height.saturating_sub(area.height),
            area,
            &mut PaintCx::new(&mut Frame::new(&mut buffer)),
        );

        assert!(renderer.rendered_questions <= 2);
        assert_eq!(renderer.visited_question_spans, renderer.rendered_questions);
        assert!(renderer.rendered_questions < snapshot.request.questions.len());
    }

    #[test]
    fn transcript_slices_use_stable_logical_coordinates_without_focus_scrolling() {
        let request = NormalizedQuestionRequest {
            questions: vec![
                question(
                    "First long question that wraps",
                    &[("First", Some("First description")), ("Second", None)],
                    true,
                    true,
                ),
                question(
                    "Second long question that wraps",
                    &[("Third", Some("Third description")), ("Fourth", None)],
                    true,
                    true,
                ),
            ],
        };
        let mut controller = QuestionInteractionController::new(request);
        let mut renderer = QuestionTerminalRenderer::default();
        let area = Rect::new(0, 0, 24, 7);
        let initial = render_snapshot(&mut renderer, &controller.snapshot(), area);
        assert!(!rendered_text(&initial).contains("more"));

        for _ in 0..6 {
            controller.handle_input(InteractionInput::Navigate {
                direction: InteractionNavigation::Next,
            });
        }
        let focused = controller.snapshot();
        assert_eq!(focused.focus, QuestionFocusTarget::Submit);
        let repeated = render_snapshot(&mut renderer, &focused, area);
        assert_ne!(rendered_text(&initial), rendered_text(&repeated));
        assert!(rendered_text(&initial).contains("> 1. ( ) First"));
        assert!(rendered_text(&repeated).contains("  1. ( ) First"));

        let logical_height = renderer.preferred_height(&focused, area.width);
        let mut buffer = Buffer::empty(area);
        let mut backend = Frame::new(&mut buffer);
        let mut frame = PaintCx::new(&mut backend);
        renderer.render_slice(&focused, logical_height, 7, area, &mut frame);
        assert_ne!(rendered_text(&initial), rendered_text(&buffer));
        assert!(!rendered_text(&buffer).contains("more"));
    }

    #[test]
    fn custom_multiline_arrows_edit_interior_rows_and_leave_at_edges() {
        let request = NormalizedQuestionRequest {
            questions: vec![question("Explain", &[], true, true)],
        };
        let mut controller = QuestionInteractionController::new(request);
        controller.handle_input(InteractionInput::Change {
            control_id: custom_control_id(0),
            value: InteractionValue::String("first\nsecond\nthird".to_owned()),
        });
        let mut renderer = QuestionTerminalRenderer::default();
        let snapshot = controller.snapshot();
        let _buffer = render_snapshot(&mut renderer, &snapshot, Rect::new(0, 0, 32, 12));
        let state = renderer.custom_inputs.get_mut(&0).expect("custom state");
        state
            .buffer_mut()
            .move_cursor(bmux_text_edit::TextMotion::Start);

        assert_eq!(
            routed_input(&mut renderer, &key(KeyCode::Up), &snapshot, &TEST_HOST),
            TerminalInteractionInput::Semantic(InteractionInput::Navigate {
                direction: InteractionNavigation::Previous,
            })
        );
        assert_eq!(
            routed_input(&mut renderer, &key(KeyCode::Down), &snapshot, &TEST_HOST),
            TerminalInteractionInput::Consumed
        );
        assert_eq!(
            routed_input(&mut renderer, &key(KeyCode::Down), &snapshot, &TEST_HOST),
            TerminalInteractionInput::Consumed
        );
        assert_eq!(
            routed_input(&mut renderer, &key(KeyCode::Down), &snapshot, &TEST_HOST),
            TerminalInteractionInput::Semantic(InteractionInput::Navigate {
                direction: InteractionNavigation::Next,
            })
        );
        assert_eq!(
            renderer.custom_inputs[&0].buffer().text(),
            "first\nsecond\nthird"
        );
    }

    #[test]
    fn tab_and_shift_tab_visit_options_custom_submit_and_cancel() {
        let request = NormalizedQuestionRequest {
            questions: vec![question(
                "Choose",
                &[("One", None), ("Two", None)],
                true,
                true,
            )],
        };
        let mut controller = QuestionInteractionController::new(request);
        let expected = [
            QuestionFocusTarget::Option {
                question_index: 0,
                option_index: 0,
            },
            QuestionFocusTarget::Option {
                question_index: 0,
                option_index: 1,
            },
            QuestionFocusTarget::Custom { question_index: 0 },
            QuestionFocusTarget::Submit,
            QuestionFocusTarget::Cancel,
        ];
        assert_eq!(controller.snapshot().focus, expected[0]);
        for target in expected.iter().skip(1) {
            controller.handle_input(InteractionInput::Navigate {
                direction: InteractionNavigation::Next,
            });
            assert_eq!(&controller.snapshot().focus, target);
        }
        for target in expected.iter().rev().skip(1) {
            controller.handle_input(InteractionInput::Navigate {
                direction: InteractionNavigation::Previous,
            });
            assert_eq!(&controller.snapshot().focus, target);
        }
    }

    #[test]
    fn clicking_an_unfocused_custom_input_focuses_and_places_cursor_on_first_click() {
        let request = NormalizedQuestionRequest {
            questions: vec![
                question("First", &[("One", None)], false, false),
                question("Second", &[], true, true),
            ],
        };
        let mut controller = QuestionInteractionController::new(request);
        controller.handle_input(InteractionInput::Change {
            control_id: custom_control_id(1),
            value: InteractionValue::String("abcd".to_owned()),
        });
        controller.handle_input(InteractionInput::Focus {
            control_id: option_control_id(0, 0),
        });
        let mut renderer = QuestionTerminalRenderer::default();
        let snapshot = controller.snapshot();
        let _buffer = render_snapshot(&mut renderer, &snapshot, Rect::new(0, 0, 40, 30));
        let area = renderer.custom_inputs[&1].content_area();
        let input = routed_input(
            &mut renderer,
            &Event::Mouse(bmux_tui::event::MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(area.x, area.y),
            )),
            &snapshot,
            &TEST_HOST,
        );
        let TerminalInteractionInput::Semantic(input) = input else {
            panic!("focus custom field");
        };
        assert_eq!(
            input,
            InteractionInput::Focus {
                control_id: custom_control_id(1),
            }
        );
        controller.handle_input(input);
        let focused = controller.snapshot();
        let _buffer = render_snapshot(&mut renderer, &focused, Rect::new(0, 0, 40, 30));
        assert_eq!(
            focused.focus,
            QuestionFocusTarget::Custom { question_index: 1 }
        );
        assert!(renderer.pending_custom_mouse_focus.is_none());
        assert!(renderer.custom_inputs[&1].buffer().cursor_byte_index() < "abcd".len());
    }

    #[test]
    fn first_click_focus_preserves_drag_selection() {
        let request = NormalizedQuestionRequest {
            questions: vec![
                question("First", &[("One", None)], false, false),
                question("Second", &[], true, true),
            ],
        };
        let mut controller = QuestionInteractionController::new(request);
        controller.handle_input(InteractionInput::Change {
            control_id: custom_control_id(1),
            value: InteractionValue::String("abcdef".to_owned()),
        });
        controller.handle_input(InteractionInput::Focus {
            control_id: option_control_id(0, 0),
        });
        let mut renderer = QuestionTerminalRenderer::default();
        let snapshot = controller.snapshot();
        let _buffer = render_snapshot(&mut renderer, &snapshot, Rect::new(0, 0, 40, 30));
        let area = renderer.custom_inputs[&1].content_area();
        let focus = routed_input(
            &mut renderer,
            &Event::Mouse(bmux_tui::event::MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(area.x, area.y),
            )),
            &snapshot,
            &TEST_HOST,
        );
        let TerminalInteractionInput::Semantic(focus) = focus else {
            panic!("focus custom field");
        };
        controller.handle_input(focus);
        let focused = controller.snapshot();
        let _buffer = render_snapshot(&mut renderer, &focused, Rect::new(0, 0, 40, 30));
        assert_eq!(
            routed_input(
                &mut renderer,
                &Event::Mouse(bmux_tui::event::MouseEvent::new(
                    MouseEventKind::Drag(MouseButton::Left),
                    Point::new(area.x.saturating_add(3), area.y),
                )),
                &focused,
                &TEST_HOST,
            ),
            TerminalInteractionInput::Consumed
        );
        assert_eq!(
            renderer.custom_inputs[&1].buffer().selected_text(),
            Some("abc".to_owned())
        );
    }

    #[test]
    fn clicking_a_visible_option_focuses_and_activates_it_once() {
        let request = NormalizedQuestionRequest {
            questions: vec![question(
                "Choose",
                &[("One", None), ("Two", None)],
                false,
                false,
            )],
        };
        let mut controller = QuestionInteractionController::new(request);
        let mut renderer = QuestionTerminalRenderer::default();
        let snapshot = controller.snapshot();
        let _buffer = render_snapshot(&mut renderer, &snapshot, Rect::new(0, 0, 32, 10));
        let second = renderer.controls[1].area;
        let input = routed_input(
            &mut renderer,
            &Event::Mouse(bmux_tui::event::MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(second.x, second.y),
            )),
            &snapshot,
            &TEST_HOST,
        );
        let TerminalInteractionInput::Semantic(input) = input else {
            panic!("visible option click");
        };
        controller.handle_input(input);
        let clicked = controller.snapshot();
        assert_eq!(clicked.answers[0].selected, ["Two"]);
        assert_eq!(
            clicked.focus,
            QuestionFocusTarget::Option {
                question_index: 0,
                option_index: 1,
            }
        );
    }
}
