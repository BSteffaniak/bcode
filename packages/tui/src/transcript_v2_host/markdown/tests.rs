//! V2 Markdown activation in the host: clicks and the `tui.markdown.*` keys open links through
//! the safe activation boundary, request session switches, navigate fragments, jump between
//! footnotes, toggle `<details>` and the Mermaid source view, with the host's notices.

use std::time::{Duration, Instant};

use bcode_session_models::{ClientId, SessionEventKind, SessionId};
use bcode_tui_transcript::engine::HostRequest;
use bcode_tui_transcript::layout::context::MarkdownOptions;
use bcode_tui_transcript::window::HistoryWindowResult;
use bmux_keyboard::parse_key_stroke;
use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::Point;

use super::MarkdownNotice;
use crate::effects::{TuiEffect, TuiEffectResult};
use crate::keymap::BmuxKeyMap;
use crate::transcript_v2_host::TranscriptV2Host;
use crate::transcript_v2_host::effects::HistoryLoaded;
use crate::transcript_v2_host::input::TranscriptInputOutcome;
use crate::transcript_v2_host::test_fixtures::{COMPOSER, Painted, REGION, event, paint};

const GLOW: Duration = Duration::from_millis(0);

fn keymap() -> BmuxKeyMap {
    BmuxKeyMap::from_config(&bcode_config::TuiConfig::default())
}

/// A host attached to a session with one question and one assistant message per entry of
/// `answers`, all resident.
fn attached(answers: &[String], at: Instant) -> TranscriptV2Host {
    let session = SessionId::new();
    let mut host = TranscriptV2Host::new();
    let update = host
        .engine
        .set_motion(bcode_tui_transcript::navigation::MotionConfig::INSTANT, at);
    host.absorb(update);
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
    let mut events = Vec::new();
    for answer in answers {
        let sequence = events.len() as u64 + 1;
        events.push(event(
            session,
            sequence,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "question".to_owned(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ));
        events.push(event(
            session,
            sequence + 1,
            SessionEventKind::AssistantMessage {
                text: answer.clone(),
            },
        ));
    }
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

fn press(host: &mut TranscriptV2Host, key: &str, at: Instant) -> TranscriptInputOutcome {
    host.handle_key(&keymap(), parse_key_stroke(key).expect("key"), at)
}

/// Click (press and release in place) at a terminal cell.
fn click(host: &mut TranscriptV2Host, point: Point, at: Instant) -> TranscriptInputOutcome {
    let down = host.handle_mouse(
        MouseEvent::new(MouseEventKind::Down(MouseButton::Left), point),
        3,
        None,
        at,
    );
    let up = host.handle_mouse(
        MouseEvent::new(MouseEventKind::Up(MouseButton::Left), point),
        3,
        None,
        at,
    );
    if up == TranscriptInputOutcome::Ignored {
        down
    } else {
        up
    }
}

/// Terminal cell where `needle` starts (ASCII rows).
fn find(painted: &Painted, needle: &str) -> Option<Point> {
    (REGION.y..REGION.bottom()).find_map(|y| {
        let row = painted.row(y);
        let byte = row.find(needle)?;
        Some(Point::new(
            u16::try_from(row[..byte].chars().count()).ok()?,
            y,
        ))
    })
}

fn shows(painted: &Painted, needle: &str) -> bool {
    find(painted, needle).is_some()
}

fn later(at: Instant, step: u64) -> Instant {
    at + Duration::from_millis(step)
}

#[test]
fn clicking_a_web_link_focuses_highlights_and_opens_it() {
    let at = Instant::now();
    let mut host = attached(
        &["see [alpha](https://alpha.example/docs) now".to_owned()],
        at,
    );
    let painted = paint(&mut host, at, GLOW);
    let link = find(&painted, "alpha").expect("link painted");
    assert_ne!(
        click(&mut host, link, later(at, 1)),
        TranscriptInputOutcome::Ignored
    );
    assert_eq!(host.markdown.opened, ["https://alpha.example/docs"]);
    assert_eq!(host.take_markdown_notice(), Some(MarkdownNotice::Opened));
    assert!(
        host.focus().transcript_target().is_none(),
        "keyboard focus stays (FO1)"
    );
    let painted = paint(&mut host, later(at, 2), GLOW);
    for offset in 0..5 {
        assert!(
            painted.highlighted(link.x + offset, link.y),
            "label cell {offset}"
        );
    }
    assert!(
        !painted.highlighted(link.x + 5, link.y),
        "only the label is highlighted"
    );
}

#[test]
fn an_opener_failure_reports_v1_s_message() {
    let at = Instant::now();
    let mut host = attached(&["[alpha](https://alpha.example)".to_owned()], at);
    host.markdown.open_failure = Some("no opener".to_owned());
    let painted = paint(&mut host, at, GLOW);
    let link = find(&painted, "alpha").expect("link");
    let _ = click(&mut host, link, later(at, 1));
    assert_eq!(
        host.take_markdown_notice(),
        Some(MarkdownNotice::OpenFailed("no opener".to_owned()))
    );
}

#[test]
fn markdown_keys_move_activate_and_copy_from_either_focus() {
    let at = Instant::now();
    let mut host = attached(
        &["[alpha](https://alpha.example) and [beta](https://beta.example)".to_owned()],
        at,
    );
    let _ = paint(&mut host, at, GLOW);
    assert_eq!(
        press(&mut host, "ctrl+tab", later(at, 1)),
        TranscriptInputOutcome::Redraw
    );
    assert_eq!(host.take_markdown_notice(), Some(MarkdownNotice::Focused));
    assert!(host.focus().transcript_target().is_none());
    assert_eq!(
        press(&mut host, "ctrl+alt+c", later(at, 2)),
        TranscriptInputOutcome::Redraw
    );
    assert_eq!(
        host.take_markdown_copy().as_deref(),
        Some("https://alpha.example/")
    );
    // With transcript focus the Markdown keys keep it (they are not transcript keys).
    assert_ne!(
        press(&mut host, "alt+u", later(at, 3)),
        TranscriptInputOutcome::Ignored
    );
    assert!(host.focus().transcript_target().is_some());
    let stroke = parse_key_stroke("ctrl+tab").expect("key");
    assert!(!host.release_focus_for_event(&keymap(), &bmux_tui::event::Event::Key(stroke)));
    assert_eq!(
        press(&mut host, "ctrl+tab", later(at, 4)),
        TranscriptInputOutcome::Redraw
    );
    assert!(
        host.focus().transcript_target().is_some(),
        "transcript focus kept"
    );
    assert_eq!(
        press(&mut host, "alt+enter", later(at, 5)),
        TranscriptInputOutcome::Redraw
    );
    assert_eq!(host.markdown.opened, ["https://beta.example/"]);
    // ctrl+shift+tab moves back.
    assert_eq!(
        press(&mut host, "ctrl+shift+tab", later(at, 6)),
        TranscriptInputOutcome::Redraw
    );
    assert_eq!(
        press(&mut host, "alt+enter", later(at, 7)),
        TranscriptInputOutcome::Redraw
    );
    assert_eq!(
        host.markdown.opened,
        ["https://beta.example/", "https://alpha.example/"]
    );
}

#[test]
fn markdown_keys_without_a_target_are_consumed_without_change() {
    let at = Instant::now();
    let mut host = attached(&["plain text only".to_owned()], at);
    let _ = paint(&mut host, at, GLOW);
    for key in ["ctrl+tab", "ctrl+shift+tab", "alt+enter", "ctrl+alt+c"] {
        assert_eq!(
            press(&mut host, key, later(at, 1)),
            TranscriptInputOutcome::Consumed,
            "{key} is the transcript's and never reaches the chat handler"
        );
    }
    assert_eq!(host.take_markdown_notice(), None);
    assert_eq!(host.take_markdown_copy(), None);
}

#[test]
fn session_links_request_a_session_switch() {
    let at = Instant::now();
    let target = SessionId::new();
    let mut host = attached(&[format!("open [child](#bcode-session-{target})")], at);
    let painted = paint(&mut host, at, GLOW);
    let link = find(&painted, "child").expect("session link");
    let _ = click(&mut host, link, later(at, 1));
    assert_eq!(host.take_session_link_request(), Some(target));
    assert_eq!(host.take_session_link_request(), None, "taken once");
    assert_eq!(
        host.markdown.opened,
        [] as [String; 0],
        "never reaches the opener"
    );
}

#[test]
fn fragment_links_navigate_the_transcript() {
    let at = Instant::now();
    let filler = (0..20)
        .map(|line| format!("filler {line}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut host = attached(
        &[format!(
            "[jump](#section)\n\n{filler}\n\n## Section\n\nSECTIONBODY"
        )],
        at,
    );
    let _ = paint(&mut host, at, GLOW);
    let _ = press(&mut host, "ctrl+home", later(at, 1));
    let painted = paint(&mut host, later(at, 2), GLOW);
    assert!(!shows(&painted, "SECTIONBODY"));
    let link = find(&painted, "jump").expect("fragment link");
    let _ = click(&mut host, link, later(at, 3));
    assert_eq!(
        host.take_markdown_notice(),
        Some(MarkdownNotice::Fragment { found: true })
    );
    let painted = paint(&mut host, later(at, 4), GLOW);
    assert!(shows(&painted, "SECTIONBODY"), "the section is revealed");
    assert_eq!(host.markdown.opened, [] as [String; 0]);
}

#[test]
fn missing_fragment_targets_report_v1_s_message() {
    let at = Instant::now();
    let mut host = attached(&["[jump](#nowhere)".to_owned()], at);
    let painted = paint(&mut host, at, GLOW);
    let link = find(&painted, "jump").expect("fragment link");
    let _ = click(&mut host, link, later(at, 1));
    assert_eq!(
        host.take_markdown_notice(),
        Some(MarkdownNotice::Fragment { found: false })
    );
}

#[test]
fn clicking_a_details_summary_toggles_its_unit_only() {
    let at = Instant::now();
    let mut host = attached(
        &[
            "<details>\n<summary>More</summary>\n\nHIDDENBODY\n\n</details>".to_owned(),
            "second answer".to_owned(),
        ],
        at,
    );
    let painted = paint(&mut host, at, GLOW);
    assert!(!shows(&painted, "HIDDENBODY"));
    // Bring the host's render configuration in (the shell syncs it every update).
    assert!(host.set_markdown_details("transcript:none:details".to_owned(), true));
    let _ = host.flush_render_config(at);
    let painted = paint(&mut host, at, GLOW);
    let summary = find(&painted, "More").expect("summary");
    let _ = host.take_layout_work();
    let _ = click(&mut host, summary, later(at, 1));
    assert_eq!(
        host.take_markdown_notice(),
        Some(MarkdownNotice::Details { open: true })
    );
    let work = host.take_layout_work();
    assert_eq!(
        work.measurements, 1,
        "only the owning unit is re-measured: {work:?}"
    );
    let painted = paint(&mut host, later(at, 2), GLOW);
    assert!(shows(&painted, "HIDDENBODY"));
    let summary = find(&painted, "More").expect("summary");
    let _ = click(&mut host, summary, later(at, 3));
    assert_eq!(
        host.take_markdown_notice(),
        Some(MarkdownNotice::Details { open: false })
    );
    let painted = paint(&mut host, later(at, 4), GLOW);
    assert!(!shows(&painted, "HIDDENBODY"));
}

#[test]
fn footnotes_jump_to_the_definition_and_back_by_keyboard() {
    let at = Instant::now();
    let filler = (0..20)
        .map(|line| format!("filler {line}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut host = attached(
        &[format!("Claim[^n] here.\n\n{filler}\n\n[^n]: NOTEBODY")],
        at,
    );
    let _ = paint(&mut host, at, GLOW);
    let _ = press(&mut host, "ctrl+home", later(at, 1));
    let painted = paint(&mut host, later(at, 2), GLOW);
    assert!(!shows(&painted, "NOTEBODY"));
    let _ = press(&mut host, "ctrl+tab", later(at, 3));
    let _ = press(&mut host, "alt+enter", later(at, 4));
    assert_eq!(
        host.take_markdown_notice(),
        Some(MarkdownNotice::FootnoteDefinition { focused: true })
    );
    let painted = paint(&mut host, later(at, 5), GLOW);
    assert!(shows(&painted, "NOTEBODY"));
    let _ = press(&mut host, "alt+enter", later(at, 6));
    assert_eq!(
        host.take_markdown_notice(),
        Some(MarkdownNotice::FootnoteReference { returned: true })
    );
    let painted = paint(&mut host, later(at, 7), GLOW);
    assert!(shows(&painted, "Claim"));
}

#[test]
fn mermaid_source_view_toggles_occludes_and_closes_out_of_view() {
    let at = Instant::now();
    let filler = (0..20)
        .map(|line| format!("filler {line}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut host = attached(
        &[format!(
            "```mermaid\ngraph TD\n  NODEA --> NODEB\n```\n\n{filler}"
        )],
        at,
    );
    let mut config = host.engine.render_config().clone();
    config.markdown = MarkdownOptions {
        mermaid: true,
        ..config.markdown
    };
    let update = host.engine.set_render_config(config, at);
    host.absorb(update);
    host.set_markdown_activate_label("alt+enter");
    let _ = paint(&mut host, at, GLOW);
    let _ = press(&mut host, "ctrl+home", later(at, 1));
    let _ = paint(&mut host, later(at, 2), GLOW);
    let _ = press(&mut host, "ctrl+tab", later(at, 3));
    let _ = press(&mut host, "alt+enter", later(at, 4));
    assert_eq!(
        host.take_markdown_notice(),
        Some(MarkdownNotice::SourceView { open: true })
    );
    assert!(
        host.markdown_source_view()
            .is_some_and(|source| source.contains("NODEA"))
    );
    let painted = paint(&mut host, later(at, 5), GLOW);
    assert!(shows(&painted, "Mermaid source · alt+enter closes"));
    assert!(shows(&painted, "NODEA --> NODEB"));
    let overlay = find(&painted, "NODEA").expect("source line");
    assert!(
        !painted
            .hits
            .iter()
            .any(|hit| hit.area.contains(overlay)
                && hit.id.as_str().starts_with("transcript-v2.item")),
        "no transcript region stays interactive under the source view"
    );
    assert_eq!(
        click(&mut host, overlay, later(at, 6)),
        TranscriptInputOutcome::Consumed,
        "a click on the source view does nothing"
    );
    assert!(host.markdown_source_view().is_some());
    // Activating again closes it.
    let _ = press(&mut host, "alt+enter", later(at, 7));
    assert_eq!(
        host.take_markdown_notice(),
        Some(MarkdownNotice::SourceView { open: false })
    );
    assert!(host.markdown_source_view().is_none());
    // Reopened, it closes once the diagram scrolls out of view.
    let _ = press(&mut host, "alt+enter", later(at, 8));
    assert!(host.markdown_source_view().is_some());
    let _ = press(&mut host, "ctrl+end", later(at, 9));
    let painted = paint(&mut host, later(at, 10), GLOW);
    assert!(host.markdown_source_view().is_none());
    assert!(!shows(&painted, "Mermaid source"));
}
