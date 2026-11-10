//! Markdown contribution focus and activation: traversal over retained per-unit targets,
//! reveal through the focus reveal, click focus, `<details>` state as a unit-scoped render
//! input, footnote jumps with return targets and `#fragment` navigation.

use std::time::Duration;
use web_time::Instant;

use bcode_markdown_render::{MarkdownContributionKind, MarkdownDestination};
use bcode_session_models::{
    ClientId, SessionEvent, SessionEventKind, SessionId, TurnAdmissionMetadata,
};
use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;

use super::*;
use crate::focus::FocusStep;
use crate::layout::context::MarkdownOptions;
use crate::markdown::{MarkdownActivation, MarkdownTargetId};
use crate::navigation::InputSource;

const WIDTH: u64 = 60;
const HEIGHT: u64 = 12;

/// A session of one user message and one assistant message per entry of `assistant`.
fn session(assistant: &[String]) -> Vec<SessionEvent> {
    let id = SessionId::new();
    let mut events = Vec::new();
    for text in assistant {
        for kind in [
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "question".to_owned(),
                admission: TurnAdmissionMetadata::default(),
            },
            SessionEventKind::AssistantMessage { text: text.clone() },
        ] {
            let sequence = events.len() as u64 + 1;
            events.push(SessionEvent {
                schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence,
                timestamp_ms: sequence,
                session_id: id,
                provenance: None,
                kind,
            });
        }
    }
    events
}

struct Harness {
    engine: TranscriptEngine,
    origin: Instant,
    elapsed: Duration,
}

impl Harness {
    fn new(assistant: &[String]) -> Self {
        Self::with_render(assistant, RenderConfig::default())
    }

    fn with_render(assistant: &[String], render: RenderConfig) -> Self {
        let engine =
            TranscriptEngine::new(ViewportGeometry::new(WIDTH, HEIGHT), MotionConfig::INSTANT)
                .with_render_config(render);
        let mut harness = Self {
            engine,
            origin: Instant::now(),
            elapsed: Duration::ZERO,
        };
        let events = session(assistant);
        let at = harness.at();
        let update = harness.engine.attach(at);
        let HostRequest::Window(issued) = update.requests[0] else {
            panic!("attach requests a window");
        };
        let result = HistoryWindowResult {
            generation: issued.generation,
            target: issued.request,
            events: events.clone(),
            has_older: false,
            has_newer: false,
            first_available_sequence: Some(1),
            last_available_sequence: events.last().map(|event| event.sequence),
        };
        harness.engine.apply_window(&result, at).expect("attach");
        harness
    }

    fn at(&mut self) -> Instant {
        self.elapsed += Duration::from_millis(1);
        self.origin + self.elapsed
    }

    fn present(&mut self) -> (Vec<String>, FrameSnapshot) {
        let clock = FrameClock { now: self.at() };
        let snapshot = self.engine.prepare_frame(clock);
        let width = u16::try_from(WIDTH).expect("width");
        let height = u16::try_from(HEIGHT).expect("height");
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        let mut frame = Frame::new(&mut buffer);
        frame.set_clock(Some(clock.presentation()));
        self.engine
            .paint_frame(&snapshot, &mut PaintCx::new(&mut frame));
        let rows = (0..height)
            .map(|row| frame.buffer().row_symbols(row).unwrap_or_default())
            .collect();
        let _ = self.engine.commit(snapshot.id);
        (rows, snapshot)
    }

    fn step(&mut self, step: FocusStep) -> bool {
        let at = self.at();
        self.engine.markdown_focus_step(step, at).1
    }

    fn focused_kind(&self) -> Option<&MarkdownContributionKind> {
        let id = self.engine.markdown_focus()?;
        self.engine.markdown_target(id).map(|target| &target.kind)
    }

    fn focused_label(&self) -> Option<String> {
        match self.focused_kind()? {
            MarkdownContributionKind::Link { label, .. } => Some(label.clone()),
            _ => None,
        }
    }

    /// The painted text under the focused target's visible rectangles.
    fn focused_text(&mut self) -> String {
        let (rows, snapshot) = self.present();
        self.engine
            .markdown_focus_rects(&snapshot)
            .iter()
            .map(|rect| {
                let row = &rows[usize::try_from(rect.y).expect("row")];
                row.chars()
                    .skip(usize::try_from(rect.x).expect("x"))
                    .take(usize::try_from(rect.width).expect("width"))
                    .collect::<String>()
            })
            .collect()
    }
}

fn filler(lines: usize) -> String {
    (0..lines)
        .map(|line| format!("filler line {line}"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[test]
fn traversal_walks_targets_in_document_order_and_wraps() {
    let mut harness = Harness::new(&[
        "first [alpha](https://alpha.example) and [beta](https://beta.example)".to_owned(),
        "second [gamma](https://gamma.example)".to_owned(),
    ]);
    let _ = harness.present();
    assert!(harness.step(FocusStep::Next));
    assert_eq!(harness.focused_label().as_deref(), Some("alpha"));
    assert!(harness.step(FocusStep::Next));
    assert_eq!(harness.focused_label().as_deref(), Some("beta"));
    assert!(harness.step(FocusStep::Next));
    assert_eq!(harness.focused_label().as_deref(), Some("gamma"));
    assert!(harness.step(FocusStep::Next), "wraps to the first target");
    assert_eq!(harness.focused_label().as_deref(), Some("alpha"));
    assert!(
        harness.step(FocusStep::Previous),
        "wraps to the last target"
    );
    assert_eq!(harness.focused_label().as_deref(), Some("gamma"));
    // The highlight covers exactly the painted label (B3, B12).
    assert_eq!(harness.focused_text(), "gamma");
}

#[test]
fn inert_destinations_are_neither_targets_nor_hit_regions() {
    let mut harness = Harness::new(&["unsafe [nope](javascript:alert(1)) here".to_owned()]);
    let (_, snapshot) = harness.present();
    assert!(!harness.step(FocusStep::Next));
    assert!(harness.engine.markdown_focus().is_none());
    assert!(
        !snapshot
            .hit_regions
            .iter()
            .any(|region| matches!(&region.hit.action, ItemAction::Activate(_))),
        "an inert link registers no activation region"
    );
}

#[test]
fn previous_without_focus_starts_in_view_and_reveals_offscreen_targets() {
    let mut harness = Harness::new(&[format!(
        "[top](https://top.example)\n\n{}\n\n[bottom](https://bottom.example)",
        filler(30)
    )]);
    let (rows, _) = harness.present();
    assert!(
        rows.iter().any(|row| row.contains("bottom")),
        "following shows the end: {rows:?}"
    );
    assert!(!rows.iter().any(|row| row.contains("top")), "{rows:?}");
    assert!(harness.step(FocusStep::Previous));
    assert_eq!(harness.focused_label().as_deref(), Some("bottom"));
    // Moving on to a target above the viewport reveals it from the top (C32).
    assert!(harness.step(FocusStep::Previous));
    assert_eq!(harness.focused_label().as_deref(), Some("top"));
    assert_eq!(
        harness.focused_text(),
        "top",
        "the revealed target is painted"
    );
}

#[test]
fn clicking_a_link_focuses_it_without_moving_the_reader() {
    let mut harness = Harness::new(&["see [alpha](https://alpha.example) now".to_owned()]);
    let (rows, snapshot) = harness.present();
    let row = rows
        .iter()
        .position(|row| row.contains("alpha"))
        .expect("link painted");
    // ASCII rows: one cell per character.
    let column = rows[row][..rows[row].find("alpha").expect("label")]
        .chars()
        .count();
    let before = harness.engine.nav().clone();
    let at = harness.at();
    let (update, hit) = harness
        .engine
        .click(&snapshot, column as u64, row as u64, at);
    let hit = hit.expect("the link is hit");
    assert!(matches!(&hit.action, ItemAction::Activate(key) if key.starts_with("markdown:")));
    assert!(update.invalidated, "focus changed");
    assert_eq!(harness.engine.nav(), &before, "the reader did not move");
    let id = harness.engine.markdown_focus().cloned().expect("focused");
    let at = harness.at();
    let (_, activation) = harness.engine.activate_markdown(&id, at);
    assert_eq!(
        activation,
        MarkdownActivation::Link(MarkdownDestination::Web(
            "https://alpha.example".parse().expect("url")
        ))
    );
}

#[test]
fn details_state_is_a_unit_scoped_render_input_and_focus_survives_the_revision() {
    let source = "<details>\n<summary>More</summary>\n\nHIDDENBODY\n\n</details>".to_owned();
    let mut harness = Harness::new(&[source]);
    let (rows, _) = harness.present();
    assert!(!rows.iter().any(|row| row.contains("HIDDENBODY")));
    assert!(harness.step(FocusStep::Next));
    let id = harness
        .engine
        .markdown_focus()
        .cloned()
        .expect("details focused");
    let at = harness.at();
    let (_, activation) = harness.engine.activate_focused_markdown(at);
    let MarkdownActivation::Details { key, open } = activation else {
        panic!("details activation, got {activation:?}");
    };
    assert!(open, "a closed block opens");
    assert_eq!(key, id.contribution);
    // The host stores the state in the render configuration; only the owning unit changes.
    let mut config = harness.engine.render_config().clone();
    config.markdown.details_open.insert(key, open);
    let at = harness.at();
    let _ = harness.engine.set_render_config(config, at);
    let (rows, _) = harness.present();
    assert!(
        rows.iter().any(|row| row.contains("HIDDENBODY")),
        "{rows:?}"
    );
    assert_eq!(harness.engine.markdown_focus(), Some(&id), "focus survives");
    let at = harness.at();
    let (_, activation) = harness.engine.activate_markdown(&id, at);
    assert!(matches!(
        activation,
        MarkdownActivation::Details { open: false, .. }
    ));
}

#[test]
fn footnote_reference_jumps_to_its_definition_and_back() {
    let mut harness = Harness::new(&[format!(
        "Claim[^note] here.\n\n{}\n\n[^note]: FOOTNOTEBODY",
        filler(20)
    )]);
    let _ = harness.present();
    let at = harness.at();
    let _ = harness.engine.intent(NavIntent::JumpToSessionStart, at);
    let (rows, _) = harness.present();
    assert!(
        !rows.iter().any(|row| row.contains("FOOTNOTEBODY")),
        "{rows:?}"
    );
    assert!(harness.step(FocusStep::Next));
    let reference = harness.engine.markdown_focus().cloned().expect("reference");
    assert!(matches!(
        harness.focused_kind(),
        Some(MarkdownContributionKind::FootnoteReference { .. })
    ));
    let at = harness.at();
    let (_, activation) = harness.engine.activate_markdown(&reference, at);
    assert_eq!(
        activation,
        MarkdownActivation::FootnoteDefinition { focused: true }
    );
    let definition = harness
        .engine
        .markdown_focus()
        .cloned()
        .expect("definition");
    assert_ne!(definition, reference);
    let (rows, snapshot) = harness.present();
    assert!(
        rows.iter().any(|row| row.contains("FOOTNOTEBODY")),
        "the definition is revealed: {rows:?}"
    );
    assert_ne!(harness.engine.markdown_focus_rects(&snapshot), []);
    let at = harness.at();
    let (_, activation) = harness.engine.activate_markdown(&definition, at);
    assert_eq!(
        activation,
        MarkdownActivation::FootnoteReference { returned: true }
    );
    assert_eq!(harness.engine.markdown_focus(), Some(&reference));
    let (rows, _) = harness.present();
    assert!(rows.iter().any(|row| row.contains("Claim")), "{rows:?}");
}

#[test]
fn fragment_navigation_reveals_the_anchor_in_the_linking_unit_first() {
    let mut harness = Harness::new(&[
        "## Target\n\nOTHERUNIT".to_owned(),
        format!(
            "[jump](#target)\n\n{}\n\n## Target\n\nSAMEUNIT\n\n{}",
            filler(20),
            filler(3)
        ),
    ]);
    let _ = harness.present();
    let at = harness.at();
    let _ = harness.engine.intent(NavIntent::JumpToSessionStart, at);
    let (rows, _) = harness.present();
    assert!(!rows.iter().any(|row| row.contains("SAMEUNIT")), "{rows:?}");
    // The linking unit is the last unit with a fragment anchor.
    let link_unit = harness
        .engine
        .units()
        .keys()
        .iter()
        .rfind(|unit| !harness.engine.markdown_view().anchors(unit).is_empty())
        .cloned()
        .expect("the linking unit has anchors");
    let at = harness.at();
    let (_, found) = harness
        .engine
        .navigate_markdown_fragment(Some(&link_unit), "#target", at);
    assert!(found);
    let (rows, _) = harness.present();
    assert!(rows[0].contains("Target"), "the anchor is on top: {rows:?}");
    assert!(
        rows.iter().any(|row| row.contains("SAMEUNIT")),
        "the linking unit's section is revealed: {rows:?}"
    );
    let at = harness.at();
    assert!(
        !harness
            .engine
            .navigate_markdown_fragment(Some(&link_unit), "missing", at)
            .1
    );
}

#[test]
fn mermaid_activation_returns_its_source() {
    let render = RenderConfig {
        markdown: MarkdownOptions {
            mermaid: true,
            ..MarkdownOptions::default()
        },
        ..RenderConfig::default()
    };
    let mut harness =
        Harness::with_render(&["```mermaid\ngraph TD\n  A --> B\n```".to_owned()], render);
    let _ = harness.present();
    assert!(harness.step(FocusStep::Next));
    let at = harness.at();
    let (_, activation) = harness.engine.activate_focused_markdown(at);
    let MarkdownActivation::Mermaid { source } = activation else {
        panic!("mermaid activation, got {activation:?}");
    };
    assert!(source.contains("A --> B"), "{source:?}");
}

#[test]
fn only_resident_targets_are_focused_and_remeasurement_keeps_focus() {
    let mut harness = Harness::new(&[
        "one [alpha](https://alpha.example)".to_owned(),
        "two [beta](https://beta.example)".to_owned(),
    ]);
    let _ = harness.present();
    assert!(harness.step(FocusStep::Previous));
    let beta = harness.engine.markdown_focus().cloned().expect("focused");
    assert_eq!(harness.focused_label().as_deref(), Some("beta"));
    let at = harness.at();
    let mut config = harness.engine.render_config().clone();
    config.markdown.mermaid = true;
    let _ = harness.engine.set_render_config(config, at);
    assert_eq!(harness.engine.markdown_focus(), Some(&beta));
    let ghost = MarkdownTargetId::new(beta.unit.clone(), "transcript:ghost");
    assert!(!harness.engine.focus_markdown(&ghost));
    assert_eq!(harness.engine.markdown_focus(), Some(&beta));
    let at = harness.at();
    assert_eq!(
        harness.engine.activate_markdown(&ghost, at).1,
        MarkdownActivation::Unavailable
    );
}

#[test]
fn a_wheel_scroll_keeps_markdown_focus_on_its_resident_target() {
    let mut harness = Harness::new(&[format!("[top](https://top.example)\n\n{}", filler(30))]);
    let at = harness.at();
    let _ = harness.engine.intent(NavIntent::JumpToSessionStart, at);
    let _ = harness.present();
    assert!(harness.step(FocusStep::Next));
    let top = harness.engine.markdown_focus().cloned().expect("focused");
    let at = harness.at();
    let _ = harness.engine.intent(
        NavIntent::ScrollBy {
            rows: 20,
            source: InputSource::Wheel,
        },
        at,
    );
    let (_, snapshot) = harness.present();
    assert_eq!(harness.engine.markdown_focus(), Some(&top));
    assert_eq!(
        harness.engine.markdown_focus_rects(&snapshot),
        [],
        "no highlight for a target out of view"
    );
}

/// A relative local link resolves against the configured base directory (the session's
/// working directory, as in V1) and becomes a focusable local-path target; without a base it
/// stays unresolved and inert.
#[test]
fn relative_local_links_resolve_against_the_base_directory() {
    let source = ["read [the manifest](Cargo.toml) first".to_owned()];
    let mut inert = Harness::new(&source);
    let _ = inert.present();
    assert!(!inert.step(FocusStep::Next), "unresolved without a base");

    let base = std::env::current_dir().expect("working directory");
    let render = RenderConfig {
        markdown: MarkdownOptions {
            base_directory: Some(base),
            ..MarkdownOptions::default()
        },
        ..RenderConfig::default()
    };
    let mut harness = Harness::with_render(&source, render);
    let (_, snapshot) = harness.present();
    assert!(harness.step(FocusStep::Next));
    assert!(
        matches!(
            harness.focused_kind(),
            Some(MarkdownContributionKind::Link {
                destination: MarkdownDestination::LocalPath(path),
                ..
            }) if path.ends_with("Cargo.toml")
        ),
        "{:?}",
        harness.focused_kind()
    );
    assert!(
        snapshot
            .hit_regions
            .iter()
            .any(|region| matches!(&region.hit.action, ItemAction::Activate(_))),
        "the resolved link registers an activation region"
    );
}
