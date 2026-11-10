//! V2 transcript tooling through host input: the structure inspector (FO6 `i`, shift+Up/Down,
//! Escape), make-default (`m`), detail cycling (`d`, `alt+d`) and the bulk disclosure routes.

use std::time::{Duration, Instant};

use bcode_presentation_policy::models::{Disclosure, UnitKind};
use bcode_tui_transcript::units::structure::DisclosureTarget;
use bmux_keyboard::parse_key_stroke;
use bmux_tui::buffer::Buffer;
use bmux_tui::event::{MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::hit::HitRegion;
use bmux_tui::paint::PaintCx;

use super::ToolNotice;
use crate::inspector::INSPECTOR_HIT_ID;
use crate::keymap::BmuxKeyMap;
use crate::render::TuiTheme;
use crate::transcript_v2_host::TranscriptV2Host;
use crate::transcript_v2_host::input::{TranscriptInputOutcome, TranscriptRouteOutcome};
use crate::transcript_v2_host::test_fixtures::{COMPOSER, REGION, attached, paint, style};

fn keymap() -> BmuxKeyMap {
    BmuxKeyMap::from_config_for_engine(
        &bcode_config::TuiConfig::default(),
        bcode_config::TuiEngine::V2,
    )
}

fn press(host: &mut TranscriptV2Host, key: &str, at: Instant) -> TranscriptInputOutcome {
    host.handle_key(&keymap(), parse_key_stroke(key).expect("key"), at)
}

/// Paint the transcript and then the inspector, as the chat frame does.
fn paint_with_inspector(host: &mut TranscriptV2Host, at: Instant) -> (Buffer, Vec<HitRegion>) {
    host.sync_geometry(REGION, at);
    let area = Rect::new(0, 0, REGION.width, REGION.bottom() + COMPOSER.height + 1);
    let mut buffer = Buffer::empty(area);
    let hits = {
        let mut frame = Frame::new(&mut buffer);
        frame.set_clock(Some(bmux_tui::clock::PresentationClock::new(at)));
        let mut cx = PaintCx::new(&mut frame);
        host.paint(REGION, &mut cx, &style(Duration::ZERO), at);
        host.paint_inspector(
            &mut cx,
            TuiTheme::for_theme_id("terminal-native"),
            &bmux_tui_components::theme::ComponentTheme::default(),
        );
        frame.hits().regions().to_vec()
    };
    host.commit();
    (buffer, hits)
}

fn screen(buffer: &Buffer) -> String {
    (0..buffer.area().height)
        .map(|y| buffer.row_symbols(y).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Step focus upward until the focused disclosure is a tool unit.
fn focus_tool_unit(host: &mut TranscriptV2Host, at: Instant) -> DisclosureTarget {
    if !host.transcript_focus_active() {
        let _ = press(host, "alt+u", at);
    }
    for _ in 0..64 {
        if let Some(DisclosureTarget::Unit(id)) = host.engine().focus_state().disclosure()
            && host
                .engine()
                .structure()
                .item_facts(id)
                .is_some_and(|facts| facts.kind == Some(UnitKind::Tool))
        {
            return DisclosureTarget::Unit(id.clone());
        }
        let _ = press(host, "k", at);
    }
    panic!("no tool unit to focus");
}

#[test]
fn fo6_i_opens_the_inspector_on_the_focused_target_and_escape_closes_it_first() {
    let at = Instant::now();
    let mut host = attached(3, at);
    let _ = paint(&mut host, at, Duration::ZERO);
    let target = focus_tool_unit(&mut host, at);

    assert_eq!(press(&mut host, "i", at), TranscriptInputOutcome::Redraw);
    assert!(host.inspector_open());
    assert_eq!(host.take_notice(), Some(ToolNotice::InspectorOpened));
    let report = host.inspector_report().expect("report");
    assert_eq!(report.target, target);
    assert_eq!(report.facts.tool.as_deref(), Some("read"));

    let (buffer, hits) = paint_with_inspector(&mut host, at);
    let text = screen(&buffer);
    assert!(text.contains("Inspector"), "{text}");
    assert!(text.contains("Facts"), "{text}");
    let panel = hits
        .iter()
        .find(|region| region.id.as_str() == INSPECTOR_HIT_ID)
        .expect("inspector hit region");
    assert!(
        panel.area.intersection(REGION) == panel.area,
        "the panel stays inside the transcript: {:?}",
        panel.area
    );

    // Escape closes the inspector, then leaves transcript focus.
    assert_eq!(
        press(&mut host, "escape", at),
        TranscriptInputOutcome::Redraw
    );
    assert!(!host.inspector_open());
    assert!(host.transcript_focus_active());
    assert_eq!(host.take_notice(), Some(ToolNotice::InspectorClosed));
    let (buffer, hits) = paint_with_inspector(&mut host, at);
    assert!(!screen(&buffer).contains("Facts"));
    assert!(
        hits.iter()
            .all(|region| region.id.as_str() != INSPECTOR_HIT_ID)
    );
    let _ = press(&mut host, "escape", at);
    assert!(!host.transcript_focus_active());

    // Leaving focus with alt+u also drops an open inspector.
    let _ = focus_tool_unit(&mut host, at);
    let _ = press(&mut host, "i", at);
    assert!(host.inspector_open());
    let _ = press(&mut host, "alt+u", at);
    assert!(!host.inspector_open());
}

#[test]
fn fo6_the_inspector_follows_focus_and_scrolls_with_shift_keys_and_the_wheel_over_it() {
    let at = Instant::now();
    let mut host = attached(4, at);
    let _ = paint(&mut host, at, Duration::ZERO);
    let first = focus_tool_unit(&mut host, at);
    let _ = press(&mut host, "i", at);
    let _ = paint_with_inspector(&mut host, at);

    // shift+Down scrolls the body; the transcript does not move.
    let nav = host.engine().nav().clone();
    assert_eq!(
        press(&mut host, "shift+down", at),
        TranscriptInputOutcome::Redraw
    );
    let _ = paint_with_inspector(&mut host, at);
    assert_eq!(
        press(&mut host, "shift+up", at),
        TranscriptInputOutcome::Redraw
    );
    assert_eq!(host.engine().nav(), &nav);

    // The wheel over the panel scrolls the inspector, never the transcript (B7).
    let (_, hits) = paint_with_inspector(&mut host, at);
    let panel = hits
        .iter()
        .find(|region| region.id.as_str() == INSPECTOR_HIT_ID)
        .expect("panel")
        .area;
    let wheel = MouseEvent::new(MouseEventKind::ScrollUp, Point::new(panel.x, panel.y));
    let _ = host.handle_mouse(wheel, 3, Some(INSPECTOR_HIT_ID), at);
    assert_eq!(
        host.engine().nav(),
        &nav,
        "a wheel over the inspector does not scroll the transcript"
    );

    // Moving focus moves the inspected target.
    let _ = press(&mut host, "k", at);
    let next = host.inspector_report().map(|report| report.target);
    assert_ne!(next.as_ref(), Some(&first));
    assert_eq!(next.as_ref(), host.engine().focus_state().disclosure());
}

#[test]
fn fo6_m_queues_the_focused_tools_current_disclosure_as_its_default() {
    let at = Instant::now();
    let mut host = attached(2, at);
    let _ = paint(&mut host, at, Duration::ZERO);
    let target = focus_tool_unit(&mut host, at);
    let current = host
        .engine()
        .structure()
        .effective_disclosure(&target)
        .expect("disclosure");
    assert_eq!(press(&mut host, "m", at), TranscriptInputOutcome::Redraw);
    let rules = host.take_default_rules();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].matcher.kind.0, vec![UnitKind::Tool]);
    assert_eq!(rules[0].matcher.tool.0, vec!["read".to_owned()]);
    assert_eq!(rules[0].set.disclosure, Some(current));
    assert!(host.take_default_rules().is_empty(), "taken once");
}

#[test]
fn fo6_d_cycles_the_focused_target_and_alt_d_cycles_every_unit() {
    let at = Instant::now();
    let mut host = attached(2, at);
    let _ = paint(&mut host, at, Duration::ZERO);

    // Outside transcript focus `alt+d` cycles every unit together.
    assert_eq!(
        press(&mut host, "alt+d", at),
        TranscriptInputOutcome::Redraw
    );
    assert!(matches!(
        host.take_notice(),
        Some(ToolNotice::Disclosure { focused: false, .. })
    ));

    let target = focus_tool_unit(&mut host, at);
    let before = host.engine().structure().effective_disclosure(&target);
    assert_eq!(press(&mut host, "d", at), TranscriptInputOutcome::Redraw);
    let after = host.engine().structure().effective_disclosure(&target);
    assert_ne!(before, after, "the focused target cycled");
}

#[test]
fn bulk_disclosure_routes_run_on_the_v2_transcript() {
    let at = Instant::now();
    let mut host = attached(3, at);
    let _ = paint(&mut host, at, Duration::ZERO);
    let tools = |host: &TranscriptV2Host| {
        host.engine()
            .session_view()
            .snapshot()
            .transcript
            .items
            .iter()
            .filter(|item| {
                host.engine()
                    .structure()
                    .item_facts(&item.id)
                    .is_some_and(|facts| facts.kind == Some(UnitKind::Tool))
            })
            .map(|item| {
                host.engine()
                    .structure()
                    .effective_disclosure(&DisclosureTarget::Unit(item.id.clone()))
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        host.run_transcript_route("tools.collapseAll", at),
        Some(TranscriptRouteOutcome::Ran(true))
    );
    assert!(
        tools(&host)
            .iter()
            .all(|disclosure| *disclosure == Some(Disclosure::Collapsed))
    );
    assert_eq!(
        host.run_transcript_route("tools.expandAll", at),
        Some(TranscriptRouteOutcome::Ran(true))
    );
    assert!(
        tools(&host)
            .iter()
            .all(|disclosure| *disclosure == Some(Disclosure::Expanded))
    );
    assert_eq!(
        host.run_transcript_route("tools.resetDisclosure", at),
        Some(TranscriptRouteOutcome::Ran(true))
    );
    assert_eq!(
        host.take_notice(),
        Some(ToolNotice::Reset { focused: false })
    );
    assert_eq!(host.run_transcript_route("session.switch", at), None);

    // The palette's inspect route enters focus and opens the inspector.
    assert_eq!(
        host.run_transcript_route("transcript.inspect", at),
        Some(TranscriptRouteOutcome::Ran(true))
    );
    assert!(host.transcript_focus_active() && host.inspector_open());
}

/// F3: the inspector follows focus in the update path. A focus move resets the panel's
/// scroll before any frame is painted, and painting only shows the prepared inspection.
#[test]
fn f3_a_focus_move_resets_the_inspector_scroll_without_a_paint() {
    let at = Instant::now();
    let mut host = attached(4, at);
    let _ = paint(&mut host, at, Duration::ZERO);
    let first = focus_tool_unit(&mut host, at);
    let _ = press(&mut host, "i", at);
    let _ = paint_with_inspector(&mut host, at);
    let _ = press(&mut host, "shift+down", at);
    assert_ne!(host.inspector_scroll_offset(), Some(0));

    let _ = press(&mut host, "k", at);
    assert_ne!(
        host.engine().focus_state().disclosure(),
        Some(&first),
        "focus moved"
    );
    assert_eq!(
        host.inspector_scroll_offset(),
        Some(0),
        "the scroll follows the focus move without a paint"
    );
    assert_eq!(
        host.inspector_report().map(|report| report.target).as_ref(),
        host.engine().focus_state().disclosure()
    );
}
