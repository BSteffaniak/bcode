//! Sharing the guide and the guide's chrome: copying a section or the whole guide as markdown,
//! exporting it to a file through the markdown file publisher, choosing a template for a
//! walkthrough, the header's guide pill opening the guide, the dragged outline split restored
//! with the review, and the panel telling a guide that explains files.

use super::guide::{area, guide_hits, guided};
use super::stable;
use crate::surface::GUIDE_PANE_ID;
use crate::testing::harness::{Scenario, Screen};
use bcode_code_review_models::{GetGuideVersionRequest, ReviewWorkspace, SaveGuideVersionRequest};
use bmux_tui::event::{MouseButton, MouseEventKind};
use bmux_tui::geometry::Rect;
use std::time::Duration;

/// Outline rows the panel registers, top first.
fn outline_rows(screen: &Screen) -> Vec<Rect> {
    let mut rows: Vec<Rect> = guide_hits(screen)
        .into_iter()
        .filter(|(id, _)| id.contains(".row."))
        .map(|(_, area)| area)
        .collect();
    rows.sort_by_key(|area| area.y);
    rows
}

/// Run the palette entry `label`.
fn palette(scenario: &mut Scenario<crate::ReviewSurface>, label: &str) {
    scenario.key(":");
    scenario.paste(label);
    scenario.key("enter");
}

#[test]
fn a_section_or_the_whole_guide_is_copied_as_markdown() {
    let (mut setup, _, _fixture) = guided(170, 40);
    setup.scenario.key("alt+g");
    stable(&mut setup.scenario);
    setup.scenario.key("y");
    stable(&mut setup.scenario);
    let copied = setup.scenario.host().clipboard();
    let section = copied.last().expect("the section is copied");
    assert!(
        section.starts_with("# Fixture walkthrough · v1"),
        "{section}"
    );
    assert!(section.contains("Purpose"), "{section}");
    assert!(
        !section.contains("Reading order"),
        "only the current section: {section}"
    );

    palette(&mut setup.scenario, "Guide: Copy as markdown");
    stable(&mut setup.scenario);
    let copied = setup.scenario.host().clipboard();
    let whole = copied.last().expect("the guide is copied");
    for part in ["Purpose", "Reading order", "Risks", "the change may race"] {
        assert!(whole.contains(part), "{part}: {whole}");
    }
    assert!(
        whole.contains(" · verified") && whole.contains(" · unverified"),
        "verification is written out: {whole}"
    );
}

#[test]
fn the_guide_is_exported_to_a_file_in_the_repository_through_the_publisher() {
    let (mut setup, _, fixture) = guided(170, 40);
    palette(&mut setup.scenario, "Guide: Export to file");
    let path = fixture.root().join(".bcode/guides/guide-1-v1.md");
    for _ in 0..20 {
        if path.exists() {
            break;
        }
        stable(&mut setup.scenario);
    }
    let written = std::fs::read_to_string(&path).expect("the export is written");
    assert!(
        written.starts_with("# Fixture walkthrough · v1"),
        "{written}"
    );
    assert!(written.contains("Risks"), "{written}");
    let screen = stable(&mut setup.scenario);
    assert!(
        screen.contains("exported the guide"),
        "the status says where: {}",
        screen.text()
    );
}

#[test]
fn a_walkthrough_template_is_chosen_from_a_menu_and_waits_for_directions() {
    let (mut setup, _, _fixture) = guided(170, 40);
    palette(
        &mut setup.scenario,
        "Guide: walk me through this review, with a template",
    );
    stable(&mut setup.scenario);
    let menu = setup.scenario.surface().overlay().expect("template menu");
    assert_eq!(
        menu.shown_actions(),
        [
            "guide_template:default",
            "guide_template:explain",
            "guide_template:freeform"
        ]
    );
    setup.scenario.key("down");
    setup.scenario.key("down");
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    let requests = setup.scenario.surface_mut().take_assistant_requests();
    assert_eq!(
        requests,
        [crate::assistant::LinkRequest::Message {
            text: "Walk me through this review using the freeform template. Directions: "
                .to_owned(),
            send: false
        }]
    );
}

#[test]
fn the_header_pill_opens_the_guide_where_its_panel_has_no_room() {
    // Too short for the guide under the document and too narrow beside it.
    let (mut setup, _, _fixture) = guided(80, 14);
    setup.scenario.key("alt+g");
    let screen = stable(&mut setup.scenario);
    assert!(area(&setup.scenario, GUIDE_PANE_ID).is_none());
    let (x, y) = (0..screen.height())
        .find_map(|y| {
            let row = screen.row_text(y);
            row.find("Guide 1/").map(|byte| {
                let x = bmux_tui::text_width::display_width(&row[..byte]);
                (u16::try_from(x).unwrap_or(0), y)
            })
        })
        .unwrap_or_else(|| panic!("no guide pill: {}", screen.text()));
    let _ = setup.scenario.surface_mut().take_assistant_requests();
    setup.scenario.click(x + 1, y);
    stable(&mut setup.scenario);
    assert_eq!(
        setup.scenario.surface_mut().take_assistant_requests(),
        [crate::assistant::LinkRequest::OpenGuide],
        "the app opens the assistant's Guide tab"
    );
}

#[test]
fn the_dragged_outline_split_is_restored_with_the_review() {
    let (mut setup, _, _fixture) = guided(170, 40);
    setup.scenario.key("alt+g");
    let screen = stable(&mut setup.scenario);
    let rows = outline_rows(&screen);
    assert_eq!(rows.len(), 6, "{}", screen.text());
    let (x, rule) = (rows[5].x + 2, rows[5].y + 1);
    setup
        .scenario
        .mouse(MouseEventKind::Down(MouseButton::Left), x, rule);
    setup
        .scenario
        .mouse(MouseEventKind::Drag(MouseButton::Left), x, rule - 3);
    setup
        .scenario
        .mouse(MouseEventKind::Up(MouseButton::Left), x, rule - 3);
    let screen = stable(&mut setup.scenario);
    assert_eq!(outline_rows(&screen).len(), 3, "{}", screen.text());
    let presentation = setup.scenario.surface().presentation();
    assert!(
        presentation.panels.guide_outline_share.is_some(),
        "{presentation:?}"
    );

    // Reopened (a restart): the panel opens with the outline as dragged.
    let workspace = ReviewWorkspace {
        presentation: Some(presentation),
        ..setup.workspace.clone()
    };
    let mut scenario = Scenario::new(
        super::interaction::build(&setup.backend, &workspace, Duration::ZERO),
        170,
        40,
    );
    let mut screen = stable(&mut scenario);
    for _ in 0..10 {
        if !outline_rows(&screen).is_empty() {
            break;
        }
        screen = stable(&mut scenario);
    }
    assert_eq!(outline_rows(&screen).len(), 3, "{}", screen.text());
}

#[test]
fn the_panel_says_when_the_guide_explains_files() {
    let (mut setup, _, _fixture) = guided(170, 40);
    let scope = crate::notes::NoteScope::for_workspace(&setup.workspace);
    let record = setup
        .backend
        .call_now(GetGuideVersionRequest {
            repo_path: scope.repo_path.clone(),
            guide_id: "guide-1".to_owned(),
            version: None,
        })
        .version
        .expect("version");
    let mut version = record.version;
    version.origin.template = Some("explain".to_owned());
    let _ = setup.backend.call_now(SaveGuideVersionRequest {
        repo_path: scope.repo_path,
        version,
        expected_revision: record.revision,
    });
    setup.scenario.surface_mut().reload_guide();
    setup.scenario.key("alt+g");
    let mut screen = stable(&mut setup.scenario);
    for _ in 0..10 {
        if screen.contains("explaining files (no diff)") {
            break;
        }
        screen = stable(&mut setup.scenario);
    }
    assert!(
        screen.contains("explaining files (no diff)"),
        "{}",
        screen.text()
    );
}

/// Start a guide as `session` through the review tools and deliver the call to the panel, the
/// way the assistant's turn does, with the turn still running.
fn start_guide(setup: &mut super::assistant::Setup, session: &str) {
    use bcode_code_review::assistant::guide::{DeclaredContext, GuideStart};
    use bcode_code_review::assistant::{ReviewRef, ReviewTools};
    let outline = serde_json::json!([
        {"id": "s1", "kind": "purpose", "title": "Purpose"},
        {"id": "s2", "kind": "scope", "title": "Scope"}
    ]);
    let services = setup.backend.services();
    let data = bcode_code_review::ReviewData::default();
    let mut tools = ReviewTools::new(&data, services.store());
    let opened = tools
        .open(
            &setup.review.repo_root,
            &ReviewRef::Session(session.to_owned()),
        )
        .expect("bound review");
    tools
        .guide_start(
            &opened,
            Some(session),
            &GuideStart {
                title: Some("Walkthrough".to_owned()),
                outline: serde_json::from_value(outline.clone()).expect("outline"),
                ..GuideStart::default()
            },
            &bcode_code_review::guide::config::GuideConfig::default(),
            &DeclaredContext::default(),
        )
        .expect("started");
    let mut snapshot = super::assistant::snapshot(vec![
        super::assistant::user("Walk me through this review."),
        super::assistant::tool(
            "start-1",
            "code_review.guide.start",
            &serde_json::json!({"title": "Walkthrough", "outline": outline}).to_string(),
        ),
    ]);
    snapshot.runtime.active_turn_id = Some("turn-1".to_owned());
    setup.deliver(session, snapshot);
}

#[test]
fn a_guide_started_in_a_narrow_assistant_shows_its_tab_and_the_pill_reopens_it() {
    // Docked under a screen too short for the guide panel.
    let mut setup = super::assistant::setup(80, 30, true);
    setup.scenario.key("ctrl+k");
    let session = setup.send("Walk me through this review.");
    start_guide(&mut setup, &session);
    for _ in 0..12 {
        if setup.app().assistant().guide_tab_shown() {
            break;
        }
        setup.settle();
    }
    let screen = setup.settle();
    assert!(
        setup.app().assistant().guide_tab_shown(),
        "the Guide tab shows by itself: {}",
        screen.text()
    );
    assert!(!screen.contains("Chat •"), "{}", screen.text());

    // The turn finishing marks the Chat tab, which stays one click away.
    setup.deliver(
        &session,
        super::assistant::snapshot(vec![
            super::assistant::user("Walk me through this review."),
            super::assistant::answer("Here is the walkthrough."),
        ]),
    );
    let screen = setup.settle();
    assert!(screen.contains("Chat •"), "{}", screen.text());
    assert!(setup.app().assistant().guide_tab_shown());

    // Back on the conversation with the guide open but nowhere to show it: the header's pill
    // opens the Guide tab again.
    let chat = screen.find("Chat •").expect("chat tab");
    setup.scenario.click(chat.x + 1, chat.y);
    setup.settle();
    assert!(!setup.app().assistant().guide_tab_shown());
    setup
        .scenario
        .surface_mut()
        .review_mut()
        .expect("review")
        .open_guide_panel();
    let screen = setup.settle();
    let pill = screen
        .find("Guide 1/")
        .unwrap_or_else(|| panic!("no guide pill: {}", screen.text()));
    setup.scenario.click(pill.x + 1, pill.y);
    let screen = setup.settle();
    assert!(
        setup.app().assistant().guide_tab_shown(),
        "{}",
        screen.text()
    );
}
