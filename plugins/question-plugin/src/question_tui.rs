//! Native TUI renderer for the interactive question tool.

use std::collections::BTreeMap;

use bcode_plugin_sdk::tui::{
    PluginTuiTheme, TerminalInteractionInput, TerminalInteractionRenderer,
};
use bcode_tool::{InteractionControlId, InteractionInput, InteractionNavigation, InteractionValue};
use bmux_keyboard::KeyCode;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::event::{Event, MouseButton, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui::text_width::wrap_text_with_continuation;
use bmux_tui_components::action_row::{ActionButton, ActionRow, ActionRowStyles};
use bmux_tui_components::key_hint_bar::{KeyHint, KeyHintBar, KeyHintBarStyles};
use bmux_tui_components::text_input::{TextInputControl, TextInputPolicy, TextInputState};
use bmux_tui_components::text_input_box::{TextInputBox, TextInputBoxPolicy};

use super::question_interaction::{
    QuestionFocusTarget, QuestionInteractionController, QuestionSnapshot, custom_control_id,
    option_control_id,
};
use super::{QUESTION_INLINE_SURFACE, QuestionSelectionMode};

const DESCRIPTION_INDENT: &str = "        ";

/// Terminal renderer for the question interaction.
#[derive(Default)]
pub struct QuestionTerminalRenderer {
    last_area: Rect,
    controls: Vec<ControlRegion>,
    logical_origin: u16,
    custom_inputs: BTreeMap<usize, TextInputState>,
    custom_areas: BTreeMap<usize, Rect>,
    pending_custom_mouse_focus: Option<usize>,
    theme: QuestionSurfaceTheme,
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
    fn resolve(theme: Option<&PluginTuiTheme>) -> Self {
        theme
            .and_then(PluginTuiTheme::component_theme)
            .map_or_else(Self::default, |theme| Self {
                text: theme.text,
                muted: theme.muted,
                focused: theme.focused,
                selection: theme.selected,
                error: theme.error,
                canvas: theme.canvas,
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ControlRegion {
    area: Rect,
    control_id: InteractionControlId,
}

impl QuestionTerminalRenderer {
    fn render_line(&self, frame: &mut Frame<'_>, content_y: &mut u16, line: &Line) {
        if let Some(screen_y) = self.screen_y(*content_y) {
            frame.write_line(
                Rect::new(self.last_area.x, screen_y, self.last_area.width, 1),
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
        frame: &mut Frame<'_>,
        content_y: &mut u16,
        text: &str,
        first_prefix: &str,
        continuation_prefix: &str,
        style: Style,
    ) {
        let first_width = usize::from(self.last_area.width)
            .saturating_sub(bmux_tui::text_width::display_width(first_prefix))
            .max(1);
        let next_width = usize::from(self.last_area.width)
            .saturating_sub(bmux_tui::text_width::display_width(continuation_prefix))
            .max(1);
        for (index, chunk) in wrap_text_with_continuation(text, first_width, next_width)
            .into_iter()
            .enumerate()
        {
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
                    Span::styled(chunk, style),
                ]),
            );
        }
    }

    fn render_title(&self, frame: &mut Frame<'_>, content_y: &mut u16) {
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
        frame: &mut Frame<'_>,
        content_y: &mut u16,
        snapshot: &QuestionSnapshot,
        question_index: usize,
    ) {
        let question = &snapshot.request.questions[question_index];
        let required = if question.required { " *" } else { "" };
        let prompt = question.header.as_ref().map_or_else(
            || format!("{}{required}", question.text),
            |header| format!("{header}{required}: {}", question.text),
        );
        self.render_wrapped(frame, content_y, &prompt, "", "", self.theme.text);
        for (option_index, option) in question.options.iter().enumerate() {
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
            let shortcut = option_shortcut_label(option_index);
            let shortcut_width = shortcut.len();
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
                &option.label,
                &prefix,
                &continuation,
                option_style(&self.theme, focused, selected),
            );
            if let Some(description) = option.description.as_deref() {
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
            self.render_wrapped(
                frame,
                content_y,
                "An answer is required.",
                "  ",
                "  ",
                self.theme.error.add_modifier(Modifier::BOLD),
            );
        }
        self.render_line(frame, content_y, &Line::from(""));
    }

    fn render_custom_answer(
        &mut self,
        frame: &mut Frame<'_>,
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
        let input_height = {
            let state = self
                .custom_inputs
                .entry(question_index)
                .or_insert_with(|| TextInputState::new(TextEditBuffer::from_text(value)));
            if state.buffer().text() != value {
                *state = TextInputState::new(TextEditBuffer::from_text(value));
                state
                    .buffer_mut()
                    .move_cursor(bmux_text_edit::TextMotion::End);
            }
            custom_input_height(state, self.last_area.width)
        };
        *content_y = content_y.saturating_add(input_height);
        let control_id = custom_control_id(question_index);
        if let Some(area) = self.control_area(start_y, input_height) {
            let state = self
                .custom_inputs
                .get_mut(&question_index)
                .expect("custom input initialized above");
            state.set_content_area(area, &TextInputPolicy::chat_composer());
            TextInputBox::new(TextInputPolicy::chat_composer())
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
                .styles(question_input_styles(&self.theme))
                .render(area, state, frame);
            self.custom_areas.insert(question_index, area);
            self.controls.push(ControlRegion { area, control_id });
        }
    }

    fn render_actions(
        &mut self,
        frame: &mut Frame<'_>,
        content_y: &mut u16,
        snapshot: &QuestionSnapshot,
    ) {
        let actions = [
            ActionButton::new("submit", "Submit"),
            ActionButton::new("cancel", "Cancel"),
        ];
        let focused = usize::from(snapshot.focus == QuestionFocusTarget::Cancel);
        if let Some(area) = self.control_area(*content_y, 1) {
            let row = ActionRow::new(&actions)
                .focused(focused)
                .styles(question_action_styles(&self.theme));
            for (index, action_area) in row.action_areas(area).into_iter().enumerate() {
                self.controls.push(ControlRegion {
                    area: action_area,
                    control_id: InteractionControlId::new(actions[index].id.clone()),
                });
            }
            row.render_with_fallback_style(area, frame, self.theme.canvas);
        }
        *content_y = content_y.saturating_add(1);
        let hints = [
            KeyHint::new("Tab/Shift-Tab or arrows", "move"),
            KeyHint::new("Enter/Space", "select"),
            KeyHint::new("Esc", "dismiss"),
        ];
        if let Some(area) = self.control_area(*content_y, 1) {
            KeyHintBar::new(&hints)
                .styles(question_hint_styles(&self.theme))
                .render(area, frame);
        }
        *content_y = content_y.saturating_add(1);
    }

    fn render_snapshot(&mut self, snapshot: &QuestionSnapshot, area: Rect, frame: &mut Frame<'_>) {
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
        self.controls.clear();
        self.custom_areas.clear();
        let mut content_y = 0;
        self.render_title(frame, &mut content_y);
        if let Some(error) = &snapshot.validation_error {
            self.render_line(
                frame,
                &mut content_y,
                &Line::from_spans(vec![Span::styled(error, self.theme.error)]),
            );
        }
        for question_index in 0..snapshot.request.questions.len() {
            self.render_question(frame, &mut content_y, snapshot, question_index);
        }
        self.render_actions(frame, &mut content_y, snapshot);
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
        host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
    ) -> TerminalInteractionInput {
        let QuestionFocusTarget::Custom { question_index } = snapshot.focus else {
            return TerminalInteractionInput::Ignored;
        };
        let value = snapshot.answers[question_index]
            .custom
            .as_deref()
            .unwrap_or_default();
        let state = self
            .custom_inputs
            .entry(question_index)
            .or_insert_with(|| TextInputState::new(TextEditBuffer::from_text(value)));
        let before = state.buffer().text().to_owned();
        match event {
            Event::Key(stroke) => {
                if let Some(motion) = host.text_selection_motion(*stroke) {
                    state
                        .buffer_mut()
                        .move_cursor_with_selection(motion, bmux_text_edit::SelectionMode::Extend);
                    state.sync_scroll_to_cursor(&TextInputPolicy::chat_composer());
                } else if let Some(command) = host.text_edit_command(*stroke) {
                    state.buffer_mut().apply_command(command);
                    state.sync_scroll_to_cursor(&TextInputPolicy::chat_composer());
                } else {
                    TextInputControl::new(&TextInputPolicy::chat_composer())
                        .handle_key(state, *stroke);
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

    fn content_height(snapshot: &QuestionSnapshot, width: u16) -> u16 {
        let width = usize::from(width.max(1));
        let mut height = 1_u16;
        if snapshot.validation_error.is_some() {
            height = height.saturating_add(1);
        }
        for (question_index, question) in snapshot.request.questions.iter().enumerate() {
            let required = if question.required { " *" } else { "" };
            let prompt = question.header.as_ref().map_or_else(
                || format!("{}{required}", question.text),
                |header| format!("{header}{required}: {}", question.text),
            );
            height = height.saturating_add(wrapped_height(&prompt, width, width));
            for (option_index, option) in question.options.iter().enumerate() {
                let prefix_width =
                    7_usize.saturating_add(option_shortcut_label(option_index).len());
                let available = width.saturating_sub(prefix_width).max(1);
                height = height.saturating_add(wrapped_height(&option.label, available, available));
                if let Some(description) = option.description.as_deref() {
                    let description_width = width
                        .saturating_sub(bmux_tui::text_width::display_width(DESCRIPTION_INDENT))
                        .max(1);
                    height = height.saturating_add(wrapped_height(
                        description,
                        description_width,
                        description_width,
                    ));
                }
            }
            if question.options.is_empty() || question.custom {
                let value = snapshot.answers[question_index]
                    .custom
                    .as_deref()
                    .unwrap_or_default();
                let state = TextInputState::new(TextEditBuffer::from_text(value));
                height = height.saturating_add(custom_input_height(
                    &state,
                    u16::try_from(width).unwrap_or(u16::MAX),
                ));
            }
            if snapshot.invalid_question_index == Some(question_index) {
                height = height.saturating_add(1);
            }
            height = height.saturating_add(1);
        }
        height.saturating_add(2)
    }

    fn focused_content_range(snapshot: &QuestionSnapshot, width: u16) -> (u16, u16) {
        let width = usize::from(width.max(1));
        let mut y = 1_u16;
        if snapshot.validation_error.is_some() {
            y = y.saturating_add(1);
        }
        for (question_index, question) in snapshot.request.questions.iter().enumerate() {
            let required = if question.required { " *" } else { "" };
            let prompt = question.header.as_ref().map_or_else(
                || format!("{}{required}", question.text),
                |header| format!("{header}{required}: {}", question.text),
            );
            y = y.saturating_add(wrapped_height(&prompt, width, width));
            for (option_index, option) in question.options.iter().enumerate() {
                let start = y;
                let prefix_width =
                    7_usize.saturating_add(option_shortcut_label(option_index).len());
                let available = width.saturating_sub(prefix_width).max(1);
                y = y.saturating_add(wrapped_height(&option.label, available, available));
                if let Some(description) = option.description.as_deref() {
                    let description_width = width
                        .saturating_sub(bmux_tui::text_width::display_width(DESCRIPTION_INDENT))
                        .max(1);
                    y = y.saturating_add(wrapped_height(
                        description,
                        description_width,
                        description_width,
                    ));
                }
                if snapshot.focus
                    == (QuestionFocusTarget::Option {
                        question_index,
                        option_index,
                    })
                {
                    return (start, y);
                }
            }
            if question.options.is_empty() || question.custom {
                let start = y;
                let value = snapshot.answers[question_index]
                    .custom
                    .as_deref()
                    .unwrap_or_default();
                let state = TextInputState::new(TextEditBuffer::from_text(value));
                y = y.saturating_add(custom_input_height(
                    &state,
                    u16::try_from(width).unwrap_or(u16::MAX),
                ));
                if snapshot.focus == (QuestionFocusTarget::Custom { question_index }) {
                    return (start, y);
                }
            }
            if snapshot.invalid_question_index == Some(question_index) {
                y = y.saturating_add(1);
            }
            y = y.saturating_add(1);
        }
        match snapshot.focus {
            QuestionFocusTarget::Submit | QuestionFocusTarget::Cancel => (y, y.saturating_add(1)),
            QuestionFocusTarget::Option { .. } | QuestionFocusTarget::Custom { .. } => (0, 1),
        }
    }
}

impl TerminalInteractionRenderer<QuestionInteractionController> for QuestionTerminalRenderer {
    const SURFACE_KIND: &'static str = QUESTION_INLINE_SURFACE;

    fn id(&self) -> &'static str {
        "question-inline"
    }

    fn title(&self) -> &'static str {
        "Question"
    }

    fn preferred_height(&mut self, snapshot: &QuestionSnapshot, width: u16) -> u16 {
        Self::content_height(snapshot, width)
    }

    fn render(&mut self, snapshot: &QuestionSnapshot, area: Rect, frame: &mut Frame<'_>) {
        self.theme = QuestionSurfaceTheme::default();
        self.render_snapshot(snapshot, area, frame);
    }

    fn render_slice(
        &mut self,
        snapshot: &QuestionSnapshot,
        logical_height: u16,
        logical_row_offset: u16,
        destination: Rect,
        frame: &mut Frame<'_>,
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
        let (start, end) = Self::focused_content_range(snapshot, width.max(1));
        Some(start..end.max(start.saturating_add(1)))
    }

    fn render_with_theme(
        &mut self,
        snapshot: &QuestionSnapshot,
        area: Rect,
        frame: &mut Frame<'_>,
        theme: Option<&PluginTuiTheme>,
    ) {
        self.theme = QuestionSurfaceTheme::resolve(theme);
        self.render_snapshot(snapshot, area, frame);
    }

    fn input(
        &mut self,
        event: &Event,
        snapshot: &QuestionSnapshot,
        host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
    ) -> TerminalInteractionInput {
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
            && host.text_submit(*stroke)
            && matches!(
                snapshot.focus,
                QuestionFocusTarget::Custom { .. } | QuestionFocusTarget::Submit
            )
        {
            return TerminalInteractionInput::Semantic(InteractionInput::Submit);
        }
        if matches!(snapshot.focus, QuestionFocusTarget::Custom { .. })
            && let Event::Key(stroke) = event
            && matches!(stroke.key, KeyCode::Up | KeyCode::Down)
            && no_modifiers(stroke.modifiers)
        {
            return self.custom_vertical_input(stroke.key, snapshot);
        }
        if matches!(snapshot.focus, QuestionFocusTarget::Custom { .. })
            && !matches!(
                event,
                Event::Key(stroke)
                    if stroke.key == KeyCode::Tab
                        || stroke.key == KeyCode::Escape
                        || stroke.key == KeyCode::Enter && !stroke.modifiers.shift
            )
            && let input = self.custom_input(event, snapshot, host)
            && !matches!(input, TerminalInteractionInput::Ignored)
        {
            return input;
        }
        standard_input(self, event, snapshot).map_or(
            TerminalInteractionInput::Ignored,
            TerminalInteractionInput::Semantic,
        )
    }
}

const fn no_modifiers(modifiers: bmux_keyboard::Modifiers) -> bool {
    !modifiers.shift
        && !modifiers.ctrl
        && !modifiers.alt
        && !modifiers.super_key
        && !modifiers.hyper
        && !modifiers.meta
}

fn standard_input(
    renderer: &QuestionTerminalRenderer,
    event: &Event,
    snapshot: &QuestionSnapshot,
) -> Option<InteractionInput> {
    match event {
        Event::Key(stroke)
            if stroke.key == KeyCode::Tab
                && stroke.modifiers.shift
                && !stroke.modifiers.ctrl
                && !stroke.modifiers.alt
                && !stroke.modifiers.super_key
                && !stroke.modifiers.hyper
                && !stroke.modifiers.meta =>
        {
            Some(InteractionInput::Navigate {
                direction: InteractionNavigation::Previous,
            })
        }
        Event::Key(stroke)
            if !stroke.modifiers.ctrl
                && !stroke.modifiers.alt
                && !stroke.modifiers.super_key
                && !stroke.modifiers.hyper
                && !stroke.modifiers.meta =>
        {
            standard_key_input(stroke.key, snapshot)
        }
        Event::Mouse(mouse) => renderer.mouse_input(mouse),
        Event::Paste(text) => custom_text_change(snapshot, |value| value.push_str(text)),
        Event::Key(_) | Event::Resize(_) | Event::Focus(_) | Event::Tick | Event::User(_) => None,
    }
}

fn standard_key_input(key: KeyCode, snapshot: &QuestionSnapshot) -> Option<InteractionInput> {
    match key {
        KeyCode::Tab | KeyCode::Down => Some(InteractionInput::Navigate {
            direction: InteractionNavigation::Next,
        }),
        KeyCode::Up => Some(InteractionInput::Navigate {
            direction: InteractionNavigation::Previous,
        }),
        KeyCode::Right => (!matches!(snapshot.focus, QuestionFocusTarget::Custom { .. }))
            .then_some(InteractionInput::Navigate {
                direction: InteractionNavigation::Next,
            }),
        KeyCode::Left => (!matches!(snapshot.focus, QuestionFocusTarget::Custom { .. })).then_some(
            InteractionInput::Navigate {
                direction: InteractionNavigation::Previous,
            },
        ),
        KeyCode::Enter | KeyCode::Space => Some(InteractionInput::Activate {
            control_id: snapshot.focused_control_id.clone(),
        }),
        KeyCode::Escape => Some(InteractionInput::Cancel),
        KeyCode::Backspace => custom_text_change(snapshot, |text| {
            text.pop();
        }),
        KeyCode::Char(character) => option_shortcut_input(character, snapshot),
        KeyCode::Home
        | KeyCode::End
        | KeyCode::PageUp
        | KeyCode::PageDown
        | KeyCode::Delete
        | KeyCode::Insert
        | KeyCode::F(_) => None,
    }
}

fn option_shortcut_input(character: char, snapshot: &QuestionSnapshot) -> Option<InteractionInput> {
    if matches!(snapshot.focus, QuestionFocusTarget::Custom { .. }) {
        return custom_text_change(snapshot, |text| text.push(character));
    }
    let QuestionFocusTarget::Option { question_index, .. } = snapshot.focus else {
        return None;
    };
    let option_index = option_shortcut(character).filter(|option_index| {
        *option_index < snapshot.request.questions[question_index].options.len()
    })?;
    Some(InteractionInput::Activate {
        control_id: option_control_id(question_index, option_index),
    })
}

fn option_shortcut_label(option_index: usize) -> String {
    match option_index {
        0..=8 => option_index.saturating_add(1).to_string(),
        9 => "0".to_owned(),
        _ => "-".to_owned(),
    }
}

fn option_shortcut(character: char) -> Option<usize> {
    match character {
        '1'..='9' => character
            .to_digit(10)
            .and_then(|digit| usize::try_from(digit).ok())
            .and_then(|digit| digit.checked_sub(1)),
        '0' => Some(9),
        _ => None,
    }
}

fn custom_text_change(
    snapshot: &QuestionSnapshot,
    change: impl FnOnce(&mut String),
) -> Option<InteractionInput> {
    let QuestionFocusTarget::Custom { question_index } = snapshot.focus else {
        return None;
    };
    let mut text = snapshot.answers[question_index]
        .custom
        .clone()
        .unwrap_or_default();
    change(&mut text);
    Some(InteractionInput::Change {
        control_id: custom_control_id(question_index),
        value: InteractionValue::String(text),
    })
}

fn custom_input_height(state: &TextInputState, width: u16) -> u16 {
    TextInputControl::new(&TextInputPolicy::chat_composer())
        .visible_rows_for_width(state, width.saturating_sub(2).max(1))
        .clamp(1, 6)
        .saturating_add(2)
}

fn wrapped_height(text: &str, first_width: usize, continuation_width: usize) -> u16 {
    u16::try_from(
        wrap_text_with_continuation(text, first_width.max(1), continuation_width.max(1)).len(),
    )
    .unwrap_or(u16::MAX)
    .max(1)
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

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_plugin_sdk::interaction::PluginInteraction;
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

    fn render_snapshot(
        renderer: &mut QuestionTerminalRenderer,
        snapshot: &QuestionSnapshot,
        area: Rect,
    ) -> Buffer {
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
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
        let mut frame = Frame::new(&mut buffer);
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
        match renderer.input(event, &snapshot, &TEST_HOST) {
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
        match renderer.input(event, &snapshot, &TEST_HOST) {
            TerminalInteractionInput::Ignored => InteractionOutput::None,
            TerminalInteractionInput::Consumed => InteractionOutput::Redraw,
            TerminalInteractionInput::Semantic(input) => controller.handle_input(input),
        }
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
                renderer.input(
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
        assert!(text.contains("Tab/Shift-Tab or arrows"));
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
        let mut frame = Frame::new(&mut buffer);
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
            renderer.input(&key(KeyCode::Up), &snapshot, &TEST_HOST),
            TerminalInteractionInput::Semantic(InteractionInput::Navigate {
                direction: InteractionNavigation::Previous,
            })
        );
        assert_eq!(
            renderer.input(&key(KeyCode::Down), &snapshot, &TEST_HOST),
            TerminalInteractionInput::Consumed
        );
        assert_eq!(
            renderer.input(&key(KeyCode::Down), &snapshot, &TEST_HOST),
            TerminalInteractionInput::Consumed
        );
        assert_eq!(
            renderer.input(&key(KeyCode::Down), &snapshot, &TEST_HOST),
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
        let input = renderer.input(
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
        let focus = renderer.input(
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
            renderer.input(
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
        let input = renderer.input(
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
