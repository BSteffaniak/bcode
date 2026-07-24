//! Native TUI renderer for the interactive question tool.

use bcode_plugin_sdk::tui::TerminalInteractionRenderer;
use bcode_tool::{InteractionControlId, InteractionInput, InteractionNavigation, InteractionValue};
use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, MouseButton, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui::text_width::wrap_text_with_continuation;

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
    viewport_offset: u16,
    content_height: u16,
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
        let visible_y = content_y.checked_sub(self.viewport_offset)?;
        (visible_y < self.last_area.height).then(|| self.last_area.y.saturating_add(visible_y))
    }

    fn control_area(&self, content_y: u16, height: u16) -> Option<Rect> {
        let first_y = content_y.max(self.viewport_offset);
        let last_y = content_y
            .saturating_add(height)
            .min(self.viewport_offset.saturating_add(self.last_area.height));
        (first_y < last_y).then(|| {
            Rect::new(
                self.last_area.x,
                self.last_area
                    .y
                    .saturating_add(first_y.saturating_sub(self.viewport_offset)),
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
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
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
        self.render_wrapped(frame, content_y, &prompt, "", "", Style::default());
        for (option_index, option) in question.options.iter().enumerate() {
            let start_y = *content_y;
            let option_id = option_control_id(question_index, option_index);
            let value = option
                .value
                .clone()
                .unwrap_or_else(|| option_index.to_string());
            let selected = snapshot.answers[question_index].selected.contains(&value);
            let marker = if question.selection_mode == QuestionSelectionMode::Multiple {
                if selected { "[x]" } else { "[ ]" }
            } else if selected {
                "(*)"
            } else {
                "( )"
            };
            let shortcut = option_shortcut_label(option_index);
            let shortcut_width = shortcut.len();
            let prefix = format!("  {shortcut}. {marker} ");
            let continuation = " ".repeat(7_usize.saturating_add(shortcut_width));
            let focused = matches!(
                snapshot.focus,
                QuestionFocusTarget::Option {
                    question_index: focused_question,
                    option_index: focused_option,
                } if focused_question == question_index && focused_option == option_index
            );
            self.render_wrapped(
                frame,
                content_y,
                &option.label,
                &prefix,
                &continuation,
                option_style(focused, selected),
            );
            if let Some(description) = option.description.as_deref() {
                self.render_wrapped(
                    frame,
                    content_y,
                    description,
                    DESCRIPTION_INDENT,
                    DESCRIPTION_INDENT,
                    Style::new().fg(Color::BrightBlack),
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
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
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
            .clone()
            .unwrap_or_default();
        let text = format!("{label}: {value}");
        let start_y = *content_y;
        let control_id = custom_control_id(question_index);
        self.render_wrapped(
            frame,
            content_y,
            &text,
            "  ",
            "  ",
            focus_style(matches!(
                snapshot.focus,
                QuestionFocusTarget::Custom { question_index: focused }
                    if focused == question_index
            )),
        );
        if let Some(area) = self.control_area(start_y, content_y.saturating_sub(start_y)) {
            self.controls.push(ControlRegion { area, control_id });
        }
    }

    fn render_actions(
        &mut self,
        frame: &mut Frame<'_>,
        content_y: &mut u16,
        snapshot: &QuestionSnapshot,
    ) {
        let action_y = *content_y;
        if let Some(area) = self.control_area(action_y, 1) {
            self.controls.push(ControlRegion {
                area: Rect::new(area.x, area.y, area.width.min(10), 1),
                control_id: InteractionControlId::new("submit"),
            });
            self.controls.push(ControlRegion {
                area: Rect::new(
                    area.x.saturating_add(11),
                    area.y,
                    area.width.saturating_sub(11).min(10),
                    1,
                ),
                control_id: InteractionControlId::new("cancel"),
            });
        }
        self.render_line(
            frame,
            content_y,
            &Line::from_spans(vec![
                Span::styled(
                    "[ Submit ]",
                    focus_style(snapshot.focus == QuestionFocusTarget::Submit),
                ),
                Span::raw(" "),
                Span::styled(
                    "[ Cancel ]",
                    focus_style(snapshot.focus == QuestionFocusTarget::Cancel),
                ),
            ]),
        );
        self.render_wrapped(
            frame,
            content_y,
            "Tab/Shift-Tab or arrows move · Enter/Space selects · Esc dismisses · transcript scroll keys remain available",
            "",
            "",
            Style::new().fg(Color::BrightBlack),
        );
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
                let label = if question.options.is_empty() {
                    "Answer"
                } else {
                    "Custom answer"
                };
                let available = width.saturating_sub(2).max(1);
                height = height.saturating_add(wrapped_height(
                    &format!("{label}: {value}"),
                    available,
                    available,
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
                let label = if question.options.is_empty() {
                    "Answer"
                } else {
                    "Custom answer"
                };
                let available = width.saturating_sub(2).max(1);
                y = y.saturating_add(wrapped_height(
                    &format!("{label}: {value}"),
                    available,
                    available,
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

    fn ensure_focus_visible(&mut self, snapshot: &QuestionSnapshot, width: u16, height: u16) {
        self.content_height = Self::content_height(snapshot, width);
        if height == 0 {
            self.viewport_offset = 0;
            return;
        }
        let (focus_start, focus_end) = Self::focused_content_range(snapshot, width);
        if focus_start < self.viewport_offset {
            self.viewport_offset = focus_start;
        } else if focus_end > self.viewport_offset.saturating_add(height) {
            self.viewport_offset = focus_end.saturating_sub(height);
        }
        self.viewport_offset = self
            .viewport_offset
            .min(self.content_height.saturating_sub(height));
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
        self.last_area = area;
        self.controls.clear();
        self.ensure_focus_visible(snapshot, area.width, area.height);
        frame.fill(area, " ", Style::new().bg(Color::Black));
        let mut content_y = 0;
        self.render_title(frame, &mut content_y);
        for question_index in 0..snapshot.request.questions.len() {
            self.render_question(frame, &mut content_y, snapshot, question_index);
        }
        self.render_actions(frame, &mut content_y, snapshot);
        if self.viewport_offset > 0 && area.height > 0 {
            frame.write_line(
                Rect::new(area.x, area.y, area.width, 1),
                &Line::from_spans(vec![Span::styled("↑ more", Style::new().fg(Color::Yellow))]),
            );
        }
        if self.viewport_offset.saturating_add(area.height) < self.content_height && area.height > 0
        {
            frame.write_line(
                Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
                &Line::from_spans(vec![Span::styled("↓ more", Style::new().fg(Color::Yellow))]),
            );
        }
    }

    fn input(&mut self, event: &Event, snapshot: &QuestionSnapshot) -> Option<InteractionInput> {
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
            Event::Key(stroke) if stroke.modifiers.is_empty() => match stroke.key {
                KeyCode::Tab | KeyCode::Down => Some(InteractionInput::Navigate {
                    direction: InteractionNavigation::Next,
                }),
                KeyCode::Up => Some(InteractionInput::Navigate {
                    direction: InteractionNavigation::Previous,
                }),
                KeyCode::Right => {
                    if matches!(snapshot.focus, QuestionFocusTarget::Custom { .. }) {
                        None
                    } else {
                        Some(InteractionInput::Navigate {
                            direction: InteractionNavigation::Next,
                        })
                    }
                }
                KeyCode::Left => {
                    if matches!(snapshot.focus, QuestionFocusTarget::Custom { .. }) {
                        None
                    } else {
                        Some(InteractionInput::Navigate {
                            direction: InteractionNavigation::Previous,
                        })
                    }
                }
                KeyCode::Enter | KeyCode::Space => Some(InteractionInput::Activate {
                    control_id: snapshot.focused_control_id.clone(),
                }),
                KeyCode::Escape => Some(InteractionInput::Cancel),
                KeyCode::Backspace => custom_text_change(snapshot, |text| {
                    text.pop();
                }),
                KeyCode::Char(character) => {
                    if matches!(snapshot.focus, QuestionFocusTarget::Custom { .. }) {
                        custom_text_change(snapshot, |text| text.push(character))
                    } else if let QuestionFocusTarget::Option { question_index, .. } =
                        snapshot.focus
                        && let Some(option_index) =
                            option_shortcut(character).filter(|option_index| {
                                *option_index
                                    < snapshot.request.questions[question_index].options.len()
                            })
                    {
                        Some(InteractionInput::Activate {
                            control_id: option_control_id(question_index, option_index),
                        })
                    } else {
                        None
                    }
                }
                KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Delete
                | KeyCode::Insert
                | KeyCode::F(_) => None,
            },
            Event::Mouse(mouse) => self.mouse_input(mouse),
            Event::Paste(text) => custom_text_change(snapshot, |value| value.push_str(text)),
            Event::Key(_) | Event::Resize(_) | Event::Focus(_) | Event::Tick | Event::User(_) => {
                None
            }
        }
    }
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

fn wrapped_height(text: &str, first_width: usize, continuation_width: usize) -> u16 {
    u16::try_from(
        wrap_text_with_continuation(text, first_width.max(1), continuation_width.max(1)).len(),
    )
    .unwrap_or(u16::MAX)
    .max(1)
}

const fn focus_style(focused: bool) -> Style {
    if focused {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new()
    }
}

const fn option_style(focused: bool, selected: bool) -> Style {
    let style = if selected {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    if focused {
        style.add_modifier(Modifier::REVERSED)
    } else {
        style
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_plugin_sdk::interaction::PluginInteraction;
    use bcode_tool::{InteractionInput, InteractionOutput};
    use bmux_keyboard::{KeyStroke, Modifiers};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::geometry::Point;

    use crate::{
        NormalizedQuestionRequest, Question, QuestionControl, QuestionCustomMode, QuestionOption,
    };

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

    fn apply_event(
        renderer: &mut QuestionTerminalRenderer,
        controller: &mut QuestionInteractionController,
        event: &Event,
    ) -> InteractionOutput {
        let snapshot = controller.snapshot();
        renderer
            .input(event, &snapshot)
            .map_or(InteractionOutput::None, |input| {
                controller.handle_input(input)
            })
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
                ),
                None,
                "{key:?} must remain unsupported until cursor-aware editing exists"
            );
        }
        assert_eq!(
            controller.snapshot().answers[0].custom.as_deref(),
            Some("answer")
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
        required.handle_input(InteractionInput::Submit);
        let validation = render_snapshot(
            &mut QuestionTerminalRenderer::default(),
            &required.snapshot(),
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
        assert!(rendered_text(&buffer).contains("more"));
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
    fn oversized_multi_question_form_keeps_focused_controls_visible() {
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
        assert!(rendered_text(&initial).contains("↓ more"));

        for _ in 0..6 {
            controller.handle_input(InteractionInput::Navigate {
                direction: InteractionNavigation::Next,
            });
        }
        let focused = controller.snapshot();
        assert_eq!(focused.focus, QuestionFocusTarget::Submit);
        let scrolled = render_snapshot(&mut renderer, &focused, area);
        assert!(renderer.viewport_offset > 0);
        assert!(renderer.controls.iter().any(|control| {
            control.control_id.as_str() == "submit"
                && control.area.y >= area.y
                && control.area.bottom() <= area.bottom()
        }));
        assert!(rendered_text(&scrolled).contains("↑ more"));
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
        let input = renderer
            .input(
                &Event::Mouse(bmux_tui::event::MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(second.x, second.y),
                )),
                &snapshot,
            )
            .expect("visible option click");
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
