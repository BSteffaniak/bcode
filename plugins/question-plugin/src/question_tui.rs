//! Native TUI surface for the interactive question tool.

use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiAction, PluginTuiHost, PluginTuiSurface,
    PluginTuiSurfaceFactory, PluginTuiSurfaceFuture, PluginTuiSurfaceOpenRequest,
};
use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, MouseButton, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use serde_json::json;

use super::{
    NormalizedQuestionRequest, Question, QuestionAnswerPayload, QuestionCustomMode,
    QuestionSelectionMode,
};

/// Native inline TUI surface kind for question requests.
pub const QUESTION_INLINE_SURFACE: &str = "bcode.question.inline";

/// Factory for inline question surfaces.
pub struct QuestionInlineSurfaceFactory;

impl PluginTuiSurfaceFactory for QuestionInlineSurfaceFactory {
    fn surface_kind(&self) -> &'static str {
        QUESTION_INLINE_SURFACE
    }

    fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        Box::pin(async move {
            let request = serde_json::from_value::<NormalizedQuestionRequest>(request.options)?;
            Ok(Box::new(QuestionInlineSurface::new(request)) as BoxedPluginTuiSurface)
        })
    }
}

struct QuestionInlineSurface {
    request: NormalizedQuestionRequest,
    answers: Vec<QuestionAnswerPayload>,
    focus: FocusTarget,
    last_area: Rect,
    controls: Vec<ControlRegion>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusTarget {
    Question(usize),
    Custom(usize),
    Submit,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlKind {
    Option { question: usize, option: usize },
    Custom { question: usize },
    Submit,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ControlRegion {
    area: Rect,
    kind: ControlKind,
}

impl QuestionInlineSurface {
    fn new(request: NormalizedQuestionRequest) -> Self {
        let answers = request
            .questions
            .iter()
            .enumerate()
            .map(|(question_index, _)| QuestionAnswerPayload {
                question_index,
                selected: Vec::new(),
                custom: None,
            })
            .collect();
        Self {
            request,
            answers,
            focus: FocusTarget::Question(0),
            last_area: Rect::default(),
            controls: Vec::new(),
        }
    }

    fn focus_next(&mut self) {
        let targets = self.focus_targets();
        if targets.is_empty() {
            return;
        }
        let index = targets
            .iter()
            .position(|target| *target == self.focus)
            .unwrap_or(0);
        self.focus = targets[(index + 1) % targets.len()];
    }

    fn focus_previous(&mut self) {
        let targets = self.focus_targets();
        if targets.is_empty() {
            return;
        }
        let index = targets
            .iter()
            .position(|target| *target == self.focus)
            .unwrap_or(0);
        self.focus = targets[(index + targets.len() - 1) % targets.len()];
    }

    fn focus_targets(&self) -> Vec<FocusTarget> {
        let mut targets = Vec::new();
        for (index, question) in self.request.questions.iter().enumerate() {
            targets.push(FocusTarget::Question(index));
            if question.custom {
                targets.push(FocusTarget::Custom(index));
            }
        }
        targets.push(FocusTarget::Submit);
        targets.push(FocusTarget::Cancel);
        targets
    }

    fn toggle_option(&mut self, question_index: usize, option_index: usize) {
        let Some(question) = self.request.questions.get(question_index) else {
            return;
        };
        let Some(option) = question.options.get(option_index) else {
            return;
        };
        let value = option
            .value
            .clone()
            .unwrap_or_else(|| option_index.to_string());
        let answer = &mut self.answers[question_index];
        if question.selection_mode == QuestionSelectionMode::Multiple {
            if let Some(index) = answer
                .selected
                .iter()
                .position(|selected| selected == &value)
            {
                answer.selected.remove(index);
            } else {
                answer.selected.push(value);
            }
        } else {
            answer.selected = vec![value];
            if question.custom_mode == QuestionCustomMode::Exclusive {
                answer.custom = None;
            }
        }
    }

    fn append_custom_char(&mut self, question_index: usize, character: char) {
        let answer = &mut self.answers[question_index];
        let mut text = answer.custom.take().unwrap_or_default();
        text.push(character);
        answer.custom = Some(text);
        if self.request.questions[question_index].custom_mode == QuestionCustomMode::Exclusive {
            answer.selected.clear();
        }
    }

    fn backspace_custom(&mut self, question_index: usize) {
        let answer = &mut self.answers[question_index];
        if let Some(text) = &mut answer.custom {
            text.pop();
            if text.is_empty() {
                answer.custom = None;
            }
        }
    }

    fn submit_payload(&self) -> serde_json::Value {
        json!({
            "status": "answered",
            "questions": self.answers,
        })
    }

    fn activate_focused(&mut self) -> PluginTuiAction {
        match self.focus {
            FocusTarget::Question(question_index) => {
                if !self.request.questions[question_index].options.is_empty() {
                    self.toggle_option(question_index, 0);
                }
                PluginTuiAction::Redraw
            }
            FocusTarget::Custom(_) => PluginTuiAction::None,
            FocusTarget::Submit => PluginTuiAction::Close {
                outcome: Some(self.submit_payload()),
            },
            FocusTarget::Cancel => PluginTuiAction::Close { outcome: None },
        }
    }

    fn handle_mouse(&mut self, event: &bmux_tui::event::MouseEvent) -> PluginTuiAction {
        if !matches!(event.kind, MouseEventKind::Up(MouseButton::Left)) {
            return PluginTuiAction::None;
        }
        let Some(control) = self
            .controls
            .iter()
            .find(|control| control.area.contains(event.position))
            .copied()
        else {
            return PluginTuiAction::None;
        };
        match control.kind {
            ControlKind::Option { question, option } => {
                self.focus = FocusTarget::Question(question);
                self.toggle_option(question, option);
                PluginTuiAction::Redraw
            }
            ControlKind::Custom { question } => {
                self.focus = FocusTarget::Custom(question);
                PluginTuiAction::Redraw
            }
            ControlKind::Submit => PluginTuiAction::Close {
                outcome: Some(self.submit_payload()),
            },
            ControlKind::Cancel => PluginTuiAction::Close { outcome: None },
        }
    }

    fn render_line(&self, frame: &mut Frame<'_>, y: &mut u16, line: &Line) {
        if *y >= self.last_area.bottom() {
            return;
        }
        frame.write_line(
            Rect::new(self.last_area.x, *y, self.last_area.width, 1),
            line,
        );
        *y = y.saturating_add(1);
    }

    fn render_title(&self, frame: &mut Frame<'_>, y: &mut u16) {
        self.render_line(
            frame,
            y,
            &Line::from_spans(vec![Span::styled(
                "Question",
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )]),
        );
    }

    fn render_question(
        &mut self,
        frame: &mut Frame<'_>,
        y: &mut u16,
        question_index: usize,
        question: &Question,
    ) {
        let prompt = question.header.as_ref().map_or_else(
            || question.text.clone(),
            |header| format!("{header}: {}", question.text),
        );
        self.render_line(frame, y, &Line::from(prompt));
        self.render_options(frame, y, question_index, question);
        self.render_custom_answer(frame, y, question_index, question);
        self.render_line(frame, y, &Line::from(""));
    }

    fn render_options(
        &mut self,
        frame: &mut Frame<'_>,
        y: &mut u16,
        question_index: usize,
        question: &Question,
    ) {
        for (option_index, option) in question.options.iter().enumerate() {
            let selected = self.answers[question_index].selected.contains(
                &option
                    .value
                    .clone()
                    .unwrap_or_else(|| option_index.to_string()),
            );
            let marker = if question.selection_mode == QuestionSelectionMode::Multiple {
                if selected { "[x]" } else { "[ ]" }
            } else if selected {
                "(*)"
            } else {
                "( )"
            };
            let style = focus_style(self.focus == FocusTarget::Question(question_index));
            self.controls.push(ControlRegion {
                area: Rect::new(self.last_area.x, *y, self.last_area.width, 1),
                kind: ControlKind::Option {
                    question: question_index,
                    option: option_index,
                },
            });
            self.render_line(
                frame,
                y,
                &Line::from_spans(vec![Span::styled(
                    format!("  {marker} {}", option.label),
                    style,
                )]),
            );
        }
    }

    fn render_custom_answer(
        &mut self,
        frame: &mut Frame<'_>,
        y: &mut u16,
        question_index: usize,
        question: &Question,
    ) {
        if !question.options.is_empty() && !question.custom {
            return;
        }
        let label = if question.options.is_empty() {
            "Answer"
        } else {
            "Custom answer"
        };
        let value = self.answers[question_index]
            .custom
            .clone()
            .unwrap_or_default();
        self.controls.push(ControlRegion {
            area: Rect::new(self.last_area.x, *y, self.last_area.width, 1),
            kind: ControlKind::Custom {
                question: question_index,
            },
        });
        self.render_line(
            frame,
            y,
            &Line::from_spans(vec![Span::styled(
                format!("  {label}: {value}"),
                focus_style(self.focus == FocusTarget::Custom(question_index)),
            )]),
        );
    }

    fn render_actions(&mut self, frame: &mut Frame<'_>, y: &mut u16) {
        self.controls.push(ControlRegion {
            area: Rect::new(self.last_area.x, *y, 10, 1),
            kind: ControlKind::Submit,
        });
        self.controls.push(ControlRegion {
            area: Rect::new(self.last_area.x.saturating_add(11), *y, 10, 1),
            kind: ControlKind::Cancel,
        });
        self.render_line(
            frame,
            y,
            &Line::from_spans(vec![
                Span::styled("[ Submit ]", focus_style(self.focus == FocusTarget::Submit)),
                Span::raw(" "),
                Span::styled("[ Cancel ]", focus_style(self.focus == FocusTarget::Cancel)),
            ]),
        );
    }
}

const fn focus_style(focused: bool) -> Style {
    if focused {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new()
    }
}

impl PluginTuiSurface for QuestionInlineSurface {
    fn id(&self) -> &'static str {
        "question-inline"
    }

    fn title(&self) -> &'static str {
        "Question"
    }

    fn preferred_height(&mut self, _width: u16) -> u16 {
        let mut height = 2_u16;
        for question in &self.request.questions {
            height = height.saturating_add(1);
            height =
                height.saturating_add(u16::try_from(question.options.len()).unwrap_or(u16::MAX));
            if question.options.is_empty() || question.custom {
                height = height.saturating_add(1);
            }
            height = height.saturating_add(1);
        }
        height.saturating_add(1)
    }

    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        self.last_area = area;
        self.controls.clear();
        let mut y = area.y;
        self.render_title(frame, &mut y);
        let questions = self.request.questions.clone();
        for (question_index, question) in questions.iter().enumerate() {
            self.render_question(frame, &mut y, question_index, question);
        }
        self.render_actions(frame, &mut y);
    }

    fn handle_event(&mut self, event: &Event, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        match event {
            Event::Key(stroke) if stroke.modifiers.is_empty() => match stroke.key {
                KeyCode::Tab | KeyCode::Down => {
                    self.focus_next();
                    PluginTuiAction::Redraw
                }
                KeyCode::Up => {
                    self.focus_previous();
                    PluginTuiAction::Redraw
                }
                KeyCode::Enter | KeyCode::Space => self.activate_focused(),
                KeyCode::Escape => PluginTuiAction::Close { outcome: None },
                KeyCode::Backspace => {
                    if let FocusTarget::Custom(question) = self.focus {
                        self.backspace_custom(question);
                        PluginTuiAction::Redraw
                    } else {
                        PluginTuiAction::None
                    }
                }
                KeyCode::Char(character) => {
                    if let FocusTarget::Custom(question) = self.focus {
                        self.append_custom_char(question, character);
                        PluginTuiAction::Redraw
                    } else if character == ' ' {
                        self.activate_focused()
                    } else {
                        PluginTuiAction::None
                    }
                }
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Delete
                | KeyCode::Insert
                | KeyCode::F(_) => PluginTuiAction::None,
            },
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Paste(text) => {
                if let FocusTarget::Custom(question) = self.focus {
                    for character in text.chars() {
                        self.append_custom_char(question, character);
                    }
                    PluginTuiAction::Redraw
                } else {
                    PluginTuiAction::None
                }
            }
            Event::Key(_) | Event::Resize(_) | Event::Focus(_) | Event::Tick | Event::User(_) => {
                PluginTuiAction::None
            }
        }
    }
}
