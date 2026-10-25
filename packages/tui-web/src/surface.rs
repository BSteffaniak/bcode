//! BMUX rendering/input using retained shared native Markdown components.
use std::cell::RefCell;

use crate::transcript::{Transcript, TranscriptComponent};
use bcode_tui_components::{
    composer::ComposerStyle,
    session_ui::{
        self, SessionComposerStyle, SessionUi, SessionUiAction, SessionUiEffect, SessionUiIds,
    },
    transcript_history::TranscriptHistory,
};
use bmux_tui::{
    component::{Component, Constraints, LayoutCx, LayoutId},
    damage::Damage,
    event::{Event, MouseEventKind},
    geometry::{Point, Rect, Size},
    paint::PaintCx,
    presentation::PresentationLimits,
    style::Style,
};
use bmux_tui_components::text_input::{TextInputControl, TextInputState};
use bmux_web_renderer::{LocalSurface, input::BrowserInput};

use crate::SnapshotState;

const EDITOR: &str = "bcode-web/editor";
const IDS: SessionUiIds<'static> = SessionUiIds {
    root: "bcode-web/root",
    composer: "bcode-web/composer",
    editor: EDITOR,
    composer_size: "bcode-web/composer-size",
};

/// Concrete local surface. This is not a portable application contract.
pub struct SessionSurface {
    pub(crate) local: LocalSurface,
    pub(crate) inner: SurfaceState,
}

pub struct SurfaceState {
    pub(crate) semantic: SnapshotState,
    ui: SessionUi,
    area: Rect,
    transcript: Transcript,
    selection: bmux_tui::selection::SelectionController,
    height: u16,
    anchor: Option<(String, usize)>,
    focused: bool,
    bindings: crate::bindings::Bindings,
    copy_requested: bool,
}

impl SessionSurface {
    /// Create a session-bound local surface, with bounded presentation allocations.
    ///
    /// # Errors
    /// Rejects invalid session identifiers and excessively large cell grids.
    pub fn new(session: String, width: u16, height: u16) -> Result<Self, &'static str> {
        let area = checked_area(width, height)?;
        Ok(Self {
            local: LocalSurface::new(
                area,
                PresentationLimits {
                    max_cells: 131_072,
                    max_entries: 16_384,
                    max_bytes: 16 * 1024 * 1024,
                },
            ),
            inner: SurfaceState {
                semantic: SnapshotState::new(session)?,
                ui: SessionUi::new(),
                area,
                transcript: Transcript::new(),
                selection: bmux_tui::selection::SelectionController::new(),
                height: 0,
                anchor: None,
                focused: true,
                bindings: crate::bindings::Bindings::new(),
                copy_requested: false,
            },
        })
    }

    /// Replace the accepted semantic observation without flattening plugin snapshots.
    ///
    /// # Errors
    /// See [`SnapshotState::accept`].
    pub fn accept_snapshot(&mut self, json: &str) -> Result<bool, &'static str> {
        let changed = self.inner.semantic.accept(json)?;
        if changed {
            self.prepare();
        }
        Ok(changed)
    }

    /// Change the cell grid. Old pointer capture is cancelled immediately.
    ///
    /// # Errors
    /// Rejects grids exceeding the bounded cell budget.
    pub fn resize(&mut self, width: u16, height: u16) -> Result<(), &'static str> {
        let area = checked_area(width, height)?;
        self.cancel_pointer();
        self.inner.area = area;
        self.local.resize(area);
        self.prepare();
        Ok(())
    }

    /// Cancel pointer capture (including blur/disposal).
    pub fn cancel_pointer(&mut self) {
        self.local.cancel_pointer();
        self.inner.selection.set_obscured(
            true,
            bmux_tui::selection::SelectionObscurationPolicy::PreserveCompleted,
        );
        self.inner.selection.set_obscured(
            false,
            bmux_tui::selection::SelectionObscurationPolicy::PreserveCompleted,
        );
        // Reset editor gesture state while retaining real text and source selection.
        let state = &mut self.inner.ui.composer;
        let area = state.content_area();
        let position = Point::new(area.x, area.y);
        TextInputControl::new(&session_ui::composer_policy()).handle_mouse(
            state,
            bmux_tui::event::MouseEvent::new(
                MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
                position,
            ),
        );
    }

    /// Route a bounded bridge event against the last successfully displayed scene.
    /// Returns true when the shared editor requests a submit.
    ///
    /// # Errors
    /// Rejects oversized or malformed bridge input.
    pub fn input(&mut self, json: &str) -> Result<bool, &'static str> {
        let area = self.inner.area;
        let input = BrowserInput::decode(json, 128 * 1024)
            .map_err(|_| "invalid browser input")?
            .normalize(64 * 1024, |x, y| map_point(area, x, y))
            .map_err(|_| "invalid browser input")?;
        let mut submit = false;
        for route in self.local.route_input(input) {
            if route.traversal_consumed {
                continue;
            }
            let mut history = WindowHistory;
            let action = match route.event {
                Event::Key(key) => {
                    use crate::bindings::Command;
                    match self.inner.bindings.command(key) {
                        Some(Command::Submit) => {
                            submit = true;
                            None
                        }
                        Some(Command::Newline) => {
                            TextInputControl::new(&session_ui::composer_policy())
                                .handle_paste(&mut self.inner.ui.composer, "\n");
                            None
                        }
                        Some(Command::PageUp) => Some(SessionUiAction::ScrollUp(usize::from(
                            self.inner.height.max(1),
                        ))),
                        Some(Command::PageDown) => Some(SessionUiAction::ScrollDown(usize::from(
                            self.inner.height.max(1),
                        ))),
                        Some(Command::Copy) => {
                            self.inner.copy_requested = true;
                            None
                        }
                        Some(Command::Ignore) => None,
                        None => {
                            self.inner.bindings.edit(&mut self.inner.ui.composer, key);
                            None
                        }
                    }
                }
                Event::Paste(text) => Some(SessionUiAction::Paste(text)),
                Event::Mouse(mouse)
                    if matches!(
                        mouse.kind,
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    ) =>
                {
                    Some(if mouse.kind == MouseEventKind::ScrollUp {
                        SessionUiAction::ScrollUp(3)
                    } else {
                        SessionUiAction::ScrollDown(3)
                    })
                }
                Event::Mouse(mouse) if route.targets(&bmux_tui::hit::HitId::new(EDITOR)) => {
                    TextInputControl::new(&session_ui::composer_policy())
                        .handle_mouse(&mut self.inner.ui.composer, mouse);
                    None
                }
                Event::Focus(focus) => {
                    self.inner.focused = focus == bmux_tui::event::FocusEvent::Gained;
                    if !self.inner.focused {
                        self.cancel_pointer();
                    }
                    None
                }
                Event::Mouse(mouse) => {
                    self.inner
                        .selection
                        .handle_mouse(self.local.presentation().selection(), mouse);
                    None
                }
                _ => None,
            };
            if let Some(action) = action {
                submit |=
                    self.inner.ui.update(action, &mut history) == SessionUiEffect::SubmitRequested;
            }
        }
        self.inner.capture_anchor();
        Ok(submit)
    }

    /// Prepare projections outside the paint callback.
    pub(crate) fn prepare(&mut self) {
        self.inner.transcript.prepare(
            self.inner.semantic.snapshot().map(|s| &s.transcript),
            self.inner.area.width,
        );
    }

    /// Copy canonical accepted Markdown source for the current logical selection.
    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        let snapshot = self
            .inner
            .selection
            .snapshot(self.local.presentation().selection())?;
        self.inner.transcript.copy(&snapshot)
    }

    /// Configure explicit application command overrides (bounded JSON key map).
    ///
    /// # Errors
    /// Rejects malformed or oversized keymap configuration.
    pub fn configure_bindings(&mut self, json: &str) -> Result<(), &'static str> {
        self.inner.bindings.configure(json)
    }

    /// Take a requested canonical clipboard payload. JS owns platform access.
    pub fn take_clipboard(&mut self) -> Option<String> {
        if !std::mem::take(&mut self.inner.copy_requested) {
            return None;
        }
        self.selected_text()
    }

    /// Current real BMUX draft source.
    #[must_use]
    pub fn draft(&self) -> &str {
        self.inner.ui.composer.buffer().text()
    }

    /// Validate a durable receipt and clear only its unchanged draft.
    ///
    /// # Errors
    /// See [`SnapshotState::settle_submission`].
    pub fn settle_submission(&mut self, json: &str) -> Result<(), &'static str> {
        let accepted = self.inner.semantic.settle_submission(json)?;
        if self.draft() == accepted {
            self.inner.ui.composer = TextInputState::new(bmux_text_edit::TextEditBuffer::new());
            self.inner
                .ui
                .update(SessionUiAction::FollowTail, &mut WindowHistory);
        }
        Ok(())
    }

    /// Render to a caller-owned local presentation sink (no network frame decode).
    ///
    /// # Errors
    /// Returns bounded presentation or output errors.
    pub fn present<E>(
        &mut self,
        output: impl FnOnce(&bmux_tui::presentation::PreparedPresentation) -> Result<(), E>,
    ) -> Result<bool, bmux_tui::presentation::PresentationError<E>> {
        let inner = &mut self.inner;
        self.local
            .present(Damage::Full, |cx| inner.paint(cx), output)
    }
}

fn checked_area(width: u16, height: u16) -> Result<Rect, &'static str> {
    if usize::from(width) * usize::from(height) > 131_072 {
        return Err("surface too large");
    }
    Ok(Rect::new(0, 0, width, height))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn map_point(area: Rect, x: f64, y: f64) -> Option<Point> {
    (x.is_finite()
        && y.is_finite()
        && x >= 0.0
        && y >= 0.0
        && x < f64::from(area.width)
        && y < f64::from(area.height))
    .then(|| Point::new(x.floor() as u16, y.floor() as u16))
}

impl SurfaceState {
    fn capture_anchor(&mut self) {
        let top = self.ui.viewport.top_row(self.transcript.rows, self.height);
        self.anchor = self.transcript.anchor(top);
    }

    pub(crate) fn paint(&mut self, cx: &mut PaintCx<'_, '_>) {
        let editor = RefCell::new(std::mem::replace(
            &mut self.ui.composer,
            TextInputState::new(bmux_text_edit::TextEditBuffer::new()),
        ));
        let policy = session_ui::composer_policy();
        let composer_height = TextInputControl::new(&policy)
            .visible_rows_for_width(&editor.borrow(), self.area.width.saturating_sub(2))
            .clamp(1, 6)
            .saturating_add(1)
            .min(self.area.height);
        let style = Style::new();
        let transcript = &self.transcript;
        let focused = self.focused;
        let build = |top| {
            session_ui::compose(
                &IDS,
                TranscriptComponent { transcript, top },
                session_ui::composer(
                    &IDS,
                    &editor,
                    &policy,
                    SessionComposerStyle {
                        panel: ComposerStyle {
                            border: style,
                            surface: style,
                        },
                        text: style,
                        selection: style,
                        muted: style,
                    },
                    focused,
                ),
                composer_height,
            )
        };
        let constraints = Constraints::tight(Size::new(self.area.width, self.area.height));
        let layout = build(0).layout(constraints, &mut LayoutCx::new());
        self.height = layout
            .find_logical_rect(&LayoutId::new("bcode-web/transcript"))
            .and_then(|rect| u16::try_from(rect.height).ok())
            .unwrap_or(0);
        let anchor = self
            .anchor
            .as_ref()
            .and_then(|anchor| self.transcript.resolve_anchor(anchor));
        self.ui.viewport.sync_with_anchor(
            (
                self.transcript
                    .rows
                    .saturating_sub(usize::from(self.height)),
                0,
                self.transcript.rows,
                self.height,
            ),
            anchor,
            0,
            0,
            &mut WindowHistory,
        );
        let top = self.ui.viewport.top_row(self.transcript.rows, self.height);
        build(top).paint(&layout, cx);
        self.ui.composer = editor.into_inner();
        if let Some(selection) = self.selection.snapshot(cx.selection()) {
            cx.paint_selection(
                &selection,
                Style::new().add_modifier(bmux_tui::style::Modifier::REVERSED),
            );
        }
        self.capture_anchor();
    }
}

// The endpoint supplies one bounded resident document. No fabricated pagination.
struct WindowHistory;
impl TranscriptHistory for WindowHistory {
    fn has_older_history(&self) -> bool {
        false
    }
    fn has_newer_history(&self) -> bool {
        false
    }
    fn reveal_request(&self) -> Option<usize> {
        None
    }
    fn newer_reveal_request(&self) -> Option<usize> {
        None
    }
    fn cancel_newer_reveal(&mut self) {}
    fn cancel_older_reveal(&mut self) {}
    fn request_load(&mut self, _: usize) {}
    fn request_load_newer(&mut self, _: usize) {}
    fn clear_reveal_request(&mut self) {}
    fn take_ready_pages(&mut self) -> (bool, bool, bool) {
        (false, false, false)
    }
    fn consume_reveal(&mut self, _: bool, _: usize) {}
}
