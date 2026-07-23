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

const OPTION_PREFIX_WIDTH: usize = 8;
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
            let number = option_index.saturating_add(1);
            let prefix = format!("  {number}. {marker} ");
            let continuation = " ".repeat(OPTION_PREFIX_WIDTH.max(prefix.len()));
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
                let prefix_width = OPTION_PREFIX_WIDTH.max(option_index.to_string().len() + 7);
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
                let prefix_width = OPTION_PREFIX_WIDTH.max(option_index.to_string().len() + 7);
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
                KeyCode::Tab | KeyCode::Down | KeyCode::Right => Some(InteractionInput::Navigate {
                    direction: InteractionNavigation::Next,
                }),
                KeyCode::Up | KeyCode::Left => Some(InteractionInput::Navigate {
                    direction: InteractionNavigation::Previous,
                }),
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
                        && let Some(option_index) = character
                            .to_digit(10)
                            .and_then(|digit| usize::try_from(digit).ok())
                            .and_then(|digit| digit.checked_sub(1))
                            .filter(|option_index| {
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
