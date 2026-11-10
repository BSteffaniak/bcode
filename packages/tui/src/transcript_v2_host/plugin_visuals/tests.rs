//! Plugin visual acceptance (E1–E6, E3, C36) and shell screen input (C39) of the V2 host.

use std::time::{Duration, Instant};

use bcode_plugin_sdk::tui::{
    PluginTuiSelectionCell, PluginTuiSelectionRow, PluginTuiTranscriptHeader,
    PluginTuiVisualRenderMode,
};
use bcode_plugin_sdk::tui_visual::{TuiVisualAnchor, TuiVisualSourceRange};
use bcode_session_models::{SessionEvent, SessionEventKind, SessionId};
use bcode_tui_transcript::layout::producers::visual::{VisualRenderMode, VisualSourceRange};
use bmux_keyboard::parse_key_stroke;
use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::text::Line;

use super::{
    MAX_PLUGIN_VISUAL_ROWS, SHELL_SCROLL_COLUMNS, accepted_visual, shifted, uncovered_parts,
};
use crate::keymap::BmuxKeyMap;
use crate::plugin_tui::{RoutedTuiSection, RoutedTuiVisual};
use crate::transcript_v2_host::TranscriptV2Host;
use crate::transcript_v2_host::input::TranscriptInputOutcome;
use crate::transcript_v2_host::test_fixtures::{
    Painted, REGION, attached_with_events, event, paint,
};

fn routed(rows: usize) -> RoutedTuiVisual {
    RoutedTuiVisual {
        route: bcode_plugin::PluginVisualAdapterRoute {
            plugin_id: "example.shell".to_owned(),
            adapter_id: "card".to_owned(),
            schema: "example.card".to_owned(),
            service_interface_id: "example.card.visual".to_owned(),
            surfaces: Vec::new(),
            priority: 0,
            producer_default: true,
            render_mode: bcode_plugin::PluginVisualAdapterRenderMode::TranscriptBlock,
        },
        render_mode: PluginTuiVisualRenderMode::TranscriptBlock,
        rows: (0..rows)
            .map(|row| Line::raw(format!("row {row}")))
            .collect(),
        header: PluginTuiTranscriptHeader {
            title: Some("Card".to_owned()),
            timeout_ms: Some(10_000),
        },
        anchors: vec![TuiVisualAnchor {
            key: "body".to_owned(),
            row: 1,
            source: Some(TuiVisualSourceRange {
                identity: "line:1".to_owned(),
                start: 4,
                end: 9,
            }),
        }],
        selection: vec![
            (
                0,
                PluginTuiSelectionRow {
                    identity: "cmd".to_owned(),
                    byte_start: 0,
                    text: "ls".to_owned(),
                    cells: vec![PluginTuiSelectionCell {
                        column: 0,
                        width: 1,
                        bytes: 0..1,
                    }],
                    revision: 1,
                },
            ),
            (
                1,
                // Out-of-range cell bytes: rejected rather than mapped untruthfully.
                PluginTuiSelectionRow {
                    identity: "bad".to_owned(),
                    byte_start: 0,
                    text: "x".to_owned(),
                    cells: vec![PluginTuiSelectionCell {
                        column: 0,
                        width: 1,
                        bytes: 0..5,
                    }],
                    revision: 1,
                },
            ),
        ],
        summary: Some("summary".to_owned()),
        target: Some("~/work".to_owned()),
        sections: vec![RoutedTuiSection {
            name: "example.files".to_owned(),
            label: None,
            rows: (0..MAX_PLUGIN_VISUAL_ROWS + 5)
                .map(|row| Line::raw(format!("file {row}")))
                .collect(),
        }],
    }
}

/// Everything V1 presents is accepted, bounded and scoped to its producer: anchors, source
/// identities and selection identities become `<plugin>:<identity>` (the identity content
/// events and retained positions are routed by), invalid selection rows are dropped.
#[test]
fn accepted_visuals_keep_every_routed_field_bounded_and_producer_scoped() {
    let visual = accepted_visual(routed(MAX_PLUGIN_VISUAL_ROWS + 10), 7);
    assert_eq!(visual.adapter, "example.shell:card");
    assert_eq!(visual.revision, 7);
    assert_eq!(visual.rows.len(), MAX_PLUGIN_VISUAL_ROWS);
    assert_eq!(visual.mode, VisualRenderMode::TranscriptBlock);
    assert_eq!(visual.title.as_deref(), Some("Card"));
    assert_eq!(visual.timeout_ms, Some(10_000));
    assert_eq!(visual.target.as_deref(), Some("~/work"));
    assert_eq!(visual.summary.as_deref(), Some("summary"));
    assert_eq!(visual.sections.len(), 1);
    assert_eq!(visual.sections[0].rows.len(), MAX_PLUGIN_VISUAL_ROWS);
    let anchor = &visual.correspondence.anchors()[0];
    assert_eq!(anchor.key, "example.shell:body");
    assert_eq!(
        anchor.source,
        Some(VisualSourceRange {
            identity: "example.shell:line:1".to_owned(),
            start: 4,
            end: 9,
        })
    );
    assert_eq!(
        visual.correspondence.position(1),
        Some(("example.shell:line:1", 4))
    );
    assert_eq!(
        visual
            .correspondence
            .selection(0)
            .map(|row| row.identity.as_str()),
        Some("example.shell:cmd")
    );
    assert!(visual.correspondence.selection(1).is_none());
}

#[test]
fn shell_scroll_steps_clamp_to_the_widest_line() {
    assert_eq!(shifted(0, 1, 100), SHELL_SCROLL_COLUMNS);
    assert_eq!(shifted(96, 1, 100), 100);
    assert_eq!(shifted(4, -1, 100), 0);
    assert_eq!(shifted(0, 1, 0), 0);
}

// ---- shell screen input (C39) ---------------------------------------------------------------

const WIDE: &str = "WIDEHEAD----------------------------------------------------------WIDETAIL";

fn shell_events(session: SessionId) -> Vec<SessionEvent> {
    vec![
        event(
            session,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-wide".to_owned(),
                producer_plugin_id: None,
                tool_name: "bash".to_owned(),
                arguments_json: serde_json::json!({ "command": "echo wide" }).to_string(),
                working_directory: None,
            },
        ),
        event(
            session,
            2,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "call-wide".to_owned(),
                    model_output: WIDE.to_owned(),
                    is_error: false,
                    presentation: None,
                    result: None,
                    content: Vec::new(),
                },
            },
        ),
    ]
}

/// A host attached to a session holding one wide shell output.
fn shell_host(at: Instant) -> TranscriptV2Host {
    let session = SessionId::new();
    attached_with_events(session, shell_events(session), at)
}

fn keymap() -> BmuxKeyMap {
    BmuxKeyMap::from_config_for_engine(
        &bcode_config::TuiConfig::default(),
        bcode_config::TuiEngine::V2,
    )
}

fn frame(host: &mut TranscriptV2Host, at: Instant) -> Painted {
    paint(host, at, Duration::ZERO)
}

/// Terminal row showing `needle`.
fn row_of(painted: &Painted, needle: &str) -> u16 {
    (REGION.y..REGION.bottom())
        .find(|&y| painted.row(y).contains(needle))
        .unwrap_or_else(|| panic!("{needle} not painted"))
}

fn text(painted: &Painted) -> String {
    (REGION.y..REGION.bottom())
        .map(|y| painted.row(y))
        .collect::<Vec<_>>()
        .join("\n")
}

/// C39: horizontal wheel ticks over the shell screen scroll it on its own (the line is never
/// wrapped), and stop at the end of the widest line; elsewhere they are not the screen's.
#[test]
fn c39_horizontal_wheel_scrolls_the_shell_screen_and_stops_at_its_end() {
    let at = Instant::now();
    let mut host = shell_host(at);
    let painted = frame(&mut host, at);
    assert!(!text(&painted).contains("WIDETAIL"), "{}", text(&painted));
    let y = row_of(&painted, "WIDEHEAD");
    let wheel = |host: &mut TranscriptV2Host, kind| {
        host.handle_mouse(MouseEvent::new(kind, Point::new(10, y)), 3, None, at)
    };
    assert_eq!(
        wheel(&mut host, MouseEventKind::ScrollRight),
        TranscriptInputOutcome::Redraw
    );
    for _ in 0..20 {
        let _ = frame(&mut host, at);
        let _ = wheel(&mut host, MouseEventKind::ScrollRight);
    }
    let scrolled = frame(&mut host, at);
    assert!(text(&scrolled).contains("WIDETAIL"), "{}", text(&scrolled));
    assert!(!text(&scrolled).contains("WIDEHEAD"));
    assert_eq!(
        wheel(&mut host, MouseEventKind::ScrollRight),
        TranscriptInputOutcome::Consumed,
        "at the end of the line the wheel changes nothing"
    );
    // Outside the shell screen a horizontal tick is not the screen's.
    assert_eq!(
        host.handle_mouse(
            MouseEvent::new(MouseEventKind::ScrollLeft, Point::new(10, REGION.y)),
            3,
            None,
            at
        ),
        TranscriptInputOutcome::Ignored
    );
}

/// C39: a right-button press focuses the shell screen; the `visual` scope's Right and Left
/// scroll it, Home resets it, Escape releases it, and any other key releases it and is left to
/// the shell. Pointer focus never moves transcript keyboard focus.
#[test]
fn c39_right_click_focuses_the_shell_screen_for_left_right_home_and_escape() {
    let at = Instant::now();
    let mut host = shell_host(at);
    let painted = frame(&mut host, at);
    let y = row_of(&painted, "WIDEHEAD");
    let right_click = |host: &mut TranscriptV2Host| {
        host.handle_mouse(
            MouseEvent::new(MouseEventKind::Down(MouseButton::Right), Point::new(10, y)),
            3,
            None,
            at,
        )
    };
    assert_eq!(right_click(&mut host), TranscriptInputOutcome::Consumed);
    assert!(host.focused_shell_screen().is_some());
    assert!(
        !host.transcript_focus_active(),
        "FO9: pointer never moves focus"
    );
    let key = |host: &mut TranscriptV2Host, key: &str| {
        host.handle_key(&keymap(), parse_key_stroke(key).expect("key"), at)
    };
    for _ in 0..20 {
        assert_ne!(key(&mut host, "right"), TranscriptInputOutcome::Ignored);
        let _ = frame(&mut host, at);
    }
    assert!(text(&frame(&mut host, at)).contains("WIDETAIL"));
    assert_eq!(key(&mut host, "left"), TranscriptInputOutcome::Redraw);
    let _ = frame(&mut host, at);
    assert_eq!(key(&mut host, "home"), TranscriptInputOutcome::Redraw);
    let reset = frame(&mut host, at);
    assert!(text(&reset).contains("WIDEHEAD") && !text(&reset).contains("WIDETAIL"));
    assert_eq!(key(&mut host, "escape"), TranscriptInputOutcome::Consumed);
    assert!(host.focused_shell_screen().is_none());

    // Any other key releases the screen and goes on to normal routing.
    assert_eq!(right_click(&mut host), TranscriptInputOutcome::Consumed);
    assert!(host.focused_shell_screen().is_some());
    assert_eq!(key(&mut host, "x"), TranscriptInputOutcome::Ignored);
    assert!(host.focused_shell_screen().is_none());
}

// ---- host overlays (B7) ---------------------------------------------------------------------

#[test]
fn uncovered_parts_leave_out_every_overlay_cell_and_keep_the_rest() {
    let area = Rect::new(0, 0, 10, 4);
    assert_eq!(uncovered_parts(area, &[]), vec![area]);
    assert_eq!(uncovered_parts(area, &[Rect::new(20, 0, 5, 5)]), vec![area]);
    assert_eq!(
        uncovered_parts(area, &[Rect::new(0, 0, 10, 4)]),
        Vec::<Rect>::new()
    );
    let hole = Rect::new(3, 1, 4, 2);
    let parts = uncovered_parts(area, &[hole]);
    let covered = |point: Point| parts.iter().filter(|part| part.contains(point)).count();
    for y in 0..4 {
        for x in 0..10 {
            let point = Point::new(x, y);
            let expected = usize::from(!hole.contains(point));
            assert_eq!(covered(point), expected, "{point:?}");
        }
    }
}

/// Paint the transcript, then the inspector over it, and commit, as the chat frame does.
fn frame_with_inspector(host: &mut TranscriptV2Host, at: Instant) {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::paint::PaintCx;
    host.sync_geometry(REGION, at);
    let mut buffer = Buffer::empty(Rect::new(0, 0, REGION.width, REGION.bottom() + 6));
    {
        let mut frame = Frame::new(&mut buffer);
        frame.set_clock(Some(bmux_tui::clock::PresentationClock::new(at)));
        let mut cx = PaintCx::new(&mut frame);
        host.paint(
            REGION,
            &mut cx,
            &crate::transcript_v2_host::test_fixtures::style(Duration::ZERO),
            at,
        );
        host.paint_inspector(
            &mut cx,
            crate::render::TuiTheme::for_theme_id("terminal-native"),
            &bmux_tui_components::theme::ComponentTheme::default(),
        );
    }
    host.commit();
}

/// The shell item of [`shell_host`].
fn shell_item(host: &TranscriptV2Host) -> bcode_session_view_models::TranscriptViewItemId {
    host.engine()
        .session_view()
        .snapshot()
        .transcript
        .items
        .iter()
        .find(|item| {
            matches!(
                item.kind,
                bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { .. }
            )
        })
        .map(|item| item.id.clone())
        .expect("shell item")
}

/// B7: the open inspector hides the plugin visual rows beneath it, so they publish no pointer
/// target there; the rows it leaves visible keep theirs.
#[test]
fn plugin_visual_targets_leave_out_the_open_inspector() {
    let at = Instant::now();
    let mut host = shell_host(at);
    let item = shell_item(&host);
    // One content key over every adapter row: each row is a pointer target.
    let mut visual = routed(6);
    visual.sections.clear();
    visual.anchors = vec![TuiVisualAnchor {
        key: "screen".to_owned(),
        row: 0,
        source: None,
    }];
    host.render
        .accept_visual_for_test(item, accepted_visual(visual, 1));
    assert!(host.flush_render_config(at));
    let _ = frame(&mut host, at);
    let painted = frame(&mut host, at);
    let before = host.committed_visual_targets();
    assert!(
        !before.is_empty(),
        "visual rows publish pointer targets\n{}",
        text(&painted)
    );
    assert!(
        host.run_transcript_route("transcript.inspect", at)
            .is_some(),
        "the inspect route runs"
    );
    frame_with_inspector(&mut host, at);
    let panel = host.inspector_area().expect("inspector painted");
    let after = host.committed_visual_targets();
    assert!(
        after
            .iter()
            .all(|(area, _)| area.intersection(panel).is_empty()),
        "{after:?} under {panel:?}"
    );
    let visible = |point: Point| !panel.contains(point);
    for (area, identity) in &before {
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let point = Point::new(x, y);
                let published = after
                    .iter()
                    .any(|(part, id)| id == identity && part.contains(point));
                assert_eq!(published, visible(point), "{identity} at {point:?}");
            }
        }
    }
}

/// B7 (C39): the open inspector owns pointer input over a shell screen beneath it: neither a
/// horizontal wheel tick nor a right-button press there reaches the hidden screen.
#[test]
fn a_shell_screen_under_the_inspector_takes_no_pointer_input() {
    let at = Instant::now();
    let mut host = shell_host(at);
    let painted = frame(&mut host, at);
    let y = row_of(&painted, "WIDEHEAD");
    assert!(
        host.run_transcript_route("transcript.inspect", at)
            .is_some(),
        "the inspect route runs"
    );
    frame_with_inspector(&mut host, at);
    let panel = host.inspector_area().expect("inspector painted");
    assert!(
        panel.contains(Point::new(10, y)),
        "{panel:?} covers row {y}"
    );
    for kind in [
        MouseEventKind::ScrollRight,
        MouseEventKind::Down(MouseButton::Right),
    ] {
        let _ = host.handle_mouse(MouseEvent::new(kind, Point::new(10, y)), 3, None, at);
        assert!(host.focused_shell_screen().is_none(), "{kind:?}");
    }
    assert_eq!(
        host.render.shell_columns(&shell_item(&host)),
        0,
        "the hidden screen did not scroll"
    );
}

/// B7 (C39): the open Mermaid source view owns pointer input over a shell screen beneath it:
/// neither a horizontal wheel tick nor a right-button press there reaches the hidden screen.
#[test]
fn a_shell_screen_under_the_mermaid_source_view_takes_no_pointer_input() {
    let at = Instant::now();
    let session = SessionId::new();
    // The diagram first, the shell screen below it, inside the source view's inset.
    let mut events = vec![event(
        session,
        1,
        SessionEventKind::AssistantMessage {
            text: "```mermaid\ngraph TD\n  NODEA --> NODEB\n```".to_owned(),
        },
    )];
    events.extend(shell_events(session).into_iter().map(|mut shell| {
        shell.sequence += 1;
        shell.timestamp_ms += 1;
        shell
    }));
    let mut host = attached_with_events(session, events, at);
    let mut config = host.engine.render_config().clone();
    config.markdown.mermaid = true;
    let update = host.engine.set_render_config(config, at);
    host.absorb(update);
    let key = |host: &mut TranscriptV2Host, key: &str| {
        host.handle_key(&keymap(), parse_key_stroke(key).expect("key"), at)
    };
    let painted = frame(&mut host, at);
    let y = row_of(&painted, "WIDEHEAD");
    let _ = key(&mut host, "ctrl+tab");
    let _ = key(&mut host, "alt+enter");
    assert!(host.markdown_source_view().is_some(), "source view opened");
    let painted = frame(&mut host, at);
    let area = host.markdown_source_area().expect("source view painted");
    let point = Point::new(10, y);
    assert!(
        area.contains(point),
        "{area:?} covers row {y}\n{}",
        text(&painted)
    );
    for kind in [
        MouseEventKind::ScrollRight,
        MouseEventKind::Down(MouseButton::Right),
    ] {
        let _ = host.handle_mouse(MouseEvent::new(kind, point), 3, None, at);
        assert!(host.focused_shell_screen().is_none(), "{kind:?}");
    }
    assert_eq!(host.render.shell_columns(&shell_item(&host)), 0);
}
