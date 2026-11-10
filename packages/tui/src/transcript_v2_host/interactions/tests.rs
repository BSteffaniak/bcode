//! Inline interaction placement in the V2 transcript (C37, D10): the reserved unit's rows
//! become the surface's area, from the frame the transcript painted.

use std::time::{Duration, Instant};

use bcode_session_models::{ClientId, SessionEvent, SessionEventKind, SessionId};
use bcode_session_view_models::TranscriptViewItemKind;
use bcode_tui_transcript::engine::HostRequest;
use bcode_tui_transcript::navigation::{InputSource, NavIntent};
use bcode_tui_transcript::window::HistoryWindowResult;

use crate::effects::{TuiEffect, TuiEffectResult};
use crate::transcript_v2_host::TranscriptV2Host;
use crate::transcript_v2_host::effects::HistoryLoaded;
use crate::transcript_v2_host::test_fixtures::{COMPOSER, REGION, event, paint};

/// A session of `turns` answered turns followed by a question tool waiting on an exchange.
fn waiting_session(session: SessionId, turns: u64) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    let mut sequence = 0;
    let mut next = || {
        sequence += 1;
        sequence
    };
    for turn in 0..turns {
        events.push(event(
            session,
            next(),
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: format!("question {turn}"),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ));
        events.push(event(
            session,
            next(),
            SessionEventKind::AssistantMessage {
                text: format!("answer {turn} line 0\\\nline 1\\\nline 2"),
            },
        ));
    }
    events.push(event(
        session,
        next(),
        SessionEventKind::ToolCallRequested {
            tool_call_id: "ask".to_owned(),
            producer_plugin_id: None,
            tool_name: "question".to_owned(),
            arguments_json: r#"{"question":"continue?"}"#.to_owned(),
            working_directory: None,
        },
    ));
    events.push(event(
        session,
        next(),
        SessionEventKind::ToolExchangeRequested {
            request: bcode_session_models::ToolExchangeRequest {
                invocation_id: "ask".to_owned(),
                exchange_id: "exchange".to_owned(),
                producer_id: "test.producer".to_owned(),
                schema: "test.question".to_owned(),
                schema_version: 1,
                payload: serde_json::json!({ "question": "continue?" }),
                response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
            },
        },
    ));
    events
}

/// A host attached to [`waiting_session`] through the real effect path.
fn attached_waiting(turns: u64, at: Instant) -> TranscriptV2Host {
    let session = SessionId::new();
    let mut host = TranscriptV2Host::new();
    host.sync_geometry(REGION, at);
    host.set_composer_area(COMPOSER);
    host.sync_session(Some(session), at);
    let effects = host.drain_effects();
    let [TuiEffect::LoadTranscriptV2History { load }] = effects.as_slice() else {
        panic!("one tail request");
    };
    let HostRequest::Window(issued) = load.request else {
        panic!("window request");
    };
    let events = waiting_session(session, turns);
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

fn interaction_id(host: &TranscriptV2Host) -> String {
    host.engine()
        .session_view()
        .snapshot()
        .transcript
        .items
        .iter()
        .find_map(|item| match &item.kind {
            TranscriptViewItemKind::Interaction { interaction } => {
                Some(interaction.interaction_id.clone())
            }
            _ => None,
        })
        .expect("interaction item")
}

#[test]
fn d10_the_surface_is_placed_over_the_rows_its_unit_reserves_in_the_painted_frame() {
    let at = Instant::now();
    let mut host = attached_waiting(1, at);
    let id = interaction_id(&host);
    let _ = paint(&mut host, at, Duration::ZERO);
    assert_eq!(host.inline_interaction_area(), None, "nothing reserved yet");

    host.sync_inline_interaction(Some((id.as_str(), 4)), at);
    let painted = paint(&mut host, at, Duration::ZERO);
    let area = host.inline_interaction_area().expect("placed");
    let viewport = host.regions().viewport.expect("viewport");
    assert_eq!(area.logical_height, 4);
    assert_eq!(area.visible_offset, 0);
    assert_eq!(area.destination.height, 4);
    assert_eq!(area.destination.x, viewport.x);
    assert_eq!(area.destination.width, viewport.width);
    assert!(area.destination.y >= viewport.y && area.destination.bottom() <= viewport.bottom());
    for y in area.destination.y..area.destination.bottom() {
        assert_eq!(
            painted.row(y).trim(),
            "",
            "the reserved rows are blank for the surface"
        );
    }

    // Dropping the reservation (resolution, docked placement) removes the placement.
    host.sync_inline_interaction(None, at);
    let _ = paint(&mut host, at, Duration::ZERO);
    assert_eq!(host.inline_interaction_area(), None);
}

#[test]
fn c37_a_clipped_reservation_reports_its_visible_offset_and_reveal_brings_it_back() {
    let at = Instant::now();
    let mut host = attached_waiting(6, at);
    let id = interaction_id(&host);
    // Taller than the viewport: following the bottom clips its first rows.
    host.sync_inline_interaction(Some((id.as_str(), 20)), at);
    let _ = paint(&mut host, at, Duration::ZERO);
    let viewport = host.regions().viewport.expect("viewport");
    let area = host.inline_interaction_area().expect("placed");
    assert_eq!(area.logical_height, 20);
    assert!(area.destination.height <= viewport.height);
    assert_eq!(
        u32::from(area.visible_offset) + u32::from(area.destination.height),
        20,
        "the bottom of the reservation is in view"
    );

    // Scrolled far away, the surface has no area (no stale hits, B7) ...
    let _ = host.engine.intent(
        NavIntent::ScrollBy {
            rows: -200,
            source: InputSource::Wheel,
        },
        at,
    );
    let _ = paint(&mut host, at, Duration::ZERO);
    assert_eq!(host.inline_interaction_area(), None);

    // ... and revealing it puts its top on the viewport top.
    assert!(host.reveal_inline_interaction(at));
    // Past any scroll motion.
    let later = at + Duration::from_secs(5);
    let _ = paint(&mut host, later, Duration::ZERO);
    let area = host.inline_interaction_area().expect("revealed");
    assert_eq!(area.visible_offset, 0);
    assert_eq!(area.destination.y, viewport.y);
}

/// F5: a reservation synced while a frame is drawn reports the change, and that frame's
/// regional damage is extended by the transcript region so the change is painted at once.
#[test]
fn f5_a_reservation_change_while_drawing_damages_the_transcript_region() {
    use bmux_tui::damage::{Damage, DamagePolicy};

    let at = Instant::now();
    let mut host = attached_waiting(1, at);
    let id = interaction_id(&host);
    let _ = paint(&mut host, at, Duration::ZERO);
    assert!(host.sync_inline_interaction(Some((id.as_str(), 4)), at));
    assert!(
        !host.sync_inline_interaction(Some((id.as_str(), 4)), at),
        "an unchanged reservation is not a change"
    );
    let region = host.regions().transcript.expect("transcript region");
    let area = bmux_tui::geometry::Rect::new(0, 0, region.right(), region.bottom() + 3);
    let composer = bmux_tui::geometry::Rect::new(0, region.bottom(), region.right(), 3);
    let policy = DamagePolicy {
        max_regions: 64,
        max_area_percent: 100,
    };
    let damage = host.with_transcript_damage(Damage::regions([composer], area, policy), area);
    assert!(!damage.is_full());
    assert!(
        damage
            .retained_regions()
            .iter()
            .any(|damaged| damaged.intersection(region) == region),
        "{damage:?}"
    );
    assert!(
        damage
            .retained_regions()
            .iter()
            .any(|damaged| damaged.intersection(composer) == composer),
        "{damage:?}"
    );
}
