//! Terminal adaptation of caller-owned startup choices.

use std::io::{self, Write};

use bcode_plugin_sdk::tui::keymap::controls::ControlAction;
#[cfg(test)]
use bmux_keyboard::KeyCode;
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::damage::Damage;
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Style, TextBlock, TextWrap};
use bmux_tui::terminal::Terminal;
use bmux_tui_components::selectable_list::{
    SelectableList, SelectableListHighlightPolicy, SelectableListItem, SelectableListPolicy,
    SelectableListState, SelectableListStyles,
};
use bmux_tui_runtime::{
    Invalidation, Lifecycle, PresentReport, Presenter, Program, RuntimeEvent, Update,
};

use crate::{TuiError, setup_connection_form::ConnectionForm, theme::PresentedTheme};

/// A display-only startup candidate. Its index refers to the caller's selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickConnectionChoice {
    /// User-facing account/model label, without credentials.
    pub label: String,
    /// Explanation supplied by the discovery owner.
    pub detail: String,
}

/// Caller-owned candidates and targeted recovery guidance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuickOnboardingInput {
    /// Choices in caller-defined order; the UI does not rank or discover them.
    pub choices: Vec<QuickConnectionChoice>,
    /// Optional safe recovery text.
    pub recovery: Option<String>,
}

/// Explicit startup action, returned after restoring the terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuickOnboardingOutcome {
    /// Use the candidate at this index.
    Selected(usize),
    /// Enrollment completed successfully; the caller resolves the model.
    Connected {
        /// Authentication provider reference.
        provider_id: String,
        /// Enrolled authentication profile reference.
        profile: String,
    },
    /// Open the existing advanced setup board.
    Advanced,
    /// Close without selecting or enrolling.
    Close,
}

struct QuickProgram {
    input: QuickOnboardingInput,
    selected: usize,
    list: SelectableListState,
    area: Rect,
    choice_area: Rect,
    outcome: QuickOnboardingOutcome,
    form: Option<ConnectionForm>,
    theme: PresentedTheme,
    keymap: crate::keymap::BmuxKeyMap,
}

enum QuickMessage {
    Progress,
    InputFailed(io::Error),
}

impl QuickProgram {
    fn new(input: QuickOnboardingInput, theme: &PresentedTheme) -> Self {
        Self {
            input,
            selected: 0,
            list: SelectableListState::new(Some(0)),
            area: Rect::new(0, 0, 0, 0),
            choice_area: Rect::new(0, 0, 0, 0),
            outcome: QuickOnboardingOutcome::Close,
            form: None,
            theme: *theme,
            keymap: crate::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
        }
    }

    fn progress() -> bmux_tui_runtime::Command<QuickMessage> {
        bmux_tui_runtime::Command::concurrent(async {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Some(QuickMessage::Progress)
        })
    }

    #[cfg(test)]
    fn key(&mut self, key: KeyCode) -> Lifecycle {
        self.handle_stroke(bmux_keyboard::KeyStroke::simple(key))
    }

    fn handle_stroke(&mut self, stroke: bmux_keyboard::KeyStroke) -> Lifecycle {
        match self.keymap.action("quick_onboarding", stroke) {
            Some("tui.control.cancel") => return Lifecycle::Exit,
            Some("tui.quick.advanced") => {
                self.outcome = QuickOnboardingOutcome::Advanced;
                return Lifecycle::Exit;
            }
            Some("tui.quick.connect") => self.form = Some(ConnectionForm::quick()),
            Some(action)
                if matches!(
                    action,
                    "tui.control.up"
                        | "tui.control.down"
                        | "tui.control.page_up"
                        | "tui.control.page_down"
                        | "tui.control.home"
                        | "tui.control.end"
                ) =>
            {
                let Some(action) = ControlAction::from_id(
                    action,
                    &[
                        ControlAction::Up,
                        ControlAction::Down,
                        ControlAction::PageUp,
                        ControlAction::PageDown,
                        ControlAction::Home,
                        ControlAction::End,
                    ],
                ) else {
                    return Lifecycle::Continue;
                };
                let items = self.items();
                let area = self.list_area();
                let _ = choice_list(&items).handle_event(
                    area,
                    &mut self.list,
                    &Event::Key(action.native_stroke()),
                );
                self.selected = self.list.focused().unwrap_or(0);
                self.list.set_selected(Some(self.selected));
            }
            Some("tui.control.submit") if self.input.choices.is_empty() => {
                self.form = Some(ConnectionForm::quick());
            }
            Some("tui.control.submit") => {
                self.outcome = QuickOnboardingOutcome::Selected(self.selected);
                return Lifecycle::Exit;
            }
            _ => {}
        }
        Lifecycle::Continue
    }

    fn items(&self) -> Vec<SelectableListItem> {
        self.input
            .choices
            .iter()
            .enumerate()
            .map(|(index, choice)| {
                SelectableListItem::multiline(
                    index.to_string(),
                    wrapped_rows(&choice.label, self.area.width, self.theme.text),
                )
            })
            .collect()
    }

    fn handle_connection_event(&mut self, event: &Event) -> Lifecycle {
        if !self
            .form
            .as_mut()
            .is_some_and(|form| form.handle_event_with_keymap(event, &self.keymap))
        {
            return Lifecycle::Continue;
        }
        if let Some((provider_id, profile)) =
            self.form.as_mut().and_then(ConnectionForm::completion)
        {
            self.outcome = QuickOnboardingOutcome::Connected {
                provider_id,
                profile,
            };
            Lifecycle::Exit
        } else {
            self.form = None;
            Lifecycle::Continue
        }
    }

    const fn list_area(&self) -> Rect {
        self.choice_area
    }

    fn footer_rows(&self, width: u16) -> Vec<Line> {
        let mut footer = Vec::new();
        if let Some(choice) = self.input.choices.get(self.selected) {
            footer.extend(wrapped_rows(&choice.detail, width, self.theme.muted));
            footer.extend(wrapped_rows(
                &format!(
                    "{}: Use this and start",
                    self.keymap.hint("quick_onboarding", "tui.control.submit")
                ),
                width,
                self.theme.focused,
            ));
        } else {
            footer.extend(wrapped_rows(
                &format!(
                    "{}: Sign in / Add key",
                    self.keymap.hint("quick_onboarding", "tui.control.submit")
                ),
                width,
                self.theme.focused,
            ));
        }
        footer.extend(wrapped_rows(
            &format!(
                "{}: Connect another (Sign in / Add key) • {}: Advanced",
                self.keymap.hint("quick_onboarding", "tui.quick.connect"),
                self.keymap.hint("quick_onboarding", "tui.quick.advanced")
            ),
            width,
            self.theme.text,
        ));
        footer.extend(wrapped_rows(
            &format!(
                "{}/{}: choose • {}: Close",
                self.keymap.hint("quick_onboarding", "tui.control.up"),
                self.keymap.hint("quick_onboarding", "tui.control.down"),
                self.keymap.hint("quick_onboarding", "tui.control.cancel")
            ),
            width,
            self.theme.muted,
        ));
        footer
    }

    fn render(&mut self, frame: &mut PaintCx<'_, '_>) {
        let local = frame.area();
        self.area = Rect::new(0, 0, local.width, local.height);
        self.choice_area = Rect::new(0, 0, 0, 0);
        if let Some(form) = &mut self.form {
            form.render_with_keymap(frame, &self.theme, &self.keymap);
            return;
        }
        let width = self.area.width;
        let mut header = wrapped_rows("Welcome to Bcode", width, self.theme.focused);
        header.extend(wrapped_rows(
            self.input
                .recovery
                .as_deref()
                .unwrap_or("Choose an account and model to start coding."),
            width,
            self.theme.text,
        ));
        if self.input.choices.is_empty() {
            header.extend(wrapped_rows(
                "No usable account is available.",
                width,
                self.theme.text,
            ));
        }
        let footer = self.footer_rows(width);
        let reserved = header.len().saturating_add(footer.len());
        let minimum = reserved.saturating_add(usize::from(!self.input.choices.is_empty()));
        if width == 0 || minimum > usize::from(self.area.height) {
            // Explicit tiny-layout degradation: no invisible choice hit regions.
            paint_rows(
                frame,
                0,
                &wrapped_rows(
                    &format!(
                        "Resize to choose • {}: continue • {}: connect • {}: Advanced • {}: close",
                        self.keymap.hint("quick_onboarding", "tui.control.submit"),
                        self.keymap.hint("quick_onboarding", "tui.quick.connect"),
                        self.keymap.hint("quick_onboarding", "tui.quick.advanced"),
                        self.keymap.hint("quick_onboarding", "tui.control.cancel")
                    ),
                    width,
                    self.theme.text,
                ),
            );
            return;
        }
        let header_height = u16::try_from(header.len()).unwrap_or(u16::MAX);
        let footer_height = u16::try_from(footer.len()).unwrap_or(u16::MAX);
        self.choice_area = Rect::new(
            0,
            header_height,
            width,
            self.area
                .height
                .saturating_sub(header_height)
                .saturating_sub(footer_height),
        );
        paint_rows(frame, 0, &header);
        if !self.input.choices.is_empty() {
            let items = self.items();
            choice_list(&items)
                .styles(SelectableListStyles {
                    normal: self.theme.text,
                    focused: self.theme.focused,
                    selected: self.theme.focused,
                    background: self.theme.canvas,
                    ..SelectableListStyles::default()
                })
                .paint(self.choice_area, &self.list, self.theme.text, frame);
        }
        paint_rows(
            frame,
            self.area.height.saturating_sub(footer_height),
            &footer,
        );
    }
}

// No prefix is reserved: the focused/selected style identifies choices, and the
// entire measured width is available to their labels, even on narrow terminals.
fn choice_list(items: &[SelectableListItem]) -> SelectableList<'_> {
    SelectableList::new(items).policy(SelectableListPolicy {
        highlight: SelectableListHighlightPolicy::new("", false),
        ..SelectableListPolicy::default()
    })
}

fn wrapped_rows(text: &str, width: u16, style: Style) -> Vec<Line> {
    if width == 0 {
        return Vec::new();
    }
    let text = TextBlock::new(text).style(style).wrap(TextWrap::Word);
    let layout = text.layout(
        Constraints::new(0, u64::from(width), 0, None),
        &mut LayoutCx::new(),
    );
    text.projection(&layout)
        .into_iter()
        .map(|row| {
            let mut line = row.line;
            for span in &mut line.spans {
                span.style = style.patch(span.style);
            }
            line
        })
        .collect()
}

fn paint_rows(frame: &mut PaintCx<'_, '_>, start: u16, rows: &[Line]) {
    for (offset, row) in rows
        .iter()
        .take(usize::from(frame.area().height.saturating_sub(start)))
        .enumerate()
    {
        frame.write_line_with_fallback_style(
            LocalRect::new(
                0,
                i64::from(start) + i64::try_from(offset).unwrap_or(i64::MAX),
                frame.area().width,
                1,
            ),
            row,
            Style::new(),
        );
    }
}

impl Program for QuickProgram {
    type Message = QuickMessage;
    type Error = TuiError;

    fn update(
        &mut self,
        event: RuntimeEvent<Self::Message>,
    ) -> Result<Update<Self::Message>, Self::Error> {
        let poll =
            matches!(event, RuntimeEvent::Message(QuickMessage::Progress)) || self.form.is_none();
        let mut lifecycle = Lifecycle::Continue;
        match event {
            RuntimeEvent::Message(QuickMessage::InputFailed(error)) => return Err(error.into()),
            RuntimeEvent::Terminal(Event::Key(key))
                if self.keymap.action("quick_onboarding", key) == Some("tui.quick.advanced") =>
            {
                self.outcome = QuickOnboardingOutcome::Advanced;
                lifecycle = Lifecycle::Exit;
            }
            RuntimeEvent::Terminal(ref event) if self.form.is_some() => {
                lifecycle = self.handle_connection_event(event);
            }
            RuntimeEvent::Terminal(Event::Key(key)) => lifecycle = self.handle_stroke(key),
            _ => {}
        }
        if let Some((provider_id, profile)) =
            self.form.as_mut().and_then(ConnectionForm::completion)
        {
            self.outcome = QuickOnboardingOutcome::Connected {
                provider_id,
                profile,
            };
            lifecycle = Lifecycle::Exit;
        }
        Ok(Update {
            lifecycle,
            invalidation: Invalidation::Redraw,
            commands: if poll && self.form.is_some() && lifecycle == Lifecycle::Continue {
                vec![Self::progress()]
            } else {
                Vec::new()
            },
            ..Update::none()
        })
    }
}

struct QuickPresenter<'a, 'b, W> {
    terminal: &'a mut Terminal<&'b mut W>,
}

impl<W: Write> Presenter<QuickProgram> for QuickPresenter<'_, '_, W> {
    type Error = io::Error;
    fn resize(&mut self, size: bmux_tui::geometry::Size) {
        self.terminal
            .resize(Rect::new(0, 0, size.width, size.height));
    }
    fn reset(&mut self, _reason: bmux_tui_runtime::ResetReason) {
        self.terminal.reset();
    }
    fn present(&mut self, program: &mut QuickProgram) -> Result<PresentReport, Self::Error> {
        let stats = self
            .terminal
            .draw_damage(Damage::Full, |frame| program.render(frame))?;
        Ok(PresentReport {
            changed_cells: stats.changed_cells,
            full_repaint: stats.full_repaint,
        })
    }
}

/// Present a quick startup choice or enroll through the existing secure provider lifecycle.
/// No candidate discovery, model ranking, or startup-state persistence occurs here.
///
/// # Errors
/// Returns configuration or terminal errors, after attempting to restore the terminal.
pub async fn run_quick_onboarding(
    input: QuickOnboardingInput,
) -> Result<QuickOnboardingOutcome, TuiError> {
    let config = bcode_config::load_config()?;
    let theme = crate::theme::resolve_configured_theme(&config.tui, std::path::Path::new("."));
    let mut program = QuickProgram::new(input, &theme);
    program.keymap = crate::keymap::BmuxKeyMap::from_config(&config.tui);
    let mut guard = crate::CrosstermTerminalGuard::enter(io::stdout())?;
    let result = {
        let mut terminal = Terminal::new(
            guard
                .writer_mut()
                .ok_or_else(|| io::Error::other("terminal writer unavailable"))?,
            crate::terminal_area()?,
        );
        let (runtime, handle) = bmux_tui_runtime::Runtime::new(
            program,
            QuickPresenter {
                terminal: &mut terminal,
            },
            bmux_tui_runtime::RuntimeConfig::default(),
        );
        let input = bmux_tui_runtime::TerminalInput::start::<QuickProgram>(
            handle,
            QuickMessage::InputFailed,
        );
        let result = Box::pin(runtime.run()).await;
        input.request_shutdown();
        match result {
            Ok(output) => Ok(output.program.outcome),
            Err(bmux_tui_runtime::RuntimeError::Program { error, .. }) => Err(error),
            Err(bmux_tui_runtime::RuntimeError::Presenter { error, .. }) => Err(error.into()),
        }
    };
    let _writer = guard.leave()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn program() -> QuickProgram {
        QuickProgram::new(
            QuickOnboardingInput {
                choices: vec![QuickConnectionChoice {
                    label: "账户 🦀 e\u{301}".to_owned(),
                    detail: "Ready".to_owned(),
                }],
                recovery: None,
            },
            &crate::theme::resolve_configured_theme(
                &bcode_config::TuiConfig::default(),
                std::path::Path::new("."),
            ),
        )
    }
    #[test]
    fn one_confirmation_selects_without_enrollment() {
        let mut program = program();
        assert_eq!(program.key(KeyCode::Enter), Lifecycle::Exit);
        assert_eq!(program.outcome, QuickOnboardingOutcome::Selected(0));
        assert!(program.form.is_none());
    }
    #[test]
    fn close_and_advanced_do_not_enroll() {
        let mut program = program();
        assert_eq!(program.key(KeyCode::Escape), Lifecycle::Exit);
        assert_eq!(program.outcome, QuickOnboardingOutcome::Close);
        assert!(program.form.is_none());
        assert_eq!(program.key(KeyCode::F(2)), Lifecycle::Exit);
        assert_eq!(program.outcome, QuickOnboardingOutcome::Advanced);
    }
    #[test]
    fn remaps_dispatch_and_hints_without_default_fallbacks() {
        let mut program = program();
        program.input.choices.push(QuickConnectionChoice {
            label: "Second".into(),
            detail: "Ready".into(),
        });
        let mut config = bcode_config::TuiConfig::default();
        config.keybindings.scopes.insert(
            "quick_onboarding".into(),
            [
                ("ctrl+j", "tui.control.down"),
                ("ctrl+y", "tui.control.submit"),
                ("ctrl+a", "tui.quick.advanced"),
                ("c", ""),
                ("escape", ""),
                ("q", ""),
            ]
            .into_iter()
            .map(|(key, action)| (key.into(), action.into()))
            .collect(),
        );
        program.keymap = crate::keymap::BmuxKeyMap::from_config(&config);
        assert!(program.keymap.error().is_none());
        for key in [
            KeyCode::Down,
            KeyCode::Enter,
            KeyCode::F(2),
            KeyCode::Char('c'),
            KeyCode::Escape,
            KeyCode::Char('q'),
        ] {
            assert_eq!(program.key(key), Lifecycle::Continue);
        }
        assert!(program.form.is_none());
        assert_eq!(program.selected, 0);
        let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 100, 24));
        program.render(&mut PaintCx::new(&mut bmux_tui::frame::Frame::new(
            &mut buffer,
        )));
        let rendered = (0..24)
            .filter_map(|row| buffer.row_symbols(row))
            .collect::<String>();
        for (action, label) in [
            ("tui.control.submit", "Use this and start"),
            ("tui.quick.advanced", "Advanced"),
            ("tui.quick.connect", "Connect another"),
            ("tui.control.cancel", "Close"),
        ] {
            assert!(rendered.contains(&format!(
                "{}: {label}",
                program.keymap.hint("quick_onboarding", action)
            )));
        }
        program.handle_stroke(bmux_keyboard::parse_key_stroke("ctrl+j").unwrap());
        assert_eq!(program.selected, 1);
        assert_eq!(
            program.handle_stroke(bmux_keyboard::parse_key_stroke("ctrl+y").unwrap()),
            Lifecycle::Exit
        );
        assert_eq!(program.outcome, QuickOnboardingOutcome::Selected(1));
    }

    #[test]
    fn remapped_advanced_is_effective_during_enrollment() {
        let mut program = program();
        let mut config = bcode_config::TuiConfig::default();
        config.keybindings.scopes.insert(
            "quick_onboarding".into(),
            [("ctrl+a".into(), "tui.quick.advanced".into())].into(),
        );
        program.keymap = crate::keymap::BmuxKeyMap::from_config(&config);
        program.form = Some(ConnectionForm::quick());
        let event = |key| RuntimeEvent::Terminal(Event::Key(key));
        assert_eq!(
            program
                .update(event(bmux_keyboard::KeyStroke::simple(KeyCode::F(2))))
                .unwrap()
                .lifecycle,
            Lifecycle::Continue
        );
        assert_eq!(
            program
                .update(event(bmux_keyboard::parse_key_stroke("ctrl+a").unwrap()))
                .unwrap()
                .lifecycle,
            Lifecycle::Exit
        );
        assert_eq!(program.outcome, QuickOnboardingOutcome::Advanced);
    }

    #[test]
    fn unicode_and_recovery_render_inside_zero_and_tiny_frames() {
        for (width, height) in [(0, 0), (1, 1), (2, 4), (18, 12), (80, 24)] {
            let mut program = program();
            program.input.recovery = Some("重新连接 e\u{301} 🦀".to_owned());
            let area = Rect::new(0, 0, width, height);
            let mut buffer = bmux_tui::buffer::Buffer::empty(area);
            program.render(&mut bmux_tui::paint::PaintCx::new(
                &mut bmux_tui::frame::Frame::new(&mut buffer),
            ));
            if width == 80 {
                let rows = (0..height)
                    .map(|row| buffer.row_symbols(row).unwrap())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(rows.contains("账户"));
                assert!(rows.contains("重新连接"));
                assert!(rows.contains(&format!(
                        "{}: Close",
                        program
                            .keymap
                            .hint("quick_onboarding", "tui.control.cancel")
                    )));
            }
        }
    }

    #[test]
    fn wide_graphemes_are_omitted_in_one_cell_viewport_without_spilling() {
        let mut program = program();
        program.input.recovery = Some("界🦀e\u{301}".to_owned());
        let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 4, 24));
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
        PaintCx::new(&mut frame)
            .with_child(0, 0, LocalRect::new(0, 0, 1, 24), |cx| program.render(cx));
        let recovery = buffer.row_symbols(1).unwrap();
        assert!(!recovery.contains('界'));
        assert!(!recovery.contains('🦀'));
        assert!(recovery.chars().skip(1).all(|character| character == ' '));
    }

    #[test]
    fn compact_chooser_shows_multiple_accounts_and_primary_actions() {
        let mut program = program();
        program.input.choices.push(QuickConnectionChoice {
            label: "Second account".to_owned(),
            detail: "Ready too".to_owned(),
        });
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        program.render(&mut PaintCx::new(&mut bmux_tui::frame::Frame::new(
            &mut buffer,
        )));
        let rows = (0..24)
            .map(|row| buffer.row_symbols(row).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rows.contains("账户"));
        assert!(rows.contains("Second account"));
        assert!(rows.contains("Use this and start"));
        assert!(rows.contains("Connect another"));
        assert!(rows.contains("Advanced"));
        program.key(KeyCode::Down);
        assert_eq!(program.key(KeyCode::Enter), Lifecycle::Exit);
        assert_eq!(program.outcome, QuickOnboardingOutcome::Selected(1));
    }

    #[test]
    fn no_account_visibly_offers_sign_in_without_implicitly_enrolling() {
        let mut program = program();
        program.input.choices.clear();
        let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 80, 24));
        program.render(&mut PaintCx::new(&mut bmux_tui::frame::Frame::new(
            &mut buffer,
        )));
        let rows = (0..24)
            .map(|row| buffer.row_symbols(row).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rows.contains("Sign in / Add key"));
        assert!(program.form.is_none());
    }

    #[test]
    fn progress_never_implicitly_enrolls() {
        let mut program = program();
        let update = program
            .update(RuntimeEvent::Message(QuickMessage::Progress))
            .unwrap();
        assert!(program.form.is_none());
        assert!(update.commands.is_empty());
        assert_eq!(program.outcome, QuickOnboardingOutcome::Close);
    }

    #[test]
    fn wrapped_guidance_details_and_actions_remain_complete() {
        let mut program = program();
        program.input.recovery =
            Some("Choose the detected account, or connect another account.".to_owned());
        program.input.choices[0].detail =
            "Unknown metadata remains unknown; detection is not authentication.".to_owned();
        let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 24, 30));
        program.render(&mut PaintCx::new(&mut bmux_tui::frame::Frame::new(
            &mut buffer,
        )));
        let text = (0..30)
            .map(|y| buffer.row_symbols(y).unwrap())
            .collect::<Vec<_>>()
            .join(" ");
        for word in ["detected", "authentication.", "Advanced", "Close", "start"] {
            assert!(text.contains(word), "missing {word}: {text}");
        }
        assert!(program.choice_area.height > 0);
    }

    #[test]
    fn measured_styled_unicode_rows_are_painted_without_rewrapping() {
        let style = Style::new().fg(bmux_tui::style::Color::Red);
        let rows = wrapped_rows("账户 🦀 e\u{301} styled words", 8, style);
        assert!(rows.len() > 1);
        assert!(
            rows.iter()
                .flat_map(|row| &row.spans)
                .all(|span| span.style == style)
        );
        let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 8, 10));
        paint_rows(
            &mut PaintCx::new(&mut bmux_tui::frame::Frame::new(&mut buffer)),
            0,
            &rows,
        );
        let text = (0..10)
            .map(|y| buffer.row_symbols(y).unwrap())
            .collect::<Vec<_>>()
            .join(" ");
        for word in ["账户", "🦀", "e\u{301}", "styled", "words"] {
            assert!(text.contains(word));
        }
        assert!(wrapped_rows("🦀", 0, style).is_empty());
    }

    #[test]
    fn multiline_catalog_scroll_resize_and_hits_share_visible_rows() {
        let mut program = program();
        program.input.choices = (0..100)
            .map(|index| QuickConnectionChoice {
                label: format!("Model choice {index} 账户 🦀 e\u{301}"),
                detail: "Ready".to_owned(),
            })
            .collect();
        for (width, height) in [(24, 20), (50, 16), (2, 2), (0, 0), (24, 20)] {
            let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, width, height));
            program.render(&mut PaintCx::new(&mut bmux_tui::frame::Frame::new(
                &mut buffer,
            )));
            if program.choice_area.height == 0 {
                assert!(
                    choice_list(&program.items())
                        .visible_semantic_regions(program.choice_area, &program.list)
                        .is_empty()
                );
                continue;
            }
            for _ in 0..30 {
                program.key(KeyCode::Down);
            }
            program.render(&mut PaintCx::new(&mut bmux_tui::frame::Frame::new(
                &mut buffer,
            )));
            let items = program.items();
            let list = choice_list(&items);
            let hits = list.visible_semantic_regions(program.choice_area, &program.list);
            assert!(!hits.is_empty());
            assert!(
                hits.iter()
                    .any(|hit| hit.key == program.selected.to_string())
            );
            for hit in hits {
                assert!(hit.rect.y >= program.choice_area.y);
                assert!(hit.rect.bottom() <= program.choice_area.bottom());
                assert_eq!(
                    list.semantic_id_at(
                        program.choice_area,
                        &program.list,
                        bmux_tui::geometry::Point::new(hit.rect.x, hit.rect.y)
                    ),
                    Some(hit.key)
                );
                assert!(!buffer.row_symbols(hit.rect.y).unwrap().trim().is_empty());
            }
        }
        assert_eq!(program.key(KeyCode::Enter), Lifecycle::Exit);
        assert_eq!(
            program.outcome,
            QuickOnboardingOutcome::Selected(program.selected)
        );
    }

    #[test]
    fn navigation_is_bounded() {
        let mut program = program();
        for _ in 0..10 {
            program.key(KeyCode::Down);
        }
        assert_eq!(program.selected, 0);
        program.key(KeyCode::Up);
        assert_eq!(program.selected, 0);
    }
}
