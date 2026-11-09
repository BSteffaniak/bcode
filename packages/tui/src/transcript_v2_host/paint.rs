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
use bcode_tui_transcript::viewport::{ScrollbarMode, ViewportGeometry};
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
        let update = self.engine.get_mut().set_latest_bar_enabled(true, at);
        self.absorb(update);
        let geometry = ViewportGeometry::new(u64::from(region.width), u64::from(region.height));
        if geometry != self.engine.get().region() {
            let update = self.engine.get_mut().resize(geometry, at);
            self.absorb(update);
        }
    }

    /// Apply the transcript scrollbar setting (VP4, X6): `Off`, an overlay over the last
    /// column, or a reserved gutter column (the engine then lays units out one column
    /// narrower). Returns whether it changed.
    pub fn set_scrollbar(&mut self, mode: ScrollbarMode, at: Instant) -> bool {
        if self.engine.get().scrollbar() == mode {
            return false;
        }
        let update = self.engine.get_mut().set_scrollbar(mode, at);
        self.absorb(update);
        true
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

    /// Prepare and paint one transcript frame into `region` (terminal coordinates).
    ///
    /// Whatever earlier painters staged in `region` (cells, hit regions, selection metadata,
    /// images, a host latest bar) is occluded and the region is cleared first, so only V2
    /// output and V2 interaction metadata remain there (B7). The frame clock is the paint
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
        self.regions.transcript = Some(region);
        self.regions.viewport = None;
        self.regions.latest_bar = None;
        self.regions.scrollbar = None;
        self.glow_frame_at = None;
        if region.is_empty() {
            self.last_frame = None;
            return;
        }
        let clock = self.frame_clock(cx, fallback_now);
        // Scrollbar track and thumb take the latest bar's muted and info styles.
        let _ = self.engine.get_mut().set_scrollbar_styles(ScrollbarStyles {
            begin: style.muted,
            track: style.muted,
            thumb: style.info,
            end: style.muted,
        });
        let snapshot = self.engine.get_mut().prepare_frame(clock);
        let viewport_height = u16::try_from(snapshot.geometry.height)
            .unwrap_or(u16::MAX)
            .min(region.height);
        let viewport = Rect::new(region.x, region.y, region.width, viewport_height);
        let engine = self.engine.get();
        let track = engine.scrollbar_track(&snapshot);
        cx.with_child(
            i32::from(viewport.x),
            i64::from(viewport.y),
            LocalRect::new(0, 0, viewport.width, viewport.height),
            |child| {
                engine.paint_frame(&snapshot, child);
                paint_interaction(engine, &snapshot, child, style.focus, track);
            },
        );
        self.regions.viewport = Some(viewport);
        self.regions.scrollbar = track
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
            let bar = Rect::new(region.x, bar_row, region.width, 1);
            paint_latest_bar(snapshot.latest_bar, bar, cx, style, clock.now);
            self.regions.latest_bar = Some(bar);
            self.glow_frame_at = next_glow_frame(snapshot.latest_bar, style.glow_window, clock.now);
        }
        self.last_frame = Some(snapshot);
        self.invalidated = false;
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

    /// Record that the last painted frame was presented (CK3).
    pub fn commit(&mut self) {
        if let Some(frame) = self.last_frame.as_ref().map(|frame| frame.id) {
            let update = self.engine.get_mut().commit(frame);
            self.absorb(update);
        }
    }
}

/// Hit regions and the focus highlight of a prepared frame, from the retained geometry the
/// frame painted (B3, B7, B12). `cx` is the viewport's context. Item hit regions stop short of
/// the scrollbar `track` (viewport cells), which overlays the last column in overlay mode, so a
/// press there is the scrollbar's.
fn paint_interaction(
    engine: &bcode_tui_transcript::engine::TranscriptEngine,
    snapshot: &FrameSnapshot,
    cx: &mut PaintCx<'_, '_>,
    focus: Style,
    track: Option<Rect>,
) {
    if let Some(rows) = engine.focus_rows(snapshot) {
        let height = u16::try_from(rows.end - rows.start).unwrap_or(u16::MAX);
        let top = u16::try_from(rows.start).unwrap_or(u16::MAX);
        let width = u16::try_from(snapshot.geometry.width).unwrap_or(u16::MAX);
        cx.restyle(LocalRect::new(0, i64::from(top), width, height), focus);
    }
    for (index, region) in engine.visible_hit_regions(snapshot).into_iter().enumerate() {
        let x = u16::try_from(region.rect.x).unwrap_or(u16::MAX);
        let mut width = u16::try_from(region.rect.width).unwrap_or(u16::MAX);
        if let Some(track) = track {
            width = width.min(track.x.saturating_sub(x));
        }
        if width == 0 {
            continue;
        }
        let rect = Rect::new(
            x,
            u16::try_from(region.rect.y).unwrap_or(u16::MAX),
            width,
            u16::try_from(region.rect.height).unwrap_or(u16::MAX),
        );
        cx.push_hit(
            HitRegion::new(format!("{ITEM_HIT_PREFIX}{index}"), rect)
                .role(HitRole::Action)
                .layer(1),
        );
    }
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
