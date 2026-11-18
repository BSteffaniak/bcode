//! A guide written by the review assistant: asking for a walkthrough, the outline appearing once
//! the guide is started and its sections filling in as each write lands, the live guide card in
//! the conversation and its clicks driving the diff, `guide_show` and `current_view`, the
//! assistant's Guide tab where the screen has no room for the guide panel, and stopping a turn
//! keeping the sections already written.
//!
//! The assistant's turn is scripted the way the fake provider replays one: `plan`, then `start`
//! with an outline, then one write per section, each run through the review tools against the
//! store the app reads, and delivered to the panel as the session's transcript.

use super::assistant::{Setup, setup, snapshot, tool, user};
use crate::assistant::actions;
use crate::host::ClientToolReply;
use bcode_code_review::assistant::guide::{DeclaredContext, GuideStart};
use bcode_code_review::assistant::{ReviewRef, ReviewTools};
use bcode_code_review::document::ItemKey;
use bcode_code_review::guide::config::GuideConfig;
use bcode_code_review::guide::write::{GuideOp, OutlineInput};
use bcode_code_review_models::{GetGuideVersionRequest, GuideStatus, ReviewLineSide, SectionState};
use bcode_session_view_models::SessionViewSnapshot;
use bmux_tui::geometry::Point;

/// The planned outline: purpose, scope, and a reading order of one step.
fn outline() -> serde_json::Value {
    serde_json::json!([
        {"id": "s1", "kind": "purpose", "title": "Purpose"},
        {"id": "s2", "kind": "scope", "title": "Scope"},
        {"id": "s3", "kind": "reading_order", "title": "Reading order", "children": [
            {"id": "s3.1", "kind": "step", "title": "First change"}
        ]}
    ])
}

/// A new-side line the review shows: its path and number.
fn shown_line(setup: &Setup) -> (String, u32) {
    let review = setup.app().review().expect("review");
    let doc = review.document().expect("document");
    (0..doc.len())
        .filter_map(|index| doc.item(index))
        .find_map(|item| match item.key() {
            ItemKey::Line {
                file,
                line,
                side: ReviewLineSide::New,
            } => Some((file.path.clone(), line)),
            ItemKey::LinePair {
                file,
                new: Some(new),
                ..
            } => Some((file.path.clone(), new)),
            _ => None,
        })
        .expect("a new-side line")
}

/// The scripted turn: what `session` has run so far, for the panel.
struct Turn {
    session: String,
    items: Vec<bcode_session_view_models::TranscriptViewItem>,
    guide_id: Option<String>,
    /// Which turn of the conversation it is: tool call ids are unique within a session.
    round: usize,
}

impl Turn {
    fn new(session: &str, question: &str) -> Self {
        Self {
            session: session.to_owned(),
            items: vec![user(question)],
            guide_id: None,
            round: 1,
        }
    }

    /// The transcript so far, with the turn still running.
    fn running(&self) -> SessionViewSnapshot {
        let mut snapshot = snapshot(self.items.clone());
        snapshot.runtime.active_turn_id = Some("turn-1".to_owned());
        snapshot
    }

    /// `guide.plan`, then `guide.start` with the outline: run and delivered.
    fn plan_and_start(&mut self, setup: &mut Setup) {
        let services = setup.backend.services();
        let data = bcode_code_review::ReviewData::default();
        let mut tools = ReviewTools::new(&data, services.store());
        let opened = tools
            .open(
                &setup.review.repo_root,
                &ReviewRef::Session(self.session.clone()),
            )
            .expect("bound review");
        let planned: Vec<OutlineInput> = serde_json::from_value(outline()).expect("outline");
        let started = tools
            .guide_start(
                &opened,
                Some(&self.session),
                &GuideStart {
                    title: Some("Retry walkthrough".to_owned()),
                    directions: Some("focus on the retries".to_owned()),
                    outline: planned,
                    ..GuideStart::default()
                },
                &GuideConfig::default(),
                &DeclaredContext::default(),
            )
            .expect("started");
        self.guide_id = started["guide_id"].as_str().map(str::to_owned);
        self.items.push(tool(
            &format!("plan-{}", self.round),
            "code_review.guide.plan",
            r#"{"strategy":"auto"}"#,
        ));
        self.items.push(tool(
            &format!("start-{}", self.round),
            "code_review.guide.start",
            &serde_json::json!({"title": "Retry walkthrough", "outline": outline()}).to_string(),
        ));
        let running = self.running();
        setup.deliver(&self.session, running);
    }

    /// One `guide.write` of `ops`: run and delivered.
    fn write(&mut self, setup: &mut Setup, id: &str, ops: &serde_json::Value) {
        let guide_id = self.guide_id.clone().expect("started");
        let services = setup.backend.services();
        let data = bcode_code_review::ReviewData::default();
        let mut tools = ReviewTools::new(&data, services.store());
        let opened = tools
            .open(
                &setup.review.repo_root,
                &ReviewRef::Session(self.session.clone()),
            )
            .expect("bound review");
        let parsed: Vec<GuideOp> = serde_json::from_value(ops.clone()).expect("ops");
        let outcome = tools
            .guide_write(
                &opened,
                Some(&self.session),
                &guide_id,
                None,
                None,
                false,
                parsed,
                &GuideConfig::default(),
            )
            .expect("write ran");
        assert!(
            matches!(
                outcome,
                bcode_code_review::assistant::guide::GuideWriteOutcome::Written(_)
            ),
            "{outcome:?}"
        );
        self.items.push(tool(
            id,
            "code_review.guide.write",
            &serde_json::json!({"guide_id": guide_id, "ops": ops}).to_string(),
        ));
        let running = self.running();
        setup.deliver(&self.session, running);
    }
}

/// Settle until `done` holds (the guide reads run in the background).
fn settle_until(setup: &mut Setup, what: &str, done: impl Fn(&Setup) -> bool) {
    for _ in 0..12 {
        if done(setup) {
            return;
        }
        setup.settle();
    }
    let screen = setup.settle();
    assert!(done(setup), "{what}:\n{}", screen.text());
}

/// Sections written in the guide the review shows.
fn written(setup: &Setup) -> usize {
    setup
        .app()
        .review()
        .and_then(|review| review.guide())
        .map_or(0, crate::guide::ActiveGuide::written)
}

/// Where `text` shows inside the assistant panel (right of the screen when docked).
fn in_panel(screen: &crate::testing::harness::Screen, text: &str, from_x: u16) -> Point {
    (0..screen.height())
        .find_map(|y| {
            screen
                .row_text(y)
                .match_indices(text)
                .map(|(byte, _)| {
                    let before = &screen.row_text(y)[..byte];
                    u16::try_from(bmux_tui::text_width::display_width(before)).unwrap_or(0)
                })
                .find(|x| *x >= from_x)
                .map(|x| Point { x, y })
        })
        .unwrap_or_else(|| panic!("no {text:?} in the panel:\n{}", screen.text()))
}

/// The cursor's file path and new-side line.
fn cursor_line(setup: &Setup) -> Option<(String, u32)> {
    match setup.app().review()?.cursor()? {
        ItemKey::Line { file, line, .. } => Some((file.path.clone(), *line)),
        ItemKey::LinePair { file, new, .. } => Some((file.path.clone(), (*new)?)),
        _ => None,
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn walking_through_a_review_fills_the_guide_live_and_its_card_drives_the_diff() {
    let mut setup = setup(170, 50, true);
    // The empty guide panel's `enter` asks the assistant for a walkthrough.
    setup.scenario.key("alt+g");
    setup.settle();
    setup.scenario.key("enter");
    setup.settle();
    let (session, request) = setup
        .backend
        .host()
        .turns()
        .last()
        .cloned()
        .expect("the walkthrough was sent");
    assert_eq!(request.text, crate::surface::WALKTHROUGH);
    assert!(setup.app().assistant().is_focused());

    let mut turn = Turn::new(&session, crate::surface::WALKTHROUGH);
    turn.plan_and_start(&mut setup);
    settle_until(&mut setup, "the outline appears once started", |setup| {
        setup
            .app()
            .review()
            .and_then(|review| review.guide())
            .is_some()
    });
    let screen = setup.settle();
    assert!(
        screen.contains("Guide · v1 · Retry walk"),
        "{}",
        screen.text()
    );
    assert!(
        screen.contains("writing 0/4"),
        "the card: {}",
        screen.text()
    );
    assert!(screen.contains("First change"), "{}", screen.text());
    assert!(
        !screen.contains("code_review.guide.start"),
        "the card stands in for the guide tool lines: {}",
        screen.text()
    );

    // Sections fill in one write at a time.
    let (path, line) = shown_line(&setup);
    turn.write(
        &mut setup,
        "write-1",
        &serde_json::json!([{"op": "put_section", "id": "s1",
            "body": "Retries back off before giving up."}]),
    );
    settle_until(&mut setup, "the purpose is written", |setup| {
        written(setup) == 1
    });
    assert!(setup.settle().contains("writing 1/4"));
    turn.write(
        &mut setup,
        "write-2",
        &serde_json::json!([{"op": "put_section", "id": "s3.1",
            "body": "The first change [[1]].",
            "citations": [{"path": path, "side": "new", "start_line": line, "end_line": line}]}]),
    );
    settle_until(&mut setup, "the step is written", |setup| {
        written(setup) == 2
    });
    let screen = setup.settle();
    assert!(screen.contains("writing 2/4"), "{}", screen.text());

    // A click on the card's step shows its citation in the diff.
    let panel_x = (0..170)
        .find(|x| setup.app().assistant().contains(Point { x: *x, y: 10 }))
        .expect("the panel is docked on the right");
    let at = in_panel(&screen, "3.1 First change", panel_x);
    setup.scenario.click(at.x, at.y);
    setup.settle();
    assert_eq!(cursor_line(&setup), Some((path, line)));

    // `current_view` reports the guide, and `guide_show` moves through it.
    let view = setup.app().view_context();
    let guide = view
        .review
        .and_then(|review| review.guide)
        .expect("the guide in the view");
    assert_eq!((guide.written, guide.sections), (2, 4));
    assert_eq!(guide.status, "generating");
    let mut reply =
        setup
            .backend
            .host()
            .call_client_tool(&session, actions::GUIDE_SHOW, r#"{"section":"s1"}"#);
    let answer = (0..20)
        .find_map(|_| {
            setup.settle();
            reply.try_recv().ok()
        })
        .expect("guide_show answered");
    let ClientToolReply::Completed(text) = answer else {
        panic!("{answer:?}");
    };
    assert!(text.contains("showing §1 Purpose"), "{text}");

    // Stopping the turn keeps what was written and marks the version stopped.
    assert!(
        setup.app().assistant().is_focused(),
        "the card click focused the panel"
    );
    setup.scenario.key("ctrl+c");
    setup.settle();
    assert_eq!(setup.backend.host().cancelled_turns(), vec![session]);
    let guide_id = turn.guide_id.clone().expect("guide");
    settle_until(&mut setup, "the version is marked stopped", |setup| {
        setup
            .backend
            .call_now(GetGuideVersionRequest {
                repo_path: setup.review.repo_root.clone(),
                guide_id: guide_id.clone(),
                version: None,
            })
            .version
            .is_some_and(|record| record.version.status == GuideStatus::Cancelled)
    });
    let stored = setup
        .backend
        .call_now(GetGuideVersionRequest {
            repo_path: setup.review.repo_root.clone(),
            guide_id,
            version: None,
        })
        .version
        .expect("version");
    for id in ["s1", "s3.1"] {
        assert_eq!(
            stored
                .version
                .section(id)
                .map(|section| section.state.clone()),
            Some(SectionState::Written),
            "{id} stays written"
        );
    }
    settle_until(&mut setup, "the card says stopped", |setup| {
        setup
            .app()
            .review()
            .and_then(|review| review.guide())
            .is_some_and(|active| active.version.status == GuideStatus::Cancelled)
    });
    let screen = setup.settle();
    assert!(
        screen.contains("stopped · 2/4 written"),
        "{}",
        screen.text()
    );
}

#[test]
fn the_guide_is_a_tab_of_the_assistant_where_the_screen_has_no_room_for_its_panel() {
    // Narrower than the right dock: the assistant docks under the screen, leaving the review
    // too few rows for the guide panel.
    let mut setup = setup(80, 30, true);
    setup.scenario.key("ctrl+k");
    let session = setup.send("Walk me through this review.");
    let mut turn = Turn::new(&session, "Walk me through this review.");
    turn.plan_and_start(&mut setup);
    let (path, line) = shown_line(&setup);
    turn.write(
        &mut setup,
        "write-1",
        &serde_json::json!([{"op": "put_section", "id": "s3.1",
            "body": "The first change [[1]].",
            "citations": [{"path": path, "side": "new", "start_line": line, "end_line": line}]}]),
    );
    settle_until(&mut setup, "the step is written", |setup| {
        written(setup) == 1
    });
    let screen = setup.settle();
    assert!(setup.app().assistant().guide_tabs(), "{}", screen.text());
    // Docked under a screen too short for the guide panel: the tabs are on its title row.
    let chat = in_panel(&screen, " Chat ", 0);
    let title = in_panel(&screen, "Assistant", 0);
    assert_eq!(
        chat.y,
        title.y,
        "the tabs are on the title row: {}",
        screen.text()
    );

    // The Guide tab shows the guide where the conversation was; its hit regions are inside it.
    let tab = in_panel(&screen, " Guide ", chat.x);
    setup.scenario.click(tab.x + 1, tab.y);
    let screen = setup.settle();
    assert!(setup.app().assistant().guide_tab_shown());
    let area = setup
        .app()
        .assistant()
        .guide_area()
        .expect("the guide's place");
    assert!(
        screen.text_in(area, area.y).contains("Guide · v1"),
        "{}",
        screen.text()
    );
    assert!(screen.contains("Reading order"), "{}", screen.text());
    let hits: Vec<_> = screen
        .hits()
        .iter()
        .filter(|region| {
            region
                .id
                .as_str()
                .contains(crate::surface::GUIDE_TAB_VIEW_ID)
        })
        .collect();
    assert!(!hits.is_empty(), "{}", screen.text());
    for region in hits {
        assert!(
            area.contains(Point {
                x: region.area.x,
                y: region.area.y
            }) && region.area.bottom() <= area.bottom(),
            "{region:?} outside {area:?}"
        );
    }

    // Guide keys work on the tab: alt+down moves the diff to the step's citation.
    setup
        .scenario
        .keys(&["alt+down", "alt+down", "alt+down", "alt+down"]);
    setup.settle();
    assert_eq!(cursor_line(&setup), Some((path, line)));
    // alt+g goes back to the conversation.
    setup.scenario.key("alt+g");
    setup.settle();
    assert!(!setup.app().assistant().guide_tab_shown());

    // Tiny sizes keep the panel within its cells and drop the tabs with the title actions.
    for (width, height) in [(20, 8), (11, 4), (1, 1)] {
        setup.scenario.resize(width, height);
        let screen = setup.settle();
        assert!(
            !screen.contains(" Guide "),
            "{width}x{height}: {}",
            screen.text()
        );
    }
    // Wide enough to dock the guide panel: no tabs.
    setup.scenario.resize(200, 50);
    setup.settle();
    assert!(!setup.app().assistant().guide_tabs());
}

#[test]
fn guide_tool_lines_stay_plain_where_no_guide_is_on_screen() {
    let mut setup = setup(170, 40, false);
    setup.scenario.key("ctrl+k");
    let session = setup.send("Walk me through the retry review.");
    let screen = setup.deliver(
        &session,
        snapshot(vec![
            user("Walk me through the retry review."),
            tool(
                "start-1",
                "code_review.guide.start",
                r#"{"review":"assistant-review","title":"Retry"}"#,
            ),
            super::assistant::answer("Started the guide."),
        ]),
    );
    assert!(
        screen.contains("code_review.guide.start"),
        "{}",
        screen.text()
    );
    assert!(!screen.contains("Guide v"), "no card: {}", screen.text());
}

/// Store a guide of `scope` whose current version `session` is writing.
fn writing_guide(
    setup: &Setup,
    guide_id: &str,
    scope: bcode_code_review_models::GuideScope,
    session: &str,
) {
    let notes = crate::notes::NoteScope::for_workspace(&setup.review);
    let _ = setup
        .backend
        .call_now(bcode_code_review_models::CreateReviewGuideRequest {
            repo_path: notes.repo_path,
            target: notes.target,
            scope: notes.scope,
            guide: bcode_code_review_models::ReviewGuide {
                guide_id: guide_id.to_owned(),
                review_key: String::new(),
                scope,
                parent_guide_id: None,
                title: guide_id.to_owned(),
                current_version: 1,
                created_at_ms: 1,
                updated_at_ms: 1,
                archived_at_ms: None,
            },
            version: bcode_code_review_models::GuideVersion {
                guide_id: guide_id.to_owned(),
                version: 1,
                schema_version: bcode_code_review_models::REVIEW_GUIDE_SCHEMA_VERSION,
                heads: std::collections::BTreeMap::new(),
                based_on: None,
                origin: bcode_code_review_models::GuideOrigin {
                    session_id: Some(session.to_owned()),
                    ..bcode_code_review_models::GuideOrigin::default()
                },
                status: GuideStatus::Generating,
                generation: None,
                outline: Vec::new(),
                citations: std::collections::BTreeMap::new(),
                retired_sections: Vec::new(),
                created_at_ms: 1,
                updated_at_ms: 1,
            },
        });
}

fn status_of(setup: &Setup, guide_id: &str) -> Option<GuideStatus> {
    setup
        .backend
        .call_now(GetGuideVersionRequest {
            repo_path: setup.review.repo_root.clone(),
            guide_id: guide_id.to_owned(),
            version: None,
        })
        .version
        .map(|record| record.version.status)
}

#[test]
fn stopping_a_turn_stops_every_guide_it_writes_and_no_other() {
    let mut setup = setup(170, 50, true);
    setup.scenario.key("alt+g");
    setup.settle();
    setup.scenario.key("enter");
    setup.settle();
    let (session, _) = setup
        .backend
        .host()
        .turns()
        .last()
        .cloned()
        .expect("the walkthrough was sent");
    writing_guide(
        &setup,
        "hidden",
        bcode_code_review_models::GuideScope::Layer {
            source_id: "elsewhere".to_owned(),
        },
        &session,
    );
    writing_guide(
        &setup,
        "foreign",
        bcode_code_review_models::GuideScope::Layer {
            source_id: "other".to_owned(),
        },
        "another-session",
    );
    let mut turn = Turn::new(&session, crate::surface::WALKTHROUGH);
    turn.plan_and_start(&mut setup);
    let shown = turn.guide_id.clone().expect("guide");
    settle_until(&mut setup, "the started guide is shown", |setup| {
        setup
            .app()
            .review()
            .and_then(|review| review.guide())
            .is_some_and(|active| active.guide.guide_id == shown)
    });
    setup.scenario.key("ctrl+c");
    settle_until(&mut setup, "the session's versions are stopped", |setup| {
        status_of(setup, &shown) == Some(GuideStatus::Cancelled)
            && status_of(setup, "hidden") == Some(GuideStatus::Cancelled)
    });
    assert_eq!(
        status_of(&setup, "foreign"),
        Some(GuideStatus::Generating),
        "another conversation's version is left to it"
    );
}

#[test]
fn guide_show_answers_once_the_guide_it_names_is_read_or_known_missing() {
    let mut setup = setup(170, 50, true);
    setup.scenario.key("alt+g");
    setup.settle();
    setup.scenario.key("enter");
    setup.settle();
    let (session, _) = setup
        .backend
        .host()
        .turns()
        .last()
        .cloned()
        .expect("the walkthrough was sent");
    let mut turn = Turn::new(&session, crate::surface::WALKTHROUGH);
    turn.plan_and_start(&mut setup);
    settle_until(&mut setup, "the guide is shown", |setup| {
        setup
            .app()
            .review()
            .and_then(|review| review.guide())
            .is_some()
    });
    writing_guide(
        &setup,
        "second",
        bcode_code_review_models::GuideScope::Layer {
            source_id: "elsewhere".to_owned(),
        },
        &session,
    );
    let ask = |setup: &mut Setup, arguments: &str| {
        let mut reply =
            setup
                .backend
                .host()
                .call_client_tool(&session, actions::GUIDE_SHOW, arguments);
        (0..20)
            .find_map(|_| {
                setup.settle();
                reply.try_recv().ok()
            })
            .expect("guide_show answered")
    };
    let answer = ask(&mut setup, r#"{"guide_id":"bogus"}"#);
    assert!(
        matches!(&answer, ClientToolReply::Failed { message, .. } if message.contains("no guide bogus")),
        "{answer:?}"
    );
    let answer = ask(&mut setup, r#"{"guide_id":"second"}"#);
    assert!(
        matches!(&answer, ClientToolReply::Completed(_)),
        "{answer:?}"
    );
    assert_eq!(
        setup
            .app()
            .review()
            .and_then(|review| review.guide())
            .map(|active| active.guide.guide_id.clone())
            .as_deref(),
        Some("second")
    );
}

#[test]
fn a_second_walkthrough_in_the_same_conversation_is_sent_and_starts_a_new_version() {
    let mut setup = setup(170, 50, true);
    setup.scenario.key("alt+g");
    setup.settle();
    setup.scenario.key("enter");
    setup.settle();
    let (session, _) = setup
        .backend
        .host()
        .turns()
        .last()
        .cloned()
        .expect("the walkthrough was sent");
    let mut turn = Turn::new(&session, crate::surface::WALKTHROUGH);
    turn.plan_and_start(&mut setup);
    turn.items
        .push(super::assistant::answer("The guide is written."));
    let finished = snapshot(turn.items.clone());
    setup.deliver(&session, finished);
    let first = turn.guide_id.clone().expect("guide");
    settle_until(&mut setup, "the guide is shown", |setup| {
        setup
            .app()
            .review()
            .and_then(|review| review.guide())
            .is_some_and(|active| active.guide.guide_id == first)
    });

    // Asked again from the palette, in the same conversation.
    setup.scenario.key("esc");
    setup.settle();
    setup.scenario.key(":");
    setup.scenario.paste("walk me through this review");
    setup.scenario.key("enter");
    setup.settle();
    let turns = setup.backend.host().turns();
    assert_eq!(turns.len(), 2, "the second walkthrough is sent: {turns:?}");
    assert_eq!(turns[1].0, session, "to the same conversation");
    assert_eq!(turns[1].1.text, crate::surface::WALKTHROUGH);

    // The assistant may start again: the review's guide gets a new version.
    let mut again = Turn::new(&session, crate::surface::WALKTHROUGH);
    again.round = 2;
    again.items = turn.items.clone();
    again.items.push(user(crate::surface::WALKTHROUGH));
    again.plan_and_start(&mut setup);
    assert_eq!(again.guide_id.as_deref(), Some(first.as_str()));
    settle_until(&mut setup, "the new version is shown", |setup| {
        setup
            .app()
            .review()
            .and_then(|review| review.guide())
            .is_some_and(|active| active.version.version == 2)
    });
}

#[test]
#[allow(clippy::too_many_lines)]
fn expand_binds_the_conversation_to_the_section_for_that_message_only() {
    let mut setup = setup(260, 50, true);
    setup.scenario.key("alt+g");
    setup.settle();
    setup.scenario.key("enter");
    setup.settle();
    let (session, _) = setup
        .backend
        .host()
        .turns()
        .last()
        .cloned()
        .expect("the walkthrough was sent");
    let mut turn = Turn::new(&session, crate::surface::WALKTHROUGH);
    turn.plan_and_start(&mut setup);
    turn.items.push(super::assistant::answer("Done."));
    let finished = snapshot(turn.items.clone());
    setup.deliver(&session, finished);
    settle_until(&mut setup, "the guide is shown", |setup| {
        setup
            .app()
            .review()
            .and_then(|review| review.guide())
            .is_some()
    });

    // Back to the review's guide panel (it kept the keyboard), onto the step, and expand it.
    setup.scenario.key("esc");
    setup.settle();
    for _ in 0..8 {
        let on_step = setup
            .app()
            .review()
            .and_then(|review| review.guide())
            .and_then(|guide| guide.current().map(|entry| entry.id == "s3.1"))
            .unwrap_or(false);
        if on_step {
            break;
        }
        setup.scenario.key("j");
        setup.settle();
    }
    setup.scenario.key("e");
    let screen = setup.settle();
    let turns = setup.backend.host().turns();
    let (to, request) = turns.last().cloned().expect("sent");
    assert_eq!(to, session, "into the same conversation");
    assert!(request.text.starts_with("Expand §3.1"), "{}", screen.text());
    assert!(
        request
            .context
            .iter()
            .any(|block| block.label == "guide section" && block.text.contains("s3.1")),
        "{request:?}"
    );
    let grant = setup
        .binding(&session)
        .and_then(|binding| binding.guide_grant)
        .expect("bound with a grant");
    assert_eq!(
        grant.mode,
        bcode_code_review_models::GrantMode::Expand {
            section_id: "s3.1".to_owned()
        }
    );

    // The expand turn runs; a follow-up sent meanwhile waits, so the running turn keeps the
    // grant it was sent with.
    let mut items = turn.items.clone();
    items.push(user(&request.text));
    let mut running = snapshot(items.clone());
    running.runtime.active_turn_id = Some("turn-expand".to_owned());
    setup.deliver(&session, running);
    setup.scenario.paste("Thanks.");
    setup.scenario.key("enter");
    let screen = setup.settle();
    assert_eq!(
        setup.backend.host().turns().len(),
        turns.len(),
        "the follow-up is held: {}",
        screen.text()
    );
    assert!(
        screen.contains("sent once the running turn ends"),
        "{}",
        screen.text()
    );
    assert!(
        setup
            .binding(&session)
            .and_then(|binding| binding.guide_grant)
            .is_some_and(|grant| matches!(
                grant.mode,
                bcode_code_review_models::GrantMode::Expand { .. }
            )),
        "the running expand keeps its grant"
    );

    // Once the expand turn ends, the follow-up is sent bound without a grant again.
    items.push(super::assistant::answer("Expanded."));
    setup.deliver(&session, snapshot(items));
    setup.settle();
    let sent = setup.backend.host().turns();
    assert_eq!(sent.len(), turns.len() + 1, "{sent:?}");
    assert_eq!(
        sent.last().map(|(_, request)| request.text.as_str()),
        Some("Thanks.")
    );
    assert_eq!(
        setup
            .binding(&session)
            .and_then(|binding| binding.guide_grant),
        None
    );
}
