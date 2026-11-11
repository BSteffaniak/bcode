//! Markdown contribution activation for the transcript.
//!
//! * Clicking an actionable Markdown contribution (press and release without a drag, BMUX 14)
//!   focuses and activates it; `tui.markdown.focusNext` / `focusPrevious` (ctrl+tab /
//!   ctrl+shift+tab) move the Markdown focus through the engine's retained per-unit targets and
//!   reveal it through the focus reveal, `tui.markdown.activate` (alt+enter) activates it and
//!   `tui.markdown.copyDestination` (ctrl+alt+c) copies its safe destination. The keys work from
//!   either keyboard focus and never take keyboard focus (FO1).
//! * Links and GitHub issue references open through the host shell's safe activation boundary
//!   ([`crate::markdown_activation`]); `#bcode-session-<id>` fragments request a session switch
//!   ([`TranscriptV2Host::take_session_link_request`]); other fragments navigate the transcript
//!   ([`bcode_tui_transcript::engine::TranscriptEngine::navigate_markdown_fragment`]).
//! * Footnote references jump to their definition and back (engine-owned return targets).
//! * `<details>` toggles are V2-owned render state keyed by unit and contribution id
//!   ([`TranscriptV2Host::set_markdown_details`]): a revision of the owning unit only (L3, L8).
//! * Mermaid diagrams toggle a source view painted over the transcript viewport while the
//!   diagram is visible; it closes when activated again or when the diagram leaves the view.
//!
//! Status messages ([`MarkdownNotice`]) are reported through the host shell's status row.
//! The focus highlight is reversed video over the focused target's cells, from the prepared
//! frame's retained geometry (B3, L5).

use std::time::Instant;

use bcode_markdown_render::MarkdownDestination;
use bcode_session_models::SessionId;
use bcode_tui_transcript::focus::FocusStep;
use bcode_tui_transcript::markdown::{MarkdownActivation, MarkdownTargetId};
use bcode_tui_transcript::units::UnitKey;
use bmux_keyboard::KeyStroke;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::{Modifier, Style};

use super::TranscriptV2Host;
use crate::app::BmuxApp;
use crate::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};
use crate::markdown_activation::{MarkdownActivation as Opened, MarkdownActivationError};

/// Chat-scope Markdown keys (`tui.markdown.*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarkdownKey {
    /// Focus the next contribution (`tui.markdown.focusNext`).
    FocusNext,
    /// Focus the previous contribution (`tui.markdown.focusPrevious`).
    FocusPrevious,
    /// Activate the focused contribution (`tui.markdown.activate`).
    Activate,
    /// Copy the focused contribution's destination (`tui.markdown.copyDestination`).
    CopyDestination,
}

/// The Markdown key `stroke` is bound to in the chat scope, if any.
#[must_use]
pub fn resolve_markdown_key(keymap: &BmuxKeyMap, stroke: KeyStroke) -> Option<MarkdownKey> {
    match keymap.action_for_key(BmuxScope::Chat, stroke)? {
        BmuxAction::MarkdownFocusNext => Some(MarkdownKey::FocusNext),
        BmuxAction::MarkdownFocusPrevious => Some(MarkdownKey::FocusPrevious),
        BmuxAction::MarkdownActivate => Some(MarkdownKey::Activate),
        BmuxAction::MarkdownCopyDestination => Some(MarkdownKey::CopyDestination),
        _ => None,
    }
}

/// Status message of one Markdown action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownNotice {
    /// Focus moved to a contribution; names the activate, copy and move keys.
    Focused,
    /// A `<details>` block opened or closed.
    Details {
        /// Whether it is now open.
        open: bool,
    },
    /// The Mermaid source view opened or closed.
    SourceView {
        /// Whether it is now open.
        open: bool,
    },
    /// A footnote definition was focused, or is not available.
    FootnoteDefinition {
        /// Whether it was focused.
        focused: bool,
    },
    /// Focus returned to a footnote reference, or none is available.
    FootnoteReference {
        /// Whether a reference was focused.
        returned: bool,
    },
    /// A destination was handed to the platform opener.
    Opened,
    /// The platform opener rejected a destination.
    OpenFailed(String),
    /// A `#fragment` link navigated to its section, or its target is not resident.
    Fragment {
        /// Whether the target was found.
        found: bool,
    },
}

impl MarkdownNotice {
    /// Status text, with key hints from the host shell's effective keymap.
    #[must_use]
    pub fn text(&self, app: &BmuxApp) -> String {
        match self {
            Self::Focused => format!(
                "Markdown contribution focused; {} activates; {} copies; {} moves",
                app.shortcut_hint("chat", "tui.markdown.activate"),
                app.shortcut_hint("chat", "tui.markdown.copyDestination"),
                app.shortcut_hint("chat", "tui.markdown.focusNext")
            ),
            Self::Details { open: true } => "Details expanded".to_owned(),
            Self::Details { open: false } => "Details collapsed".to_owned(),
            Self::SourceView { open: true } => {
                "Mermaid source view opened; activate again to close".to_owned()
            }
            Self::SourceView { open: false } => "Mermaid source view closed".to_owned(),
            Self::FootnoteDefinition { focused: true } => {
                "Footnote definition focused; activate to return".to_owned()
            }
            Self::FootnoteDefinition { focused: false } => {
                "Footnote definition is not available".to_owned()
            }
            Self::FootnoteReference { returned: true } => {
                "Returned to footnote reference".to_owned()
            }
            Self::FootnoteReference { returned: false } => {
                "Footnote reference is not available".to_owned()
            }
            Self::Opened => "Opened Markdown destination".to_owned(),
            Self::OpenFailed(error) => format!("Could not open Markdown destination: {error}"),
            Self::Fragment { found: true } => "Navigated to Markdown section".to_owned(),
            Self::Fragment { found: false } => "Markdown fragment target was not found".to_owned(),
        }
    }
}

/// The open Mermaid source view.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceView {
    target: MarkdownTargetId,
    source: String,
}

/// Host-side Markdown activation state: the source view, the frame's focus highlight and the
/// requests and notices waiting for the host shell.
#[derive(Debug, Default)]
pub struct MarkdownHost {
    source_view: Option<SourceView>,
    /// Terminal rectangle of the source view of the prepared frame.
    source_area: Option<Rect>,
    /// Viewport cells of the focused contribution in the prepared frame.
    focus_rects: Vec<Rect>,
    notice: Option<MarkdownNotice>,
    copy: Option<String>,
    session_link: Option<SessionId>,
    /// Activate key label for the source view title.
    activate_label: String,
    /// Destinations the platform opener received (tests never reach the real opener).
    #[cfg(test)]
    pub(super) opened: Vec<String>,
    /// Error the test opener reports.
    #[cfg(test)]
    pub(super) open_failure: Option<String>,
}

impl TranscriptV2Host {
    /// Route one chat-scope Markdown key. Returns whether anything changed (a status notice
    /// counts); `false` when nothing is focused or the action does not apply.
    pub(super) fn markdown_key(&mut self, key: MarkdownKey, at: Instant) -> bool {
        match key {
            MarkdownKey::FocusNext | MarkdownKey::FocusPrevious => {
                let step = if key == MarkdownKey::FocusNext {
                    FocusStep::Next
                } else {
                    FocusStep::Previous
                };
                let (update, changed) = self.engine.markdown_focus_step(step, at);
                self.absorb(update);
                if changed {
                    self.markdown_notify(MarkdownNotice::Focused);
                }
                changed
            }
            MarkdownKey::Activate => self
                .engine
                .markdown_focus()
                .cloned()
                .is_some_and(|target| self.activate_markdown_target(&target, at)),
            MarkdownKey::CopyDestination => self.copy_markdown_destination(),
        }
    }

    /// Activate the Markdown contribution an item region click hit (`markdown:<id>` of
    /// `unit`); the engine click already focused it. Returns whether anything changed.
    pub(super) fn activate_markdown_hit(&mut self, unit: &UnitKey, key: &str, at: Instant) -> bool {
        self.engine
            .markdown_target_for_action(unit, key)
            .is_some_and(|target| self.activate_markdown_target(&target, at))
    }

    /// Activate a resident Markdown target with the host shell's policy and messages.
    fn activate_markdown_target(&mut self, target: &MarkdownTargetId, at: Instant) -> bool {
        let (update, activation) = self.engine.activate_markdown(target, at);
        self.absorb(update);
        match activation {
            MarkdownActivation::Details { key, open } => {
                if self.set_markdown_details(key, open) {
                    let _ = self.flush_render_config(at);
                }
                self.markdown_notify(MarkdownNotice::Details { open });
                true
            }
            MarkdownActivation::Mermaid { source } => {
                let open = self
                    .markdown
                    .source_view
                    .as_ref()
                    .is_none_or(|view| &view.target != target);
                self.markdown.source_view = open.then(|| SourceView {
                    target: target.clone(),
                    source,
                });
                self.invalidated = true;
                self.markdown_notify(MarkdownNotice::SourceView { open });
                true
            }
            MarkdownActivation::FootnoteDefinition { focused } => {
                self.markdown_notify(MarkdownNotice::FootnoteDefinition { focused });
                true
            }
            MarkdownActivation::FootnoteReference { returned } => {
                self.markdown_notify(MarkdownNotice::FootnoteReference { returned });
                true
            }
            MarkdownActivation::Link(destination) => {
                self.activate_markdown_link(&target.unit, destination, at)
            }
            MarkdownActivation::Unavailable => false,
        }
    }

    /// Open a classified link destination: a `#bcode-session-<id>` fragment requests a session
    /// switch, other fragments navigate the transcript (from the link's unit first), safe web
    /// and local destinations reach the platform opener; inert destinations do nothing.
    fn activate_markdown_link(
        &mut self,
        unit: &UnitKey,
        destination: MarkdownDestination,
        at: Instant,
    ) -> bool {
        if let MarkdownDestination::Fragment(fragment) = &destination
            && let Some(child) = fragment.strip_prefix("bcode-session-")
            && let Ok(session) = child.parse()
        {
            self.markdown.session_link = Some(session);
            return true;
        }
        match self.open_markdown_destination(&destination) {
            Ok(Opened::External) => self.markdown_notify(MarkdownNotice::Opened),
            Ok(Opened::Fragment) => {
                let MarkdownDestination::Fragment(fragment) = destination else {
                    return false;
                };
                let (update, found) =
                    self.engine
                        .navigate_markdown_fragment(Some(unit), &fragment, at);
                self.absorb(update);
                self.markdown_notify(MarkdownNotice::Fragment { found });
            }
            Ok(Opened::Inert) => return false,
            Err(error) => self.markdown_notify(MarkdownNotice::OpenFailed(error.to_string())),
        }
        true
    }

    /// The host shell's safe activation boundary; tests record the destinations instead of
    /// reaching the platform opener.
    #[cfg(not(test))]
    #[expect(
        clippy::unused_self,
        reason = "tests replace the opener with a recorder on the host"
    )]
    fn open_markdown_destination(
        &self,
        destination: &MarkdownDestination,
    ) -> Result<Opened, MarkdownActivationError> {
        crate::markdown_activation::activate_markdown_destination(destination)
    }

    #[cfg(test)]
    fn open_markdown_destination(
        &mut self,
        destination: &MarkdownDestination,
    ) -> Result<Opened, MarkdownActivationError> {
        let markdown = &mut self.markdown;
        crate::markdown_activation::activate_markdown_destination_with(destination, |target| {
            markdown.opened.push(target.to_string_lossy().into_owned());
            markdown
                .open_failure
                .clone()
                .map_or(Ok(()), |error| Err(MarkdownActivationError(error)))
        })
    }

    /// Queue the focused contribution's safe destination for the host shell's clipboard path.
    /// Returns whether one was queued (fragments, inert and non-link contributions copy
    /// nothing).
    fn copy_markdown_destination(&mut self) -> bool {
        use bcode_markdown_render::MarkdownContributionKind;
        let text = self
            .engine
            .markdown_focus()
            .and_then(|id| self.engine.markdown_target(id))
            .and_then(|target| match &target.kind {
                MarkdownContributionKind::Link { destination, .. }
                | MarkdownContributionKind::GitHubIssue { destination, .. } => {
                    crate::markdown_activation::markdown_destination_text(destination)
                }
                _ => None,
            });
        let queued = text.is_some();
        self.markdown.copy = text;
        queued
    }

    fn markdown_notify(&mut self, notice: MarkdownNotice) {
        self.markdown.notice = Some(notice);
        // The notice reports through the status row (F5).
        self.full_damage = true;
    }

    /// The status notice of the last Markdown action, once.
    pub const fn take_markdown_notice(&mut self) -> Option<MarkdownNotice> {
        self.markdown.notice.take()
    }

    /// The destination text the copy key queued for the clipboard, once.
    pub const fn take_markdown_copy(&mut self) -> Option<String> {
        self.markdown.copy.take()
    }

    /// The session a rendered `#bcode-session-<id>` link asked to switch to, once.
    pub const fn take_session_link_request(&mut self) -> Option<SessionId> {
        self.markdown.session_link.take()
    }

    /// The open Mermaid source view's text.
    #[must_use]
    pub fn markdown_source_view(&self) -> Option<&str> {
        self.markdown
            .source_view
            .as_ref()
            .map(|view| view.source.as_str())
    }

    /// Set the activate key label the source view title names.
    pub fn set_markdown_activate_label(&mut self, label: &str) {
        if self.markdown.activate_label != label {
            label.clone_into(&mut self.markdown.activate_label);
            self.invalidated |= self.markdown.source_view.is_some();
        }
    }

    /// Terminal cells of the open Mermaid source view, which occludes the transcript beneath
    /// it (B7).
    pub(super) const fn markdown_source_area(&self) -> Option<Rect> {
        self.markdown.source_area
    }

    /// Whether `point` (terminal cells) lies on the painted source view, which owns pointer
    /// input over it.
    #[must_use]
    pub fn markdown_overlay_contains(&self, point: Point) -> bool {
        self.markdown
            .source_area
            .is_some_and(|area| area.contains(point))
    }

    /// Derive the prepared frame's Markdown presentation (F1): the focus highlight cells and
    /// the source view area. A source view whose diagram is no longer visible closes.
    pub(super) fn prepare_markdown(&mut self, viewport: Rect) {
        self.markdown.focus_rects.clear();
        self.markdown.source_area = None;
        let Some(snapshot) = self.last_frame.as_ref() else {
            return;
        };
        self.markdown.focus_rects = self
            .engine
            .markdown_focus_rects(snapshot)
            .into_iter()
            .filter_map(cells)
            .collect();
        let visible = self.markdown.source_view.as_ref().is_some_and(|view| {
            !self
                .engine
                .visible_markdown_rects(snapshot, &view.target)
                .is_empty()
        });
        if !visible {
            if self.markdown.source_view.take().is_some() {
                self.full_damage = true;
            }
            return;
        }
        let area = viewport.intersection(Rect::new(
            viewport.x.saturating_add(2),
            viewport.y.saturating_add(1),
            viewport.width.saturating_sub(4),
            viewport.height.saturating_sub(2),
        ));
        self.markdown.source_area = (!area.is_empty()).then_some(area);
    }

    /// Paint the focus highlight into the viewport context `cx` (paint-only, L5).
    pub(super) fn paint_markdown_focus(&self, cx: &mut PaintCx<'_, '_>) {
        let reversed = Style::new().add_modifier(Modifier::REVERSED);
        for rect in &self.markdown.focus_rects {
            cx.restyle(
                LocalRect::new(
                    i32::from(rect.x),
                    i64::from(rect.y),
                    rect.width,
                    rect.height,
                ),
                reversed,
            );
        }
    }

    /// Paint the Mermaid source view over the viewport (terminal coordinates): a title naming
    /// the activate key, then the source lines, clipped at the view's width and height. Earlier
    /// transcript output under it is occluded, so no hidden region stays interactive (B7).
    pub(super) fn paint_markdown_source(
        &self,
        cx: &mut PaintCx<'_, '_>,
        background: Style,
        title: Style,
        text: Style,
    ) {
        let (Some(area), Some(view)) = (self.markdown.source_area, &self.markdown.source_view)
        else {
            return;
        };
        let local = LocalRect::terminal(area);
        cx.occlude(local);
        cx.fill(local, " ", background);
        let label = if self.markdown.activate_label.is_empty() {
            "unbound"
        } else {
            &self.markdown.activate_label
        };
        let heading = Line::from_spans(vec![Span::styled(
            format!(" Mermaid source · {label} closes "),
            title.add_modifier(Modifier::BOLD),
        )]);
        cx.write_line(
            LocalRect::terminal(Rect::new(area.x, area.y, area.width, 1)),
            &heading,
        );
        for (offset, line) in view
            .source
            .lines()
            .take(usize::from(area.height.saturating_sub(1)))
            .enumerate()
        {
            let y = area
                .y
                .saturating_add(1)
                .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
            cx.write_line(
                LocalRect::terminal(Rect::new(area.x, y, area.width, 1)),
                &Line::from_spans(vec![Span::styled(line.to_owned(), text)]),
            );
        }
    }
}

/// A viewport rectangle in cells, when addressable.
fn cells(rect: bmux_tui::component::LogicalRect) -> Option<Rect> {
    Some(Rect::new(
        u16::try_from(rect.x).ok()?,
        u16::try_from(rect.y).ok()?,
        u16::try_from(rect.width).ok()?,
        u16::try_from(rect.height).ok()?,
    ))
}

#[cfg(test)]
mod tests;
