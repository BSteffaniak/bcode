//! Transcript frames for the V2 engine (spec sections 3.6 and 9, rules F1–F7).
//!
//! The chat-screen chrome (header, status, composer) keeps the host frame layout; the V2
//! transcript owns the whole transcript region, including the latest-bar row (F6). Each frame
//! is prepared once with the presentation clock (F2) and painted through BMUX `PaintCx` only
//! (B9, F3). Cells, hit regions and the focus highlight come from the same prepared snapshot
//! and retained layouts (B3, B7, B12), and the regions recorded here are the ones pointer input
//! is resolved against ([`super::input`]).

use std::time::{Duration, Instant};

use bcode_tui_components::activity;
use bcode_tui_transcript::frame::{FrameClock, FrameSnapshot};
use bcode_tui_transcript::latest_bar::LatestBarState;
use bcode_tui_transcript::layout::ActivityPhase;
use bcode_tui_transcript::viewport::{ScrollbarMode, ViewportGeometry};
use bmux_tui::damage::{Damage, DamagePolicy};
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::Line;
use bmux_tui::style::Style;
use bmux_tui_components::scrollbar::ScrollbarStyles;

use super::TranscriptV2Host;
use crate::render::FrameLayout;

/// Hit id of the V2 latest bar (distinct from the V1 bar, so V1 pointer paths ignore it).
pub const LATEST_BAR_HIT: &str = "transcript-v2.latest-bar";
/// Hit id prefix of V2 transcript interactive regions.
pub const ITEM_HIT_PREFIX: &str = "transcript-v2.item:";

/// Damage bounds of transcript scroll damage: the root frame's region policy (BMUX 11).
const DAMAGE_POLICY: DamagePolicy = DamagePolicy {
    max_regions: 64,
    max_area_percent: 100,
};

/// Terminal rectangles of the last painted frame, used to resolve pointer input against what
/// was presented (B7).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Regions {
    /// The whole transcript region, latest-bar row included.
    pub transcript: Option<Rect>,
    /// The transcript viewport painted by the engine (the region minus the latest bar).
    pub viewport: Option<Rect>,
    /// The latest bar, while shown.
    pub latest_bar: Option<Rect>,
    /// The transcript scrollbar track, while a scrollbar is presented (VP4, X6). An overlay
    /// track lies over the viewport's last column; item hit regions exclude it.
    pub scrollbar: Option<Rect>,
    /// The composer panel (a press there returns focus to the composer, FO3).
    pub composer: Option<Rect>,
}

/// Repaint scope of the transcript's changes since the host shell last asked (F5, D6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TranscriptDamage {
    /// Nothing changed.
    None,
    /// Only the transcript region (viewport and latest bar) changed.
    Transcript,
    /// The whole surface needs repainting (focus kind, the inspector overlay, notices).
    Full,
}

/// Styles and labels a V2 transcript frame is painted with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptPaintStyle<'a> {
    /// Transcript region background.
    pub background: Style,
    /// Latest-bar message while glowing.
    pub text: Style,
    /// Latest-bar message at rest.
    pub muted: Style,
    /// Latest-bar jump affordance and glow peaks.
    pub info: Style,
    /// Latest-bar glow trail.
    pub accent: Style,
    /// Focus highlight layered over the focused target's rows (paint-only, FO7).
    pub focus: Style,
    /// Selection highlight patched over selected cells (C34).
    pub selection: Style,
    /// Jump-to-latest key label ("New messages below · KEY to jump").
    pub jump_key: &'a str,
    /// Latest-bar glow window from the motion policy; zero under reduced motion (C21, C22).
    pub glow_window: Duration,
}

/// The transcript region of a host frame layout: the transcript body plus the row the host
/// layout reserves for its own latest bar, which the V2 transcript owns (F6).
#[must_use]
pub fn transcript_region(layout: FrameLayout) -> Rect {
    let body = layout.body();
    layout.latest_bar().map_or(body, |bar| {
        Rect::new(
            body.x,
            body.y,
            body.width,
            body.height.saturating_add(bar.height),
        )
    })
}

impl TranscriptV2Host {
    /// Apply the transcript region's size before a frame is prepared. The region includes the
    /// latest-bar row; the engine reserves it while the bar is shown (F6). A changed size is a
    /// navigation geometry input (keyed anchors keep the reading position, P1).
    pub fn sync_geometry(&mut self, region: Rect, at: Instant) {
        let update = self.engine.set_latest_bar_enabled(true, at);
        self.absorb(update);
        let geometry = ViewportGeometry::new(u64::from(region.width), u64::from(region.height));
        if geometry != self.engine.region() {
            let update = self.engine.resize(geometry, at);
            self.absorb(update);
        }
    }

    /// Apply the transcript scrollbar setting (VP4, X6): `Off`, an overlay over the last
    /// column, or a reserved gutter column (the engine then lays units out one column
    /// narrower). Returns whether it changed.
    pub fn set_scrollbar(&mut self, mode: ScrollbarMode, at: Instant) -> bool {
        if self.engine.scrollbar() == mode {
            return false;
        }
        let update = self.engine.set_scrollbar(mode, at);
        self.absorb(update);
        true
    }

    /// The repaint scope of changes since the last call, then reset the full-damage mark. The
    /// transcript stays damaged until it is painted.
    pub const fn take_damage(&mut self) -> TranscriptDamage {
        if std::mem::replace(&mut self.full_damage, false) {
            TranscriptDamage::Full
        } else if self.invalidated {
            TranscriptDamage::Transcript
        } else {
            TranscriptDamage::None
        }
    }

    /// Accept the host shell's activity frame as the spinner and elapsed phase (F7, D7):
    /// labels advance only when the shell accepted a new activity frame, and the wall-clock
    /// milliseconds elapsed labels count to are sampled here, in the update, never while
    /// drawing (F2).
    pub fn sync_activity(&mut self, spinner_step: usize) {
        let frame = u64::try_from(spinner_step).unwrap_or(u64::MAX);
        if self.activity_frame == Some(frame) {
            return;
        }
        self.activity_frame = Some(frame);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok());
        let update = self.engine.set_activity(ActivityPhase {
            spinner_frame: frame,
            now_ms,
        });
        self.absorb(update);
    }

    /// Record the composer panel (FO3: a press there returns focus to the composer).
    pub const fn set_composer_area(&mut self, composer: Rect) {
        self.regions.composer = Some(composer);
    }

    /// Terminal rectangles of the last painted frame.
    #[must_use]
    pub const fn regions(&self) -> Regions {
        self.regions
    }

    /// Prepare the frame for `region` (terminal coordinates) at `clock` (spec 9.1 step 2): the
    /// engine builds one snapshot from its derived caches (F1), and the host records the
    /// regions pointer input resolves against and the next glow deadline. Painting
    /// ([`Self::paint`]) then reads only this snapshot (F3). A frame prepared but never
    /// painted or presented has no navigation effect (CK1).
    pub fn prepare(&mut self, region: Rect, clock: FrameClock, style: &TranscriptPaintStyle<'_>) {
        self.regions.transcript = Some(region);
        self.regions.viewport = None;
        self.regions.latest_bar = None;
        self.regions.scrollbar = None;
        self.glow_frame_at = None;
        self.prepared = None;
        if region.is_empty() {
            self.last_frame = None;
            return;
        }
        // Scrollbar track and thumb take the latest bar's muted and info styles.
        let _ = self.engine.set_scrollbar_styles(ScrollbarStyles {
            begin: style.muted,
            track: style.muted,
            thumb: style.info,
            end: style.muted,
        });
        let snapshot = self.engine.prepare_frame(clock);
        let viewport_height = u16::try_from(snapshot.geometry.height)
            .unwrap_or(u16::MAX)
            .min(region.height);
        let viewport = Rect::new(region.x, region.y, region.width, viewport_height);
        self.regions.viewport = Some(viewport);
        self.regions.scrollbar = snapshot
            .scrollbar_track
            .map(|track| {
                Rect::new(
                    viewport.x.saturating_add(track.x),
                    viewport.y.saturating_add(track.y),
                    track.width,
                    track.height.min(viewport.height),
                )
            })
            .filter(|track| !track.is_empty());
        let bar_row = viewport.bottom();
        if snapshot.latest_bar.visible && bar_row < region.bottom() {
            self.regions.latest_bar = Some(Rect::new(region.x, bar_row, region.width, 1));
            self.glow_frame_at = next_glow_frame(snapshot.latest_bar, style.glow_window, clock.now);
        }
        self.prepared = Some(region);
        self.last_frame = Some(snapshot);
    }

    /// The damage of the prepared frame given the host shell's `damage` (F5, D6, BMUX 11).
    ///
    /// When `damage` repaints the whole transcript region and the prepared
    /// frame differs from the committed one only by its scroll position
    /// ([`FrameSnapshot::scrolled_from`]), the transcript viewport becomes `Damage::Scroll`
    /// with every other region (the latest-bar row included) repainted as well. Anything else
    /// (full damage, an open inspector or inline interaction over the transcript, any content,
    /// focus or selection change) is returned unchanged: scroll damage is only an output hint
    /// and repaints exactly the same regions.
    #[must_use]
    pub fn frame_damage(&self, damage: Damage, area: Rect) -> Damage {
        let (Some(region), Some(viewport)) = (self.regions.transcript, self.regions.viewport)
        else {
            return damage;
        };
        let Damage::Regions(regions) = &damage else {
            return damage;
        };
        // Region damage is merged, so the transcript region may be part of a larger region.
        if !regions
            .iter()
            .any(|damaged| damaged.intersection(region) == region)
            || self.inspector_open()
            || self.inline_interaction_area().is_some()
        {
            return damage;
        }
        let rows = self
            .last_frame
            .as_ref()
            .zip(self.committed_frame.as_ref())
            .and_then(|(frame, committed)| frame.scrolled_from(committed));
        let Some(rows) = rows else {
            return damage;
        };
        let bar = Rect::new(
            region.x,
            viewport.bottom(),
            region.width,
            region.bottom().saturating_sub(viewport.bottom()),
        );
        let also = regions
            .iter()
            .copied()
            .filter(|other| *other != region)
            .chain((!bar.is_empty()).then_some(bar));
        Damage::scroll(viewport, rows, also, area, DAMAGE_POLICY)
    }

    /// `damage` extended by the transcript region, for a transcript change made while the frame
    /// is being drawn (an inline interaction reservation sized from the drawn layout), so the
    /// change is painted in this frame rather than left for a later full repaint (F5).
    #[must_use]
    pub fn with_transcript_damage(&self, damage: Damage, area: Rect) -> Damage {
        let Some(region) = self.regions.transcript else {
            return Damage::Full;
        };
        damage.merge(
            Damage::regions([region], area, DAMAGE_POLICY),
            area,
            DAMAGE_POLICY,
        )
    }

    /// Paint the prepared frame into `region` (terminal coordinates), preparing it first when
    /// it was not prepared for `region` ([`Self::prepare`]).
    ///
    /// Whatever earlier painters staged in `region` (cells, hit regions, selection metadata,
    /// images, a host latest bar) is occluded and the region is cleared first, so only V2
    /// output and V2 interaction metadata remain there (B7). Cells, hit regions, the focus
    /// highlight, selection fragments and the selection highlight come from the snapshot and
    /// the retained layouts it names, in one pass (F3, F4). The frame clock is the paint
    /// context's presentation clock when it has one, else `fallback_now` (F2).
    pub fn paint(
        &mut self,
        region: Rect,
        cx: &mut PaintCx<'_, '_>,
        style: &TranscriptPaintStyle<'_>,
        fallback_now: Instant,
    ) {
        let area = LocalRect::terminal(region);
        cx.occlude(area);
        cx.fill(area, " ", style.background);
        if self.prepared != Some(region) {
            let clock = self.frame_clock(cx, fallback_now);
            self.prepare(region, clock, style);
        }
        self.prepared = None;
        self.invalidated = false;
        let (Some(snapshot), Some(viewport)) = (self.last_frame.as_ref(), self.regions.viewport)
        else {
            return;
        };
        let engine = &self.engine;
        cx.with_child(
            i32::from(viewport.x),
            i64::from(viewport.y),
            LocalRect::new(0, 0, viewport.width, viewport.height),
            |child| {
                engine.paint_frame(snapshot, child);
                engine.paint_selection(snapshot, child, style.selection);
                paint_interaction(snapshot, child, style.focus);
            },
        );
        if let Some(bar) = self.regions.latest_bar {
            paint_latest_bar(snapshot.latest_bar, bar, cx, style, snapshot.clock.now);
        }
    }

    /// The frame's single clock sample (F2): the paint context's presentation clock, else
    /// `fallback_now`. Scripted host time ([`TranscriptV2Host::now`]) replaces both in tests.
    #[cfg_attr(
        not(test),
        expect(clippy::unused_self, reason = "only tests script the frame clock")
    )]
    fn frame_clock(&self, cx: &PaintCx<'_, '_>, fallback_now: Instant) -> FrameClock {
        #[cfg(test)]
        if let Some(now) = self.scripted_now {
            return FrameClock { now };
        }
        cx.clock()
            .map_or(FrameClock { now: fallback_now }, FrameClock::from)
    }

    /// The frame clock of a presentation sampled at `clock` (F2), else the host clock.
    /// Scripted host time ([`TranscriptV2Host::now`]) replaces both in tests.
    #[must_use]
    pub fn presentation_frame_clock(
        &self,
        clock: Option<bmux_tui::clock::PresentationClock>,
    ) -> FrameClock {
        #[cfg(test)]
        if let Some(now) = self.scripted_now {
            return FrameClock { now };
        }
        clock.map_or_else(|| FrameClock { now: self.now() }, FrameClock::from)
    }

    /// When the latest-bar glow needs its next frame (C21); `None` while the bar is still.
    #[must_use]
    pub const fn presentation_deadline(&self) -> Option<Instant> {
        self.glow_frame_at
    }

    /// Request the next glow frame when its deadline passed. Returns whether a repaint is due.
    pub fn advance_presentation(&mut self, now: Instant) -> bool {
        if self.glow_frame_at.is_some_and(|at| at <= now) {
            self.glow_frame_at = None;
            self.invalidated = true;
            return true;
        }
        false
    }

    /// Record that the last painted frame was presented (CK3); it becomes the frame the next
    /// frame's scroll damage is measured against (BMUX 11).
    pub fn commit(&mut self) {
        if let Some(frame) = self.last_frame.as_ref() {
            let update = self.engine.commit(frame.id);
            self.committed_frame = Some(frame.clone());
            self.absorb(update);
        }
    }
}

/// Hit regions and the focus highlight of a prepared frame, from the retained geometry the
/// snapshot recorded (B3, B7, B12). `cx` is the viewport's context. Item hit regions stop short
/// of the scrollbar track (viewport cells), which overlays the last column in overlay mode, so
/// a press there is the scrollbar's.
fn paint_interaction(snapshot: &FrameSnapshot, cx: &mut PaintCx<'_, '_>, focus: Style) {
    if let Some(rows) = &snapshot.focus_rows {
        let height = u16::try_from(rows.end - rows.start).unwrap_or(u16::MAX);
        let top = u16::try_from(rows.start).unwrap_or(u16::MAX);
        let width = u16::try_from(snapshot.geometry.width).unwrap_or(u16::MAX);
        cx.restyle(LocalRect::new(0, i64::from(top), width, height), focus);
    }
    for (index, region) in item_hit_regions(snapshot) {
        cx.push_hit(
            HitRegion::new(format!("{ITEM_HIT_PREFIX}{index}"), region)
                .role(HitRole::Action)
                .layer(1),
        );
    }
}

/// The item hit regions of a prepared frame in viewport cells, with their indices, clipped to
/// stop short of the scrollbar track.
pub(super) fn item_hit_regions(
    snapshot: &FrameSnapshot,
) -> impl Iterator<Item = (usize, Rect)> + '_ {
    let track = snapshot.scrollbar_track;
    snapshot
        .hit_regions
        .iter()
        .enumerate()
        .filter_map(move |(index, region)| {
            let x = u16::try_from(region.rect.x).unwrap_or(u16::MAX);
            let mut width = u16::try_from(region.rect.width).unwrap_or(u16::MAX);
            if let Some(track) = track {
                width = width.min(track.x.saturating_sub(x));
            }
            (width > 0).then(|| {
                (
                    index,
                    Rect::new(
                        x,
                        u16::try_from(region.rect.y).unwrap_or(u16::MAX),
                        width,
                        u16::try_from(region.rect.height).unwrap_or(u16::MAX),
                    ),
                )
            })
        })
}

/// Paint the latest bar (F6, C21) and register its hit region (one rectangle for both).
fn paint_latest_bar(
    state: LatestBarState,
    bar: Rect,
    cx: &mut PaintCx<'_, '_>,
    style: &TranscriptPaintStyle<'_>,
    now: Instant,
) {
    cx.push_hit(
        HitRegion::new(LATEST_BAR_HIT, bar)
            .role(HitRole::ListItem)
            .layer(1),
    );
    let line = latest_bar_line(state, bar.width, style, now);
    let area = LocalRect::terminal(bar);
    cx.fill(area, " ", style.background);
    cx.write_line_with_fallback_style(area, &line, style.background);
}

/// The bar's line: the still banner, or the glowing banner while activity is recent. Both
/// are the shared latest-activity component recipes.
fn latest_bar_line(
    state: LatestBarState,
    width: u16,
    style: &TranscriptPaintStyle<'_>,
    now: Instant,
) -> Line {
    let Some(age) = state.active_age(now, style.glow_window) else {
        return activity::stale_latest_activity_line(
            width,
            style.jump_key,
            activity::LatestActivityStyle {
                background: style.background,
                muted: style.muted,
                info: style.info,
            },
        );
    };
    let burst = activity::latest_activity_effective_burst(state.burst, age, style.glow_window);
    let phase = usize::try_from(
        age.as_millis() / activity::latest_activity_frame(burst).as_millis().max(1),
    )
    .unwrap_or_default();
    activity::glowing_latest_activity_line(
        width,
        style.jump_key,
        burst,
        phase,
        activity::GlowingLatestActivityStyle {
            background: style.background,
            text: style.text,
            info: style.info,
            accent: style.accent,
        },
    )
}

/// Next glow repaint: the next animation frame inside the window, then one final still frame
/// when the window ends; `None` once still or under reduced motion (C21, C22).
fn next_glow_frame(state: LatestBarState, window: Duration, now: Instant) -> Option<Instant> {
    let age = state.active_age(now, window)?;
    let frame = activity::latest_activity_frame(activity::latest_activity_effective_burst(
        state.burst,
        age,
        window,
    ));
    let end = state.activity_at? + window;
    Some((now + frame).min(end))
}

#[cfg(test)]
mod tests;
