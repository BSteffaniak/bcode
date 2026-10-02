//! Terminal adaptation for per-goal swarm settings. Execution semantics live in `swarm`.
use super::{
    CollaborationMode, Color, Component, Constraints, Event, FromStr, Insets, KeyCode, LayoutCx,
    LocalRect, LoopSurface, MAX_PROMPT_BYTES, ModalFrame, ModalFrameComponent, ModalSizing,
    ModalTheme, PaintCx, PluginTuiTheme, PluginWorkflowStartRequest, Rect, Size, TextBlock,
    TextInputPolicy, TextInputState, event_click_in, input_text, swarm, text_state,
};
use bmux_tui_components::button::{
    Button, ButtonComponent, ButtonOutcome, ButtonPolicy, ButtonState,
};

pub struct SwarmUi {
    pub open: bool,
    pub error: String,
    pub instructions: TextInputState,
    attempts: TextInputState,
    concurrency: TextInputState,
    focus: usize,
    areas: [Rect; 5],
    mode_button: std::cell::Cell<ButtonState>,
    done_button: std::cell::Cell<ButtonState>,
    pub launcher: std::cell::Cell<ButtonState>,
    pub launcher_area: Rect,
}

impl Default for SwarmUi {
    fn default() -> Self {
        Self {
            open: false,
            error: String::new(),
            instructions: text_state(""),
            attempts: text_state(""),
            concurrency: text_state(""),
            focus: 0,
            areas: [Rect::new(0, 0, 0, 0); 5],
            mode_button: std::cell::Cell::new(ButtonState::new()),
            done_button: std::cell::Cell::new(ButtonState::new()),
            launcher: std::cell::Cell::new(ButtonState::new()),
            launcher_area: Rect::new(0, 0, 0, 0),
        }
    }
}

impl SwarmUi {
    pub fn launch_event(
        &mut self,
        mode: CollaborationMode,
        focused: bool,
        enabled: bool,
        event: &Event,
    ) -> bool {
        let mut state = self.launcher.get();
        state.set_focused(focused);
        state.set_disabled(!enabled);
        let outcome = if !matches!(event, Event::Key(_) | Event::Paste(_)) || focused {
            Button::new(Self::label(mode))
                .policy(ButtonPolicy::interactive())
                .handle_event(self.launcher_area, &mut state, event)
        } else {
            ButtonOutcome::Ignored
        };
        self.launcher.set(state);
        if outcome == ButtonOutcome::Pressed {
            self.open = true;
        }
        outcome.is_handled()
    }

    pub fn configure(&mut self, arguments: &swarm::Arguments) {
        self.instructions = text_state(&arguments.swarm_instructions);
        self.attempts = text_state(
            &arguments
                .worker_attempts
                .map_or_else(String::new, |n| n.to_string()),
        );
        self.concurrency = text_state(
            &arguments
                .concurrency
                .map_or_else(String::new, |n| n.to_string()),
        );
    }

    pub fn apply_limits(&self, request: &mut PluginWorkflowStartRequest) -> Result<(), String> {
        if let Some(concurrency) = self.concurrency()? {
            request.limits.concurrency_cap = concurrency;
        }
        Ok(())
    }

    pub fn attempts(&self) -> Result<Option<u64>, String> {
        positive(&input_text(&self.attempts), "Additional attempts")
    }

    fn concurrency(&self) -> Result<Option<u32>, String> {
        positive(&input_text(&self.concurrency), "Concurrent nodes")
    }

    pub const fn label(mode: CollaborationMode) -> &'static str {
        match mode {
            CollaborationMode::Requested => "Agents: Collaborate · Swarm settings (Ctrl+S)",
            CollaborationMode::NoDelegation => "Agents: No delegation · Swarm settings (Ctrl+S)",
            CollaborationMode::Auto | CollaborationMode::Disabled => {
                "Agents: Auto · Swarm settings (Ctrl+S)"
            }
        }
    }

    pub fn paint_launcher(
        &mut self,
        mode: CollaborationMode,
        content: Rect,
        frame: &mut PaintCx<'_, '_>,
    ) -> Rect {
        let rect = Rect::new(content.x, content.y, content.width, 1).intersection(content);
        self.launcher_area = rect;
        let button = ButtonComponent::new("goal.swarm", Self::label(mode), &self.launcher)
            .policy(ButtonPolicy::interactive());
        paint(&button, rect, frame);
        let reserved = content.height.min(2);
        Rect::new(
            content.x,
            content.y.saturating_add(reserved),
            content.width,
            content.height.saturating_sub(reserved),
        )
    }

    pub fn paint(
        &mut self,
        mode: CollaborationMode,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
        theme: Option<&PluginTuiTheme>,
    ) {
        let theme_frame = theme.map_or_else(
            || ModalTheme::dark(Color::Cyan),
            |t| {
                ModalTheme::new(
                    t.canvas,
                    t.border.patch(t.canvas),
                    t.focused.patch(t.canvas),
                    t.text.patch(t.canvas),
                    t.muted.patch(t.canvas),
                    t.focused.patch(t.canvas),
                )
            },
        );
        let modal = ModalFrame::new(
            ModalSizing::new(Size::new(40, 18), Size::new(100, 30), Insets::all(1)),
            theme_frame,
        )
        .title(" Swarm behavior ")
        .padding(Insets::all(1));
        let content = modal.content_area(area).intersection(area);
        paint(
            &ModalFrameComponent::new("goal.swarm.modal", modal, TextBlock::new("")),
            area,
            frame,
        );
        let explanation = if self.error.is_empty() {
            match mode {
                CollaborationMode::Auto | CollaborationMode::Disabled => {
                    "Delegate when useful; adapt within your requirements and permissions."
                }
                CollaborationMode::Requested => {
                    "Require actual worker contributions, integration and validation."
                }
                CollaborationMode::NoDelegation => {
                    "No workflow delegation tools; restricted tool set. Shell permissions still apply."
                }
            }
        } else {
            self.error.as_str()
        };
        let message_area = Rect::new(content.x, content.y, content.width, 2).intersection(content);
        paint(&TextBlock::new(explanation), message_area, frame);
        let content = Rect::new(
            content.x,
            content.y.saturating_add(message_area.height),
            content.width,
            content.height.saturating_sub(message_area.height),
        );
        let mut y = content.y;
        let heights = [1, content.height.saturating_sub(14), 4, 4, 1];
        for (index, height) in heights.into_iter().enumerate() {
            self.areas[index] =
                Rect::new(content.x, y, content.width, height).intersection(content);
            y = y.saturating_add(height).saturating_add(1);
        }
        for (state, index) in [(&self.mode_button, 0), (&self.done_button, 4)] {
            let mut value = state.get();
            value.set_focused(self.focus == index);
            state.set(value);
        }
        paint(
            &ButtonComponent::new("goal.swarm.mode", Self::label(mode), &self.mode_button)
                .policy(ButtonPolicy::interactive()),
            self.areas[0],
            frame,
        );
        for (index, label, state) in [
            (
                1,
                "Strategy requirements (LLM instructions, not permission grants)",
                &mut self.instructions,
            ),
            (
                2,
                "Additional attempts (blank = 20; not workers or cost)",
                &mut self.attempts,
            ),
            (
                3,
                "Concurrent nodes (blank = runtime default; enforced)",
                &mut self.concurrency,
            ),
        ] {
            LoopSurface::render_input(
                self.areas[index],
                frame,
                label,
                state,
                self.focus == index,
                self.areas[index].height,
                theme,
            );
        }
        paint(
            &ButtonComponent::new("goal.swarm.done", "Done · Esc returns", &self.done_button)
                .policy(ButtonPolicy::interactive()),
            self.areas[4],
            frame,
        );
    }

    pub fn event(&mut self, mode: &mut CollaborationMode, event: &Event) -> Result<(), String> {
        self.error.clear();
        if let Event::Key(key) = event {
            if key.key == KeyCode::Escape {
                self.validate()?;
                self.open = false;
                return Ok(());
            }
            if key.key == KeyCode::Tab {
                self.focus = (self.focus + if key.modifiers.shift { 4 } else { 1 }) % 5;
                return Ok(());
            }
        }
        for index in 0..5 {
            if event_click_in(event, self.areas[index]) {
                self.focus = index;
            }
        }
        for (index, label, state) in [
            (0, Self::label(*mode), &self.mode_button),
            (4, "Done · Esc returns", &self.done_button),
        ] {
            let mut value = state.get();
            value.set_focused(self.focus == index);
            let outcome =
                if !matches!(event, Event::Key(_) | Event::Paste(_)) || self.focus == index {
                    Button::new(label)
                        .policy(ButtonPolicy::interactive())
                        .handle_event(self.areas[index], &mut value, event)
                } else {
                    ButtonOutcome::Ignored
                };
            state.set(value);
            if outcome == ButtonOutcome::Pressed {
                if index == 0 {
                    *mode = match mode {
                        CollaborationMode::Auto | CollaborationMode::Disabled => {
                            CollaborationMode::Requested
                        }
                        CollaborationMode::Requested => CollaborationMode::NoDelegation,
                        CollaborationMode::NoDelegation => CollaborationMode::Auto,
                    };
                } else {
                    self.validate()?;
                    self.open = false;
                }
                return Ok(());
            }
        }
        let state = match self.focus {
            1 => &mut self.instructions,
            2 => &mut self.attempts,
            3 => &mut self.concurrency,
            _ => return Ok(()),
        };
        let _ = bmux_tui_components::text_input::TextInputControl::new(
            &TextInputPolicy::chat_composer(),
        )
        .handle_event(state, event);
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        self.attempts()?;
        self.concurrency()?;
        if input_text(&self.instructions).len() > MAX_PROMPT_BYTES {
            return Err("Swarm instructions are too large".into());
        }
        Ok(())
    }
}

fn positive<T: FromStr + Default + PartialEq>(
    text: &str,
    label: &str,
) -> Result<Option<T>, String> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    let value = text
        .trim()
        .parse::<T>()
        .map_err(|_| format!("{label} must be a positive integer"))?;
    if value == T::default() {
        return Err(format!("{label} must be a positive integer"));
    }
    Ok(Some(value))
}

fn paint(component: &impl Component, rect: Rect, frame: &mut PaintCx<'_, '_>) {
    let layout = component.layout(Constraints::tight(rect.size()), &mut LayoutCx::new());
    frame.with_child(
        i32::from(rect.x),
        i64::from(rect.y),
        LocalRect::new(0, 0, rect.width, rect.height),
        |cx| component.paint(&layout, cx),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_keyboard::{KeyStroke, Modifiers};

    fn key(key: KeyCode) -> Event {
        Event::Key(KeyStroke {
            key,
            modifiers: Modifiers::default(),
        })
    }

    #[test]
    fn mode_cycle_and_numeric_validation() {
        let mut ui = SwarmUi {
            open: true,
            ..SwarmUi::default()
        };
        let mut mode = CollaborationMode::Auto;
        ui.event(&mut mode, &key(KeyCode::Enter)).unwrap();
        assert_eq!(mode, CollaborationMode::Requested);
        ui.event(&mut mode, &key(KeyCode::Enter)).unwrap();
        assert_eq!(mode, CollaborationMode::NoDelegation);
        ui.event(&mut mode, &key(KeyCode::Enter)).unwrap();
        assert_eq!(mode, CollaborationMode::Auto);
        ui.attempts = text_state("0");
        assert!(ui.event(&mut mode, &key(KeyCode::Escape)).is_err());
        assert!(ui.open);
        ui.attempts = text_state("12");
        ui.concurrency = text_state("3");
        assert_eq!(ui.attempts().unwrap(), Some(12));
        assert_eq!(ui.concurrency().unwrap(), Some(3));
        ui.event(&mut mode, &key(KeyCode::Escape)).unwrap();
        assert!(!ui.open);
    }

    #[test]
    fn numeric_bounds_reject_overflow_and_accept_runtime_default() {
        let mut ui = SwarmUi::default();
        assert_eq!(ui.concurrency().unwrap(), None);
        ui.concurrency = text_state("4294967296");
        assert!(ui.validate().is_err());
        ui.concurrency = text_state("-1");
        assert!(ui.validate().is_err());
    }
}
