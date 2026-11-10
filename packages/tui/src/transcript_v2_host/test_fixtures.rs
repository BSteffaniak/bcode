//! Shared fixtures for V2 host input and paint tests: a host attached to a session through the
//! real effect path, painted into an off-screen buffer.

use std::time::{Duration, Instant};

use bcode_session_models::{ClientId, SessionEvent, SessionEventKind, SessionId};
use bcode_tui_transcript::engine::HostRequest;
use bcode_tui_transcript::window::HistoryWindowResult;
use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::hit::HitRegion;
use bmux_tui::paint::PaintCx;
use bmux_tui::style::{Modifier, Style};

use super::TranscriptV2Host;
use super::paint::TranscriptPaintStyle;
use crate::effects::{TuiEffect, TuiEffectResult};
use crate::transcript_v2_host::effects::HistoryLoaded;

/// Transcript region of the fixtures.
pub const REGION: Rect = Rect::new(0, 1, 50, 12);
/// Composer panel of the fixtures.
pub const COMPOSER: Rect = Rect::new(0, 14, 50, 4);

pub fn event(session_id: SessionId, sequence: u64, kind: SessionEventKind) -> SessionEvent {
    SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence,
        timestamp_ms: sequence,
        session_id,
        provenance: None,
        kind,
    }
}

/// `turns` turns of a user message, one tool call and a three-line answer.
pub fn conversation(session_id: SessionId, turns: u64) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    for turn in 0..turns {
        let base = turn * 3;
        events.push(event(
            session_id,
            base + 1,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: format!("question {turn}"),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ));
        events.push(event(
            session_id,
            base + 2,
            SessionEventKind::ToolCallRequested {
                tool_call_id: format!("call-{turn}"),
                producer_plugin_id: None,
                tool_name: "read".to_owned(),
                arguments_json: "{}".to_owned(),
                working_directory: None,
            },
        ));
        events.push(event(
            session_id,
            base + 3,
            SessionEventKind::AssistantMessage {
                text: format!("answer {turn} line 0\\\nline 1\\\nline 2"),
            },
        ));
    }
    events
}

/// A host attached to a session of `turns` turns, all resident, geometry synced to
/// [`REGION`] and the composer recorded at [`COMPOSER`].
pub fn attached(turns: u64, at: Instant) -> TranscriptV2Host {
    attached_to(SessionId::new(), turns, at)
}

/// [`attached`] for a given session.
pub fn attached_to(session: SessionId, turns: u64, at: Instant) -> TranscriptV2Host {
    let mut host = TranscriptV2Host::new();
    host.sync_geometry(REGION, at);
    host.set_composer_area(COMPOSER);
    host.sync_session(Some(session), at);
    let effects = host.drain_effects();
    let [TuiEffect::LoadTranscriptV2History { load }] = effects.as_slice() else {
        panic!("one tail request, got {} effects", effects.len());
    };
    let HostRequest::Window(issued) = load.request else {
        panic!("window request");
    };
    let events = conversation(session, turns);
    let last = events.last().map(|event| event.sequence);
    let result = TuiEffectResult::TranscriptV2HistoryLoaded {
        load: *load,
        result: Ok(HistoryLoaded::Window(HistoryWindowResult {
            generation: issued.generation,
            target: issued.request,
            events,
            has_older: false,
            has_newer: false,
            first_available_sequence: Some(1),
            last_available_sequence: last,
        })),
    };
    assert!(host.apply_effect_result(result, at));
    host
}

/// Paint styles of the fixtures: the focus highlight is reversed video.
pub fn style(glow_window: Duration) -> TranscriptPaintStyle<'static> {
    TranscriptPaintStyle {
        background: Style::new(),
        text: Style::new(),
        muted: Style::new(),
        info: Style::new(),
        accent: Style::new(),
        focus: Style::new().add_modifier(Modifier::REVERSED),
        selection: Style::new().add_modifier(Modifier::UNDERLINE),
        jump_key: "ctrl+end",
        glow_window,
    }
}

/// One painted frame.
pub struct Painted {
    pub buffer: Buffer,
    pub hits: Vec<HitRegion>,
}

impl Painted {
    /// Symbols of terminal row `y`.
    pub fn row(&self, y: u16) -> String {
        self.buffer.row_symbols(y).unwrap_or_default()
    }

    /// Whether the cell at (`x`, `y`) carries the focus highlight.
    pub fn highlighted(&self, x: u16, y: u16) -> bool {
        self.buffer
            .get(bmux_tui::geometry::Point::new(x, y))
            .is_some_and(|cell| cell.style.modifiers.contains(Modifier::REVERSED))
    }
}

/// Sync geometry (as every host frame does), paint one frame at `at` and commit it.
pub fn paint(host: &mut TranscriptV2Host, at: Instant, glow_window: Duration) -> Painted {
    host.sync_geometry(REGION, at);
    let area = Rect::new(0, 0, REGION.width, REGION.bottom() + COMPOSER.height + 1);
    let mut buffer = Buffer::empty(area);
    let hits = {
        let mut frame = Frame::new(&mut buffer);
        frame.set_clock(Some(bmux_tui::clock::PresentationClock::new(at)));
        host.paint(
            REGION,
            &mut PaintCx::new(&mut frame),
            &style(glow_window),
            at,
        );
        frame.hits().regions().to_vec()
    };
    host.commit();
    Painted { buffer, hits }
}
