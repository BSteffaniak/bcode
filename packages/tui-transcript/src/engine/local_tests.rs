//! Process-local rows (LR1–LR7) observed through prepared and painted frames.

use std::time::Duration;
use web_time::Instant;

use bcode_session_models::{
    ClientId, SessionEvent, SessionEventKind, SessionId, TurnAdmissionMetadata,
};
use bcode_session_view_models::{
    ChatMessageView, TextFormat, TranscriptViewItem, TranscriptViewItemId, TranscriptViewItemKind,
};
use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::PaintCx;

use crate::engine::{HostRequest, TranscriptEngine};
use crate::frame::FrameClock;
use crate::navigation::{InputSource, MotionConfig, NavIntent, NavMode};
use crate::units::UnitKey;
use crate::units::local::{LocalKey, LocalPlacement, LocalRow};
use crate::viewport::ViewportGeometry;
use crate::window::{HistoryWindowResult, LiveInput};

const WIDTH: u64 = 40;
const HEIGHT: u64 = 10;

fn event(session: SessionId, sequence: u64, text: &str) -> SessionEvent {
    SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence,
        timestamp_ms: sequence,
        session_id: session,
        provenance: None,
        kind: if sequence % 2 == 1 {
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: text.to_owned(),
                admission: TurnAdmissionMetadata::default(),
            }
        } else {
            SessionEventKind::AssistantMessage {
                text: text.to_owned(),
            }
        },
    }
}

fn message(text: &str, label: Option<&str>) -> ChatMessageView {
    ChatMessageView {
        display_label: label.map(str::to_owned),
        format: TextFormat::PlainText,
        ..ChatMessageView::markdown(text)
    }
}

fn row_item(kind: TranscriptViewItemKind) -> TranscriptViewItem {
    TranscriptViewItem {
        id: TranscriptViewItemId::new("replaced"),
        revision: 1,
        sequence: None,
        timestamp_ms: None,
        output_location: None,
        streaming: false,
        kind,
    }
}

fn notice(id: u64, after: Option<u64>, text: &str) -> LocalRow {
    LocalRow::new(
        LocalKey::Notice(id),
        LocalPlacement::After {
            item: after.map(TranscriptViewItemId::event),
            sequence: after,
        },
        row_item(TranscriptViewItemKind::SystemMessage {
            message: message(text, None),
        }),
    )
}

fn pending(id: &str, text: &str, state: &str) -> LocalRow {
    LocalRow::new(
        LocalKey::Submission(id.to_owned()),
        LocalPlacement::Tail,
        row_item(TranscriptViewItemKind::UserMessage {
            message: message(text, Some(state)),
        }),
    )
}

struct Harness {
    engine: TranscriptEngine,
    session: SessionId,
    next: u64,
    at: Instant,
}

impl Harness {
    /// Attach a session of `events` one-line events with plain rows.
    fn attach(events: u64) -> Self {
        Self::attach_with(
            events,
            TranscriptEngine::with_producer(
                ViewportGeometry::new(WIDTH, HEIGHT),
                MotionConfig::INSTANT,
                Box::new(crate::layout::producers::testing::PlainTextProducer),
            ),
        )
    }

    /// Attach a session of `events` one-line events to `engine`.
    fn attach_with(events: u64, mut engine: TranscriptEngine) -> Self {
        let session = SessionId::new();
        let at = Instant::now();
        let update = engine.attach(at);
        let HostRequest::Window(issued) = update.requests[0] else {
            panic!("attach requests a window");
        };
        let result = HistoryWindowResult {
            generation: issued.generation,
            target: issued.request,
            events: (1..=events)
                .map(|sequence| event(session, sequence, &format!("m{sequence}")))
                .collect(),
            has_older: false,
            has_newer: false,
            first_available_sequence: Some(1),
            last_available_sequence: Some(events),
        };
        engine.apply_window(&result, at).expect("attach");
        Self {
            engine,
            session,
            next: events + 1,
            at,
        }
    }

    fn live(&mut self, text: &str) -> u64 {
        let sequence = self.next;
        self.next += 1;
        let _ = self.engine.apply_live(
            LiveInput::Durable(Box::new(event(self.session, sequence, text))),
            self.at,
        );
        sequence
    }

    fn set(&mut self, rows: Vec<LocalRow>) -> bool {
        self.engine.set_local_rows(rows, self.at).invalidated
    }

    fn present(&mut self) -> Vec<String> {
        self.at += Duration::from_millis(1);
        let clock = FrameClock { now: self.at };
        let snapshot = self.engine.prepare_frame(clock);
        let width = u16::try_from(WIDTH).expect("width");
        let height = u16::try_from(HEIGHT).expect("height");
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        let mut frame = Frame::new(&mut buffer);
        frame.set_clock(Some(clock.presentation()));
        self.engine
            .paint_frame(&snapshot, &mut PaintCx::new(&mut frame));
        let rows = (0..height)
            .map(|row| {
                frame
                    .buffer()
                    .row_symbols(row)
                    .unwrap_or_default()
                    .trim_end()
                    .to_owned()
            })
            .collect();
        let _ = self.engine.commit(snapshot.id);
        rows
    }
}

fn row_of(rows: &[String], needle: &str) -> Option<usize> {
    rows.iter().position(|row| row == needle)
}

#[test]
fn lr2_pending_rows_follow_the_canonical_tail_in_order() {
    let mut harness = Harness::attach(4);
    assert!(harness.set(vec![
        pending("a", "first", "sending"),
        pending("b", "second", "queued #1"),
    ]));
    let rows = harness.present();
    let last = row_of(&rows, "m4").expect("canonical tail visible");
    assert_eq!(row_of(&rows, "first"), Some(last + 1));
    assert_eq!(row_of(&rows, "second"), Some(last + 2));
    // A canonical item arriving later still precedes the pending rows.
    harness.live("m5");
    let rows = harness.present();
    assert_eq!(
        row_of(&rows, "first"),
        row_of(&rows, "m5").map(|row| row + 1)
    );
}

#[test]
fn lr2_notices_keep_their_chronology_boundary() {
    let mut harness = Harness::attach(4);
    harness.set(vec![
        notice(1, Some(2), "after two"),
        notice(2, None, "first"),
    ]);
    harness.live("m5");
    let rows = harness.present();
    assert_eq!(row_of(&rows, "first"), Some(1), "before every item");
    assert_eq!(
        row_of(&rows, "after two"),
        row_of(&rows, "m2").map(|row| row + 1)
    );
    assert_eq!(
        row_of(&rows, "m3"),
        row_of(&rows, "after two").map(|row| row + 1)
    );
}

#[test]
fn lr2_a_notice_whose_anchor_left_falls_back_to_its_sequence() {
    let mut harness = Harness::attach(4);
    let mut row = notice(1, Some(3), "fallback");
    row.placement = LocalPlacement::After {
        item: Some(TranscriptViewItemId::new("not-resident")),
        sequence: Some(3),
    };
    harness.set(vec![row]);
    let rows = harness.present();
    assert_eq!(
        row_of(&rows, "fallback"),
        row_of(&rows, "m3").map(|row| row + 1)
    );
}

#[test]
fn lr3_equal_rows_are_a_no_op_and_a_state_change_revises_only_that_row() {
    let mut harness = Harness::attach(4);
    let rows = vec![notice(1, Some(4), "note"), pending("a", "text", "sending")];
    assert!(harness.set(rows.clone()));
    let before = harness.engine.units().keys().to_vec();
    let notice_revision = harness
        .engine
        .units()
        .revision(&UnitKey::Local(LocalKey::Notice(1)));
    assert!(!harness.set(rows), "equal rows change nothing");
    assert!(harness.set(vec![
        notice(1, Some(4), "note"),
        pending("a", "text", "queued")
    ]));
    assert_eq!(harness.engine.units().keys(), before.as_slice());
    assert_eq!(
        harness
            .engine
            .units()
            .revision(&UnitKey::Local(LocalKey::Notice(1))),
        notice_revision
    );
}

#[test]
fn lr7_following_shows_a_new_local_row_and_reading_does_not_move() {
    let mut harness = Harness::attach(30);
    harness.present();
    assert_eq!(harness.engine.nav().mode, NavMode::Following);
    harness.set(vec![pending("a", "pending text", "sending")]);
    let rows = harness.present();
    assert_eq!(row_of(&rows, "pending text"), Some(9), "on the bottom row");
    assert_eq!(harness.engine.nav().mode, NavMode::Following);

    let _ = harness.engine.intent(
        NavIntent::ScrollBy {
            rows: -6,
            source: InputSource::Wheel,
        },
        harness.at,
    );
    let reading = harness.present();
    harness.set(vec![
        pending("a", "pending text", "sending"),
        notice(1, Some(30), "a notice"),
    ]);
    assert_eq!(harness.present(), reading, "the reader does not move");
}

#[test]
fn lr6_a_canonical_message_before_a_pending_row_is_revealed_on_acceptance() {
    let mut harness = Harness::attach(30);
    harness.present();
    harness.set(vec![pending("a", "zq", "sending")]);
    harness.present();
    let sequence = harness.live("zq");
    let item = UnitKey::Item(TranscriptViewItemId::event(sequence));
    let _ = harness.engine.submission_accepted(item.clone(), harness.at);
    harness.set(Vec::new());
    let rows = harness.present();
    assert_eq!(
        row_of(&rows, "zq"),
        Some(0),
        "the accepted message is on top"
    );
    assert!(
        matches!(&harness.engine.nav().mode, NavMode::Holding { item: held, .. } if *held == item)
    );
}

#[test]
fn lr1_local_rows_render_before_attach() {
    let mut engine = TranscriptEngine::with_producer(
        ViewportGeometry::new(WIDTH, HEIGHT),
        MotionConfig::INSTANT,
        Box::new(crate::layout::producers::testing::PlainTextProducer),
    );
    let at = Instant::now();
    let _ = engine.set_local_rows(vec![pending("a", "draft text", "sending")], at);
    let snapshot = engine.prepare_frame(FrameClock { now: at });
    let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 10));
    let mut frame = Frame::new(&mut buffer);
    engine.paint_frame(&snapshot, &mut PaintCx::new(&mut frame));
    assert!((0..10).any(|row| {
        frame
            .buffer()
            .row_symbols(row)
            .is_some_and(|row| row.trim_end() == "draft text")
    }));
}

#[test]
fn turn_summaries_render_as_one_row_led_by_the_theme_glyph() {
    let mut theme = bcode_tui_theme::definition::ResolvedThemeDefinition::empty("t", "T");
    theme.chrome.turn_summary.glyph = bcode_tui_theme::chrome::ChromeText::new("✻");
    let engine = TranscriptEngine::new(ViewportGeometry::new(WIDTH, HEIGHT), MotionConfig::INSTANT)
        .with_render_config(crate::layout::RenderConfig::for_theme(std::sync::Arc::new(
            theme,
        )));
    let mut harness = Harness::attach_with(2, engine);
    harness.set(vec![LocalRow::new(
        LocalKey::Summary(1),
        LocalPlacement::After {
            item: Some(TranscriptViewItemId::event(2)),
            sequence: Some(2),
        },
        row_item(TranscriptViewItemKind::SystemMessage {
            message: message("Baked for 7s · done 9:02 PM", Some("Summary")),
        }),
    )]);
    let rows = harness.present();
    assert!(
        row_of(&rows, "✻ Baked for 7s · done 9:02 PM").is_some(),
        "{rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("Summary")),
        "no heading row: {rows:?}"
    );
}
