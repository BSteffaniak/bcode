//! Plugin visuals and content input of the V2 transcript (E1–E6, E3, C36, C39).
//!
//! * Acceptance ([`accepted_visual`]): a routed adapter visual is accepted with everything V1
//!   presents (render mode, header title and timeout, operation target, summary, named
//!   sections, v3 content keys and v4 source ranges, selection rows), bounded and scoped to its
//!   producer: anchor keys, source identities and selection identities become
//!   `<plugin>:<identity>`, the identity [`PluginTuiPresentation::content_event_with_host`] and
//!   [`PluginTuiPresentation::retain_content_position`] split again. Invalid selection rows
//!   are dropped. Selection rows make plugin cells copy their adapter source (C36). Native
//!   routed visuals carry no images or action regions, so those parts of the accepted visual
//!   stay empty (statically linked portable adapters supply them in browser hosts).
//! * Adapters render with the effective frontend keymap ([`VisualKeymapHost`]), so their key
//!   hints match the keys that reach them.
//! * Pointer targets ([`TranscriptV2Host::committed_visual_targets`]): the plugin visual rows
//!   of the presented frame (published only after its output flushed), in terminal cells, with their producer-scoped identity. The root
//!   model routes horizontal wheel input and right-button presses on them to the adapter, and
//!   the focused visual's keys through the `visual` scope (Escape releases it), exactly as for
//!   V1. Targets come from the presented frame's retained geometry: content scrolled away,
//!   resized or changed leaves no target behind.
//! * Content positions ([`TranscriptV2Host::sync_content_positions`]): while the reader is
//!   anchored inside plugin content, the adapter is asked to retain that source position
//!   across reflow; following the latest content clears the requests.
//! * Canonical shell screens (C39): horizontal wheel input over a shell screen scrolls it by
//!   [`SHELL_SCROLL_COLUMNS`]; a right-button press focuses it, then the `visual` scope's
//!   Left/Right scroll it and Home resets it, Escape (or any other key) releases it. The
//!   offset is host view state applied through the render configuration (only that unit is
//!   re-measured); it never becomes invocation input.

use std::sync::Arc;
use std::time::Instant;

use bcode_plugin_sdk::tui::keymap::KeymapRegistry;
use bcode_session_view_models::TranscriptViewItemId;
use bcode_tui_transcript::engine::VisibleContentTarget;
use bcode_tui_transcript::frame::FrameId;
use bcode_tui_transcript::layout::SourcePosition;
use bcode_tui_transcript::layout::context::AcceptedPluginVisual;
use bcode_tui_transcript::layout::producers::visual::{
    ContentTargetKind, VisualAnchor, VisualCorrespondence, VisualRenderMode, VisualSection,
    VisualSelectionCell, VisualSelectionRow, VisualSourceRange,
};
use bcode_tui_transcript::navigation::{AnchorOffset, ListPosition};
use bcode_tui_transcript::units::UnitKey;
use bmux_keyboard::KeyStroke;
use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::{Point, Rect};

use super::TranscriptV2Host;
use super::input::TranscriptInputOutcome;
use crate::keymap::BmuxKeyMap;
use crate::plugin_tui::{PluginTuiPresentation, RoutedTuiVisual};

/// Most accepted rows of one plugin visual and of each of its named sections.
pub const MAX_PLUGIN_VISUAL_ROWS: usize = 2048;
/// Most named sections of one plugin visual.
pub const MAX_PLUGIN_VISUAL_SECTIONS: usize = 32;
/// Columns one horizontal wheel tick or Left/Right key scrolls a shell screen (as the shell
/// plugin's own terminal screens).
pub const SHELL_SCROLL_COLUMNS: u64 = 8;

/// Content input state of the V2 host: the focused shell screen and the plugin content
/// position last handed to the plugin presentation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContentInput {
    /// Shell screen focused by a right-button press (C39).
    focused_shell: Option<TranscriptViewItemId>,
    /// Content position request last sent to the plugin presentation.
    position: Option<ContentPosition>,
    /// Shell screens of the presented frame (its id): terminal cells, item, most columns.
    /// Read once per presented frame, so input delivered before the next frame keeps using
    /// the geometry the reader sees even after an offset change re-measured the unit (CK4).
    shell_screens: Option<(FrameId, Vec<ShellScreen>)>,
}

/// One shell screen of a presented frame.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellScreen {
    /// Terminal cells.
    area: Rect,
    /// Owning item.
    item: TranscriptViewItemId,
    /// Most columns the screen scrolls.
    max_columns: u64,
}

/// A content position request sent to the plugin presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ContentPosition {
    /// Following the latest content: no position is retained.
    Cleared,
    /// Retain this producer-scoped identity and offset across reflow.
    Retained(String, u64),
}

/// The keymap view adapters render and handle content events with: the effective frontend
/// keymap (key hints and `shell.terminal.*` actions).
pub struct VisualKeymapHost<'a>(pub &'a KeymapRegistry);

impl bcode_plugin_sdk::tui::PluginTuiHost for VisualKeymapHost<'_> {
    fn spawn(&self, _task: bcode_plugin_sdk::tui::PluginTask) {}

    fn spawn_blocking(&self, _task: Box<dyn FnOnce() + Send + 'static>) {}

    fn request_redraw(&self) {}

    fn keymap_action(&self, scope: &str, stroke: KeyStroke) -> Option<String> {
        self.0.action_for_key(scope, stroke).map(str::to_owned)
    }

    fn keymap_keys(&self, scope: &str, action: &str) -> Vec<String> {
        self.0
            .keys_for_action(scope, action)
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    fn keymap_is_overridden(&self, scope: &str, stroke: KeyStroke) -> bool {
        self.0.is_overridden_key(scope, stroke)
    }
}

/// `routed` accepted at `revision`: bounded, with producer-scoped identities (see the module
/// documentation).
#[must_use]
pub fn accepted_visual(routed: RoutedTuiVisual, revision: u64) -> AcceptedPluginVisual {
    let RoutedTuiVisual {
        route,
        render_mode,
        mut rows,
        header,
        anchors,
        selection,
        summary,
        target,
        sections,
    } = routed;
    rows.truncate(MAX_PLUGIN_VISUAL_ROWS);
    let plugin = &route.plugin_id;
    let scoped = |identity: &str| format!("{plugin}:{identity}");
    let anchors = anchors
        .into_iter()
        .map(|anchor| VisualAnchor {
            row: anchor.row,
            key: scoped(&anchor.key),
            source: anchor.source.map(|source| VisualSourceRange {
                identity: scoped(&source.identity),
                start: u64::try_from(source.start).unwrap_or(u64::MAX),
                end: u64::try_from(source.end).unwrap_or(u64::MAX),
            }),
        })
        .collect();
    let selection = selection
        .into_iter()
        .filter(|(_, row)| row.is_valid())
        .map(|(index, row)| {
            (
                index,
                VisualSelectionRow {
                    identity: scoped(&row.identity),
                    byte_start: row.byte_start,
                    text: row.text,
                    cells: row
                        .cells
                        .into_iter()
                        .map(|cell| VisualSelectionCell {
                            column: cell.column,
                            width: cell.width,
                            bytes: cell.bytes,
                        })
                        .collect(),
                },
            )
        });
    let correspondence = VisualCorrespondence::new(anchors, selection, rows.len());
    AcceptedPluginVisual {
        adapter: format!("{plugin}:{}", route.adapter_id),
        revision,
        mode: match render_mode {
            bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::Inline => VisualRenderMode::Inline,
            bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::TranscriptBlock => {
                VisualRenderMode::TranscriptBlock
            }
            bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::FullBlock => {
                VisualRenderMode::FullBlock
            }
        },
        title: header.title,
        // Set by the caller from the visual's source (`render_config`).
        fallback_title: None,
        timeout_ms: header.timeout_ms,
        target,
        summary,
        sections: sections
            .into_iter()
            .take(MAX_PLUGIN_VISUAL_SECTIONS)
            .map(|mut section| {
                section.rows.truncate(MAX_PLUGIN_VISUAL_ROWS);
                VisualSection {
                    name: section.name,
                    label: section.label,
                    rows: Arc::from(section.rows),
                }
            })
            .collect(),
        correspondence: Arc::new(correspondence),
        rows: Arc::from(rows),
        // Native adapters return neither images nor action regions (the routed contract has
        // none); copy-from-source comes from the selection rows above.
        images: Arc::default(),
        actions: Arc::default(),
    }
}

impl TranscriptV2Host {
    /// Content targets of the presented frame (the last painted one, which hit testing uses
    /// until the next frame, CK4), in viewport cells.
    fn presented_content_targets(&self) -> Vec<VisibleContentTarget> {
        self.last_frame
            .as_ref()
            .map(|frame| self.engine.visible_content_targets(frame))
            .unwrap_or_default()
    }

    /// `rect` (viewport cells) in terminal cells, clipped to the viewport and short of an
    /// overlay scrollbar track.
    fn terminal_rect(&self, rect: bmux_tui::component::LogicalRect) -> Option<Rect> {
        let viewport = self.regions.viewport?;
        let cells = |value: u64| u16::try_from(value).unwrap_or(u16::MAX);
        let mut area = Rect::new(
            viewport.x.saturating_add(cells(rect.x)),
            viewport.y.saturating_add(cells(rect.y)),
            cells(rect.width),
            cells(rect.height),
        )
        .intersection(viewport);
        if let Some(track) = self.regions.scrollbar
            && track.x >= area.x
            && track.x < area.right()
        {
            area.width = track.x - area.x;
        }
        (!area.is_empty()).then_some(area)
    }

    /// Terminal cells the host paints over the transcript viewport: the open inspector panel
    /// and the Mermaid source view. Content beneath them is hidden and takes no pointer input
    /// (B7).
    fn overlay_areas(&self) -> Vec<Rect> {
        self.inspector_area()
            .into_iter()
            .chain(self.markdown_source_area())
            .filter(|area| !area.is_empty())
            .collect()
    }

    /// Whether `point` (terminal cells) lies on a host overlay ([`Self::overlay_areas`]).
    fn over_overlay(&self, point: Point) -> bool {
        self.overlay_areas().iter().any(|area| area.contains(point))
    }

    /// Plugin visual rows of the presented frame in terminal cells with their producer-scoped
    /// identities (E1): the root model routes pointer and focused-key input on them to the
    /// adapter. Cells under a host overlay are left out, so a hidden adapter never receives
    /// input meant for the overlay (B7); one target may therefore publish several rectangles.
    #[must_use]
    pub fn committed_visual_targets(&self) -> Vec<(Rect, String)> {
        let overlays = self.overlay_areas();
        self.presented_content_targets()
            .into_iter()
            .filter_map(|target| match target.kind {
                ContentTargetKind::PluginVisual { identity } => {
                    Some((self.terminal_rect(target.rect)?, identity))
                }
                ContentTargetKind::ShellScreen { .. } => None,
            })
            .flat_map(|(area, identity)| {
                uncovered_parts(area, &overlays)
                    .into_iter()
                    .map(move |part| (part, identity.clone()))
            })
            .collect()
    }

    /// Hand the presented frame's reading position to the plugin presentation (E3): retain the
    /// plugin content position the reader is anchored on across reflow, or clear every request
    /// while following the latest content. A request is sent only when it changed.
    pub fn sync_content_positions(&mut self, presentation: &PluginTuiPresentation) {
        let Some(frame) = self.last_frame.as_ref() else {
            return;
        };
        let wanted = if frame.following {
            Some(ContentPosition::Cleared)
        } else {
            match &frame.position {
                ListPosition::Anchored { anchor } | ListPosition::TailSpace { anchor } => {
                    match &anchor.offset {
                        AnchorOffset::Source {
                            position: SourcePosition::PluginSource { identity, offset },
                            ..
                        } => Some(ContentPosition::Retained(identity.clone(), *offset)),
                        _ => None,
                    }
                }
                ListPosition::FollowBottom => Some(ContentPosition::Cleared),
            }
        };
        let Some(wanted) = wanted else {
            return;
        };
        if self.content_input.position.as_ref() == Some(&wanted) {
            return;
        }
        match &wanted {
            ContentPosition::Cleared => presentation.clear_content_positions(),
            ContentPosition::Retained(identity, offset) => presentation
                .retain_content_position(identity, usize::try_from(*offset).unwrap_or(usize::MAX)),
        }
        self.content_input.position = Some(wanted);
    }

    /// Shell screens of the presented frame: terminal cells, item and most columns.
    fn presented_shell_screens(&mut self) -> &[ShellScreen] {
        let frame = self.last_frame.as_ref().map(|frame| frame.id);
        if self
            .content_input
            .shell_screens
            .as_ref()
            .map(|(id, _)| Some(*id))
            != Some(frame)
        {
            let screens = self
                .presented_content_targets()
                .into_iter()
                .filter_map(|target| match (target.unit, target.kind) {
                    (UnitKey::Item(id), ContentTargetKind::ShellScreen { max_columns, .. }) => {
                        Some(ShellScreen {
                            area: self.terminal_rect(target.rect)?,
                            item: id,
                            max_columns,
                        })
                    }
                    _ => None,
                })
                .collect();
            self.content_input.shell_screens = frame.map(|frame| (frame, screens));
        }
        self.content_input
            .shell_screens
            .as_ref()
            .map_or(&[], |(_, screens)| screens.as_slice())
    }

    /// The presented shell screen at terminal `point`: its item and most columns. A point on a
    /// host overlay hits no shell screen (B7).
    fn shell_screen_at(&mut self, point: Point) -> Option<(TranscriptViewItemId, u64)> {
        if self.over_overlay(point) {
            return None;
        }
        self.presented_shell_screens()
            .iter()
            .find(|screen| screen.area.contains(point))
            .map(|screen| (screen.item.clone(), screen.max_columns))
    }

    /// The most columns the presented shell screen of `item` scrolls, while it is visible.
    fn shell_screen(&mut self, item: &TranscriptViewItemId) -> Option<u64> {
        self.presented_shell_screens()
            .iter()
            .find(|screen| &screen.item == item)
            .map(|screen| screen.max_columns)
    }

    /// The shell screen focused by a right-button press (C39), if any (test observation).
    #[cfg(test)]
    #[must_use]
    pub const fn focused_shell_screen(&self) -> Option<&TranscriptViewItemId> {
        self.content_input.focused_shell.as_ref()
    }

    /// Scroll `item`'s shell screen to `columns` and apply it (only that unit is re-measured).
    /// Returns whether it changed.
    fn scroll_shell(&mut self, item: TranscriptViewItemId, columns: u64, at: Instant) -> bool {
        if !self.render.set_shell_columns(item, columns) {
            return false;
        }
        self.flush_render_config(at)
    }

    /// Pointer input on a presented shell screen (C39): horizontal wheel ticks scroll it and a
    /// right-button press focuses it. `None` for any other input.
    pub(super) fn shell_screen_pointer(
        &mut self,
        mouse: MouseEvent,
        at: Instant,
    ) -> Option<TranscriptInputOutcome> {
        let step: i64 = match mouse.kind {
            MouseEventKind::ScrollLeft => -1,
            MouseEventKind::ScrollRight => 1,
            MouseEventKind::Down(MouseButton::Right) => 0,
            _ => return None,
        };
        let (item, max_columns) = self.shell_screen_at(mouse.position)?;
        // Ticks delivered before the next frame accumulate on the requested offset.
        let columns = self.render.shell_columns(&item).min(max_columns);
        if step == 0 {
            self.content_input.focused_shell = Some(item);
            return Some(TranscriptInputOutcome::Consumed);
        }
        let scrolled = shifted(columns, step, max_columns);
        Some(if self.scroll_shell(item, scrolled, at) {
            TranscriptInputOutcome::Redraw
        } else {
            TranscriptInputOutcome::Consumed
        })
    }

    /// A key for the focused shell screen (C39): the `visual` scope's Left/Right scroll it,
    /// Home resets it and Escape releases it. Any other key, or a focused screen that is no
    /// longer visible, releases the focus and is left to the caller (`None`).
    pub(super) fn shell_screen_key(
        &mut self,
        keymap: &BmuxKeyMap,
        stroke: KeyStroke,
        at: Instant,
    ) -> Option<TranscriptInputOutcome> {
        let item = self.content_input.focused_shell.clone()?;
        let Some(max_columns) = self.shell_screen(&item) else {
            self.content_input.focused_shell = None;
            return None;
        };
        let columns = self.render.shell_columns(&item).min(max_columns);
        let scrolled = match keymap.action("visual", stroke) {
            Some("tui.control.left") => shifted(columns, -1, max_columns),
            Some("tui.control.right") => shifted(columns, 1, max_columns),
            Some("tui.control.home") => 0,
            Some("tui.control.cancel") => {
                self.content_input.focused_shell = None;
                return Some(TranscriptInputOutcome::Consumed);
            }
            _ => {
                self.content_input.focused_shell = None;
                return None;
            }
        };
        Some(if self.scroll_shell(item, scrolled, at) {
            TranscriptInputOutcome::Redraw
        } else {
            TranscriptInputOutcome::Consumed
        })
    }
}

/// The parts of `area` no rectangle of `overlays` covers, as disjoint rectangles (at most four
/// per overlay: the bands above and below it and the strips beside it).
fn uncovered_parts(area: Rect, overlays: &[Rect]) -> Vec<Rect> {
    let mut parts = vec![area];
    for overlay in overlays {
        parts = parts
            .into_iter()
            .flat_map(|part| subtract(part, *overlay))
            .collect();
    }
    parts
}

/// `area` without `hole`.
fn subtract(area: Rect, hole: Rect) -> Vec<Rect> {
    let cut = area.intersection(hole);
    if cut.is_empty() {
        return vec![area];
    }
    let bands = [
        Rect::new(area.x, area.y, area.width, cut.y - area.y),
        Rect::new(
            area.x,
            cut.bottom(),
            area.width,
            area.bottom() - cut.bottom(),
        ),
        Rect::new(area.x, cut.y, cut.x - area.x, cut.height),
        Rect::new(cut.right(), cut.y, area.right() - cut.right(), cut.height),
    ];
    bands.into_iter().filter(|band| !band.is_empty()).collect()
}

/// `columns` moved by `steps` of [`SHELL_SCROLL_COLUMNS`], within `0..=max_columns`.
fn shifted(columns: u64, steps: i64, max_columns: u64) -> u64 {
    let delta = SHELL_SCROLL_COLUMNS.saturating_mul(steps.unsigned_abs());
    if steps < 0 {
        columns.saturating_sub(delta)
    } else {
        columns.saturating_add(delta).min(max_columns)
    }
}

#[cfg(test)]
mod tests;
