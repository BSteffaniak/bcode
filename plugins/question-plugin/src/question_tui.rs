//! Native TUI surface for the interactive question tool.

use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiAction, PluginTuiHost, PluginTuiSurface,
    PluginTuiSurfaceFactory, PluginTuiSurfaceFuture, PluginTuiSurfaceOpenRequest,
};
use bcode_tool::{
    InteractionControlId, InteractionController, InteractionInput, InteractionNavigation,
    InteractionOutput,
};
use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, MouseButton, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

use super::NormalizedQuestionRequest;
use super::question_interaction::{
    QuestionFocusTarget, QuestionInteractionController, QuestionSnapshot, custom_control_id,
    option_control_id,
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
    controller: QuestionInteractionController,
    last_area: Rect,
    controls: Vec<ControlRegion>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ControlRegion {
    area: Rect,
    control_id: InteractionControlId,
}

impl QuestionInlineSurface {
    fn new(request: NormalizedQuestionRequest) -> Self {
        Self {
            controller: QuestionInteractionController::new(request),
            last_area: Rect::default(),
            controls: Vec::new(),
        }
    }

    fn action_from_output(output: InteractionOutput) -> PluginTuiAction {
        match output {
            InteractionOutput::None => PluginTuiAction::None,
            InteractionOutput::Redraw => PluginTuiAction::Redraw,
            InteractionOutput::Submitted { payload } => PluginTuiAction::Close {
                outcome: Some(payload),
            },
            InteractionOutput::Cancelled => PluginTuiAction::Close { outcome: None },
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
        snapshot: &QuestionSnapshot,
        question_index: usize,
    ) {
        let question = &snapshot.request.questions[question_index];
        let prompt = question.header.as_ref().map_or_else(
            || question.text.clone(),
            |header| format!("{header}: {}", question.text),
        );
        self.render_line(frame, y, &Line::from(prompt));
        for (option_index, option) in question.options.iter().enumerate() {
            let option_id = option_control_id(question_index, option_index);
            let selected = snapshot.answers[question_index].selected.contains(
                &option
                    .value
                    .clone()
                    .unwrap_or_else(|| option_index.to_string()),
            );
            let marker = if question.selection_mode == super::QuestionSelectionMode::Multiple {
                if selected { "[x]" } else { "[ ]" }
            } else if selected {
                "(*)"
            } else {
                "( )"
            };
            self.controls.push(ControlRegion {
                area: Rect::new(self.last_area.x, *y, self.last_area.width, 1),
                control_id: option_id,
            });
            self.render_line(
                frame,
                y,
                &Line::from_spans(vec![Span::styled(
                    format!("  {marker} {}", option.label),
                    focus_style(matches!(
                        snapshot.focus,
                        QuestionFocusTarget::Question { question_index: focused }
                            if focused == question_index
                    )),
                )]),
            );
        }
        self.render_custom_answer(frame, y, snapshot, question_index);
        self.render_line(frame, y, &Line::from(""));
    }

    fn render_custom_answer(
        &mut self,
        frame: &mut Frame<'_>,
        y: &mut u16,
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
        let control_id = custom_control_id(question_index);
        self.controls.push(ControlRegion {
            area: Rect::new(self.last_area.x, *y, self.last_area.width, 1),
            control_id,
        });
        self.render_line(
            frame,
            y,
            &Line::from_spans(vec![Span::styled(
                format!("  {label}: {value}"),
                focus_style(matches!(
                    snapshot.focus,
                    QuestionFocusTarget::Custom { question_index: focused }
                        if focused == question_index
                )),
            )]),
        );
    }

    fn render_actions(&mut self, frame: &mut Frame<'_>, y: &mut u16, snapshot: &QuestionSnapshot) {
        self.controls.push(ControlRegion {
            area: Rect::new(self.last_area.x, *y, 10, 1),
            control_id: InteractionControlId::new("submit"),
        });
        self.controls.push(ControlRegion {
            area: Rect::new(self.last_area.x.saturating_add(11), *y, 10, 1),
            control_id: InteractionControlId::new("cancel"),
        });
        self.render_line(
            frame,
            y,
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
    }

    fn handle_mouse(&mut self, event: &bmux_tui::event::MouseEvent) -> PluginTuiAction {
        if !matches!(event.kind, MouseEventKind::Up(MouseButton::Left)) {
            return PluginTuiAction::None;
        }
        let Some(control_id) = self
            .controls
            .iter()
            .find(|control| control.area.contains(event.position))
            .map(|control| control.control_id.clone())
        else {
            return PluginTuiAction::None;
        };
        Self::action_from_output(
            self.controller
                .handle_input(InteractionInput::Activate { control_id }),
        )
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
        let snapshot = self.controller.snapshot();
        let mut height = 2_u16;
        for question in &snapshot.request.questions {
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
        let snapshot = self.controller.snapshot();
        let mut y = area.y;
        self.render_title(frame, &mut y);
        for question_index in 0..snapshot.request.questions.len() {
            self.render_question(frame, &mut y, &snapshot, question_index);
        }
        self.render_actions(frame, &mut y, &snapshot);
    }

    fn handle_event(&mut self, event: &Event, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        match event {
            Event::Key(stroke) if stroke.modifiers.is_empty() => match stroke.key {
                KeyCode::Tab | KeyCode::Down => Self::action_from_output(
                    self.controller.handle_input(InteractionInput::Navigate {
                        direction: InteractionNavigation::Next,
                    }),
                ),
                KeyCode::Up => Self::action_from_output(self.controller.handle_input(
                    InteractionInput::Navigate {
                        direction: InteractionNavigation::Previous,
                    },
                )),
                KeyCode::Enter | KeyCode::Space => Self::action_from_output(
                    self.controller.handle_input(InteractionInput::Activate {
                        control_id: self.controller.focus().control_id(),
                    }),
                ),
                KeyCode::Escape => {
                    Self::action_from_output(self.controller.handle_input(InteractionInput::Cancel))
                }
                KeyCode::Backspace => {
                    if let QuestionFocusTarget::Custom { question_index } = self.controller.focus()
                    {
                        self.controller.backspace_custom(question_index);
                        PluginTuiAction::Redraw
                    } else {
                        PluginTuiAction::None
                    }
                }
                KeyCode::Char(character) => {
                    if let QuestionFocusTarget::Custom { question_index } = self.controller.focus()
                    {
                        self.controller
                            .append_custom_text(question_index, &character.to_string());
                        PluginTuiAction::Redraw
                    } else if character == ' ' {
                        Self::action_from_output(self.controller.handle_input(
                            InteractionInput::Activate {
                                control_id: self.controller.focus().control_id(),
                            },
                        ))
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
                if let QuestionFocusTarget::Custom { question_index } = self.controller.focus() {
                    self.controller.append_custom_text(question_index, text);
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

const fn focus_style(focused: bool) -> Style {
    if focused {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new()
    }
}
