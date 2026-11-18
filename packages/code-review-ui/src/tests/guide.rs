//! The guided review on the review screen, driven by guides seeded through the review store:
//! the guide panel's placements and narrow-width degradation, citation badges and the current
//! section's tint in the diff, guide navigation with jumping back, gap reveal for cited context,
//! the context peek for code outside the review, and glyphs that keep reviewed, verified, and
//! approved apart.

use super::stable;
use crate::guide::panel::{self, GuidePanelView, PanelClick, Placement};
use crate::surface::{GUIDE_PANE_ID, PEEK_ID, REVIEW_PANE_ID};
use crate::testing::harness::{Scenario, Screen, paint};
use crate::theme::{IconStyle, Icons, ReviewTheme};
use crate::{Focus, ReviewSurface};
use bcode_code_review::document::{FileRef, ItemKey, ReviewItem};
use bcode_code_review_models::{
    Citation, CitationRelation, CreateReviewGuideRequest, Finding, FindingKind, GuideOrigin,
    GuideScope, GuideStatus, GuideVersion, REVIEW_GUIDE_SCHEMA_VERSION, ReviewGuide,
    ReviewLineInterval, ReviewLineSide, ReviewThreadSeverity, RevisionRef, Section, SectionKind,
    SectionState, Verification,
};
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::geometry::{Rect, Size};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use super::interaction::{Setup, build, fixture, move_to};
use crate::testing::backend::TestBackend;
use bcode_code_review::guide::config::{GuideConfig, Markers};
use bcode_code_review_models::{ReviewWorkspace, UpdateReviewWorkspaceRequest};
use std::sync::Arc;
use std::time::Duration;

/// What the seeded guide cites.
pub(super) struct Cited {
    /// A changed line: its file and new-side line.
    pub(super) changed: (FileRef, u32),
    /// A line hidden in a gap of a diff file: its file and new-side line.
    pub(super) gap: (FileRef, u32),
}

/// A CJK and emoji title, to check cell-aware fitting.
const WIDE_TITLE: &str = "変更された行 🚀 RetryBudget";

pub(super) fn section(id: &str, kind: SectionKind, title: &str, citations: &[&str]) -> Section {
    Section {
        id: id.to_owned(),
        kind,
        title: title.to_owned(),
        body: String::new(),
        findings: Vec::new(),
        citations: citations.iter().map(|id| (*id).to_owned()).collect(),
        children: Vec::new(),
        state: SectionState::Written,
        refreshed_from: None,
        checks: Vec::new(),
    }
}

pub(super) fn citation(
    id: &str,
    file: &FileRef,
    lines: (u32, u32),
    relation: CitationRelation,
    verified: bool,
    revision: &RevisionRef,
) -> Citation {
    Citation {
        id: id.to_owned(),
        source_id: Some(file.source_id.clone()),
        path: file.path.clone(),
        side: ReviewLineSide::New,
        lines: ReviewLineInterval {
            start: lines.0,
            end: lines.1,
        },
        revision: revision.clone(),
        blob_oid: None,
        quote_hash: String::new(),
        excerpt: String::new(),
        relation,
        verification: if verified {
            Verification::Verified {
                at_ms: 1,
                adjusted_from: None,
            }
        } else {
            Verification::Unverified
        },
    }
}

/// A changed line and a gap line of the open review.
pub(super) fn cited(scenario: &Scenario<ReviewSurface>) -> Cited {
    let doc = scenario.surface().document().expect("document");
    let mut changed = None;
    let mut gap = None;
    for index in 0..doc.len() {
        match doc.item(index) {
            Some(ReviewItem::Line(line)) if changed.is_none() && line.old_line.is_none() => {
                changed = line.new_line.map(|number| (line.file.clone(), number));
            }
            Some(ReviewItem::LinePair(pair))
                if changed.is_none() && pair.old.is_none() && pair.new.is_some() =>
            {
                changed = pair.new.as_ref().map(|side| (pair.file.clone(), side.line));
            }
            Some(ReviewItem::Gap(item))
                if gap.is_none() && item.end.is_some_and(|end| end > item.start + 1) =>
            {
                gap = Some((item.file.clone(), item.start + 1));
            }
            _ => {}
        }
    }
    Cited {
        changed: changed.expect("an added line"),
        gap: gap.expect("a gap with hidden lines"),
    }
}

/// Seed a guide for the review: a purpose, a reading order of three steps (a changed line, a
/// line in a gap, and code outside the review), and a risks section.
pub(super) fn seed(setup: &mut Setup, cited: &Cited) {
    let head = setup
        .scenario
        .surface()
        .document()
        .and_then(|doc| doc.groups().next().map(|group| group.source.head.clone()))
        .expect("a source");
    let (changed_file, changed_line) = &cited.changed;
    let (gap_file, gap_line) = &cited.gap;
    let citations = vec![
        citation(
            "c1",
            changed_file,
            (*changed_line, *changed_line),
            CitationRelation::Changed,
            true,
            &head,
        ),
        citation(
            "c2",
            gap_file,
            (*gap_line, *gap_line),
            CitationRelation::Unchanged,
            false,
            &head,
        ),
        citation(
            "c3",
            changed_file,
            (1, 3),
            CitationRelation::OutsideReview,
            true,
            &head,
        ),
    ];
    let mut purpose = section("s1", SectionKind::Purpose, "Purpose", &[]);
    purpose.body = "Walks through the change [[c1]] and its context [[c2]].".to_owned();
    let mut reading = section("s2", SectionKind::ReadingOrder, "Reading order", &[]);
    reading.children = vec![
        section("s2.1", SectionKind::Step, WIDE_TITLE, &["c1"]),
        section("s2.2", SectionKind::Step, "Context in a gap", &["c2"]),
        section("s2.3", SectionKind::Step, "Outside the review", &["c3"]),
    ];
    let mut risks = section(
        "s3",
        SectionKind::Topic {
            key: "risks".to_owned(),
        },
        "Risks",
        &[],
    );
    risks.findings = vec![Finding {
        id: "f1".to_owned(),
        kind: FindingKind::Risk,
        severity: Some(ReviewThreadSeverity::Warning),
        text: "the change may race".to_owned(),
        citations: Vec::new(),
        drafted_thread_id: None,
    }];
    let scope = crate::notes::NoteScope::for_workspace(&setup.workspace);
    let _ = setup.backend.call_now(CreateReviewGuideRequest {
        repo_path: scope.repo_path,
        target: scope.target,
        scope: scope.scope,
        guide: ReviewGuide {
            guide_id: "guide-1".to_owned(),
            review_key: String::new(),
            scope: GuideScope::Review,
            parent_guide_id: None,
            title: "Fixture walkthrough".to_owned(),
            current_version: 1,
            created_at_ms: 1,
            updated_at_ms: 1,
            archived_at_ms: None,
        },
        version: GuideVersion {
            guide_id: "guide-1".to_owned(),
            version: 1,
            schema_version: REVIEW_GUIDE_SCHEMA_VERSION,
            heads: BTreeMap::new(),
            based_on: None,
            origin: GuideOrigin::default(),
            status: GuideStatus::Complete,
            generation: None,
            outline: vec![purpose, reading, risks],
            citations: citations
                .into_iter()
                .map(|citation| (citation.id.clone(), citation))
                .collect(),
            retired_sections: Vec::new(),
            created_at_ms: 1,
            updated_at_ms: 1,
        },
    });
    setup.scenario.surface_mut().reload_guide();
    stable(&mut setup.scenario);
}

/// A durable review at `width` x `height` whose guide markers follow `markers`.
fn setup_with(
    fixture: &crate::testing::fixture::GitFixture,
    width: u16,
    height: u16,
    markers: Markers,
) -> Setup {
    setup_configured(
        fixture,
        width,
        height,
        GuideConfig {
            markers,
            ..GuideConfig::default()
        },
    )
}

/// A durable review at `width` x `height` with guide configuration `config`.
pub(super) fn setup_configured(
    fixture: &crate::testing::fixture::GitFixture,
    width: u16,
    height: u16,
    config: GuideConfig,
) -> Setup {
    let backend = Arc::new(TestBackend::default().with_guide_config(config));
    let workspace = ReviewWorkspace {
        id: "guide-review".to_owned(),
        ..ReviewWorkspace::from_target(
            fixture.root().to_path_buf(),
            crate::testing::fixture_branch_compare(),
        )
    };
    let _ = backend.call_now(UpdateReviewWorkspaceRequest {
        repo_path: workspace.repo_root.clone(),
        workspace: workspace.clone(),
        base: None,
    });
    let mut scenario = Scenario::new(build(&backend, &workspace, Duration::ZERO), width, height);
    stable(&mut scenario);
    Setup {
        scenario,
        backend,
        workspace,
    }
}

/// A review at `width` x `height` with the seeded guide read.
pub(super) fn guided(
    width: u16,
    height: u16,
) -> (Setup, Cited, crate::testing::fixture::GitFixture) {
    guided_with(width, height, Markers::Current)
}

/// [`guided`] with the guide markers following `markers`.
fn guided_with(
    width: u16,
    height: u16,
    markers: Markers,
) -> (Setup, Cited, crate::testing::fixture::GitFixture) {
    let fixture = fixture();
    let mut setup = setup_with(&fixture, width, height, markers);
    let cited = cited(&setup.scenario);
    seed(&mut setup, &cited);
    assert!(
        setup.scenario.surface().guide().is_some(),
        "the seeded guide is read"
    );
    (setup, cited, fixture)
}

pub(super) fn area(scenario: &Scenario<ReviewSurface>, id: &str) -> Option<Rect> {
    scenario.surface().body_area(id)
}

pub(super) fn entry_of(scenario: &Scenario<ReviewSurface>, id: &str) -> usize {
    scenario
        .surface()
        .guide()
        .and_then(|guide| guide.entries.iter().position(|entry| entry.id == id))
        .expect("section")
}

pub(super) fn selected(scenario: &Scenario<ReviewSurface>) -> String {
    let guide = scenario.surface().guide().expect("guide");
    guide.entries[guide.selected].id.clone()
}

/// The line the cursor is on, when it is on a diff line: file, new-side line.
fn cursor_line(scenario: &Scenario<ReviewSurface>) -> Option<(FileRef, u32)> {
    let doc = scenario.surface().document()?;
    let index = doc.index_of(scenario.surface().cursor()?)?;
    match doc.item(index)? {
        ReviewItem::Line(line) => line.new_line.map(|number| (line.file.clone(), number)),
        ReviewItem::LinePair(pair) => pair.new.as_ref().map(|side| (pair.file.clone(), side.line)),
        _ => None,
    }
}

/// Hits of the guide panel's content.
pub(super) fn guide_hits(screen: &Screen) -> Vec<(String, Rect)> {
    screen
        .hits()
        .iter()
        .filter(|hit| hit.id.as_str().starts_with(panel::HIT_PREFIX))
        .map(|hit| (hit.id.as_str().to_owned(), hit.area))
        .collect()
}

#[test]
fn guide_panel_sits_beside_or_under_the_document_and_follows_resizes() {
    let (mut setup, _, _fixture) = guided(170, 40);
    setup.scenario.key("alt+g");
    let screen = stable(&mut setup.scenario);
    assert_eq!(setup.scenario.surface().focus(), Focus::Guide);
    let review = area(&setup.scenario, REVIEW_PANE_ID).expect("review");
    let guide = area(&setup.scenario, GUIDE_PANE_ID).expect("guide beside the document");
    assert!(guide.x >= review.right(), "{guide:?} right of {review:?}");
    assert!(screen.contains("Guide · v1"), "{}", screen.text());
    assert!(
        screen.contains("2.1"),
        "the outline shows: {}",
        screen.text()
    );

    // Medium: still beside the document, at least the guide's minimum width.
    setup.scenario.resize(120, 40);
    let screen = stable(&mut setup.scenario);
    let review = area(&setup.scenario, REVIEW_PANE_ID).expect("review");
    let guide = area(&setup.scenario, GUIDE_PANE_ID).expect("guide beside");
    assert!(guide.x >= review.right());
    assert!(guide.width >= crate::panels::GUIDE_MIN_WIDTH, "{guide:?}");
    assert!(
        review.width >= crate::panels::REVIEW_MIN_WIDTH,
        "{review:?}"
    );
    assert!(screen.contains("Reading order"), "{}", screen.text());

    // Narrow: under the document, the current section alone with where it is.
    setup.scenario.resize(90, 30);
    let screen = stable(&mut setup.scenario);
    let review = area(&setup.scenario, REVIEW_PANE_ID).expect("review");
    let guide = area(&setup.scenario, GUIDE_PANE_ID).expect("guide under");
    assert!(guide.y >= review.bottom(), "{guide:?} under {review:?}");
    assert!(guide.height >= crate::panels::GUIDE_MIN_HEIGHT);
    assert!(
        screen.contains("1/6"),
        "position under the document: {}",
        screen.text()
    );

    // Too short for it under the document: hidden, and focus goes back to the document; the
    // header says where the guide stands instead.
    setup.scenario.resize(90, 12);
    let screen = stable(&mut setup.scenario);
    assert!(area(&setup.scenario, GUIDE_PANE_ID).is_none());
    assert!(guide_hits(&screen).is_empty(), "no hits of a hidden panel");
    assert_eq!(setup.scenario.surface().focus(), Focus::Review);
    assert!(
        screen.row_text(0).contains("Guide 1/6"),
        "the header's guide chip: {}",
        screen.text()
    );

    // Tiny bodies have no panels at all.
    for (width, height) in [(20, 8), (1, 1), (0, 0)] {
        setup.scenario.resize(width, height);
        let screen = stable(&mut setup.scenario);
        assert!(
            area(&setup.scenario, GUIDE_PANE_ID).is_none(),
            "{width}x{height}"
        );
        assert!(guide_hits(&screen).is_empty(), "{width}x{height}");
    }

    // Back to wide: beside again; the open state was kept, and the chip is gone.
    setup.scenario.resize(170, 40);
    let screen = stable(&mut setup.scenario);
    assert!(area(&setup.scenario, GUIDE_PANE_ID).is_some());
    assert!(
        !screen.row_text(0).contains("Guide 1/6"),
        "{}",
        screen.text()
    );
}

#[test]
fn a_review_without_a_guide_says_how_to_get_one() {
    let fixture = fixture();
    let mut setup = setup_with(&fixture, 150, 40, Markers::Current);
    setup.scenario.key("alt+g");
    let screen = stable(&mut setup.scenario);
    assert!(
        screen.contains("No guide for this review"),
        "{}",
        screen.text()
    );
    assert!(
        screen.contains("enter asks the review"),
        "{}",
        screen.text()
    );
    setup.scenario.key("alt+down");
    let screen = stable(&mut setup.scenario);
    assert!(
        screen.contains("no guide for this review yet"),
        "{}",
        screen.text()
    );
    // `enter` on the empty panel asks the assistant for a walkthrough.
    setup.scenario.surface_mut().take_assistant_requests();
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    assert_eq!(
        setup.scenario.surface_mut().take_assistant_requests(),
        vec![crate::assistant::LinkRequest::Message {
            text: crate::surface::WALKTHROUGH.to_owned(),
            send: true,
        }]
    );
}

#[test]
fn guide_and_threads_share_the_right_column() {
    let (mut setup, _, _fixture) = guided(180, 40);
    setup.scenario.key("alt+g");
    setup.scenario.key("shift+t");
    stable(&mut setup.scenario);
    let guide = area(&setup.scenario, GUIDE_PANE_ID).expect("guide");
    let threads = area(&setup.scenario, crate::surface::THREADS_PANE_ID).expect("threads");
    assert_eq!(guide.x, threads.x, "one column");
    assert!(threads.y >= guide.bottom(), "the guide on top");
}

#[test]
fn alt_down_moves_to_the_cited_line_with_its_tint_and_backspace_returns() {
    let (mut setup, cited, _fixture) = guided(150, 40);
    let before = setup.scenario.surface().cursor().cloned();
    // The first step shows where the guide starts; sections citing nothing are stops too.
    let screen = step_to(&mut setup.scenario, "s2.1");
    assert_eq!(cursor_line(&setup.scenario), Some(cited.changed.clone()));
    let (file, line) = &cited.changed;
    let marks = setup
        .scenario
        .surface()
        .guide_marks()
        .at(file, ReviewLineSide::New, *line, None);
    assert!(marks.focus, "the current section's line is tinted");
    assert_eq!(marks.badge.map(|badge| badge.number), Some(1));
    // The badge is painted in the document, and the row's gutter carries the tint (seen once
    // the cursor line, which paints over it, moves on).
    setup.scenario.key("j");
    let screen = {
        let _ = screen;
        stable(&mut setup.scenario)
    };
    let review = area(&setup.scenario, REVIEW_PANE_ID).expect("review");
    let row = (review.y..review.bottom())
        .find(|y| screen.text_in(review, *y).contains("[1]◈"))
        .expect("the badge is painted");
    let focus = ReviewTheme::derive(None, IconStyle::Unicode).guide.focus;
    let tinted = (review.x..review.x + 6).any(|x| screen.style_at(x, row).bg == focus.bg);
    assert!(tinted, "row {row}: {}", screen.row_text(row));

    setup.scenario.key("backspace");
    stable(&mut setup.scenario);
    assert_eq!(setup.scenario.surface().cursor().cloned(), before);
    assert!(
        setup.scenario.surface().guide().is_some(),
        "jumping back keeps the guide"
    );
}

/// Press `alt+down` until section `id` is the current one.
pub(super) fn step_to(scenario: &mut Scenario<ReviewSurface>, id: &str) -> Screen {
    for _ in 0..16 {
        scenario.key("alt+down");
        let screen = stable(scenario);
        if selected(scenario) == id {
            return screen;
        }
    }
    panic!("never reached {id}");
}

#[test]
fn a_badge_click_focuses_its_section() {
    let (mut setup, cited, _fixture) = guided_with(150, 40, Markers::All);
    let (file, line) = cited.changed;
    let index = setup
        .scenario
        .surface()
        .document()
        .unwrap()
        .index_of(&ItemKey::Line {
            file,
            side: ReviewLineSide::New,
            line,
        })
        .expect("the changed line is shown");
    move_to(&mut setup.scenario, index);
    // Opening the panel shows every citation's badge (`markers = "all"`); select the risks.
    setup.scenario.key("alt+g");
    for _ in 0..16 {
        if selected(&setup.scenario) == "s3" {
            break;
        }
        setup.scenario.key("j");
    }
    let screen = stable(&mut setup.scenario);
    assert_eq!(selected(&setup.scenario), "s3");
    let review = area(&setup.scenario, REVIEW_PANE_ID).expect("review");
    let (x, y) = (review.y..review.bottom())
        .find_map(|y| {
            screen
                .text_in(review, y)
                .contains("[1]◈")
                .then(|| screen.column_of(y, "[1]◈").map(|x| (x, y)))
                .flatten()
        })
        .expect("the badge in the document");
    setup.scenario.click(x + 1, y);
    stable(&mut setup.scenario);
    assert_eq!(selected(&setup.scenario), "s2.1", "the badge's section");
    assert!(area(&setup.scenario, GUIDE_PANE_ID).is_some());
}

#[test]
fn context_in_a_gap_is_revealed_and_code_outside_the_review_peeks() {
    let (mut setup, cited, _fixture) = guided(150, 40);
    let (file, line) = cited.gap;
    let hidden = setup
        .scenario
        .surface()
        .document()
        .unwrap()
        .index_of(&ItemKey::Line {
            file: file.clone(),
            side: ReviewLineSide::New,
            line,
        });
    assert!(hidden.is_none(), "the cited line starts hidden in its gap");
    setup.scenario.key("alt+g");
    stable(&mut setup.scenario);
    let gap_step = entry_of(&setup.scenario, "s2.2");
    for _ in 0..16 {
        if setup.scenario.surface().guide().unwrap().selected == gap_step {
            break;
        }
        setup.scenario.key("j");
    }
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    assert_eq!(cursor_line(&setup.scenario), Some((file.clone(), line)));
    assert!(
        setup
            .scenario
            .surface()
            .document()
            .unwrap()
            .revealed(&file)
            .is_some_and(|revealed| revealed.contains(line)),
        "the gap was revealed"
    );

    setup.scenario.key("j");
    setup.scenario.key("enter");
    let screen = stable(&mut setup.scenario);
    assert!(setup.scenario.surface().guide_peek().is_some());
    assert!(screen.contains("outside this review"), "{}", screen.text());
    assert!(
        area(&setup.scenario, PEEK_ID).is_some(),
        "the peek is in the document's place"
    );
    // `esc` leaves the panel for the document, then leaves the peek.
    setup.scenario.key("esc");
    assert_eq!(setup.scenario.surface().focus(), Focus::Review);
    setup.scenario.key("esc");
    stable(&mut setup.scenario);
    assert!(setup.scenario.surface().guide_peek().is_none());
    assert!(area(&setup.scenario, PEEK_ID).is_none());
}

#[test]
fn the_line_under_the_cursor_shows_its_section() {
    let (mut setup, cited, _fixture) = guided(150, 40);
    let (file, line) = cited.changed;
    let doc = setup.scenario.surface().document().unwrap();
    let index = doc
        .index_of(&ItemKey::Line {
            file,
            side: ReviewLineSide::New,
            line,
        })
        .expect("the changed line is shown");
    super::interaction::move_to(&mut setup.scenario, index);
    setup.scenario.key("'");
    stable(&mut setup.scenario);
    assert_eq!(selected(&setup.scenario), "s2.1");
    assert!(
        area(&setup.scenario, GUIDE_PANE_ID).is_some(),
        "the panel opens"
    );
}

#[test]
fn panel_keys_reserved_for_later_say_so_instead_of_acting_on_the_review() {
    let (mut setup, _, _fixture) = guided(150, 40);
    setup.scenario.key("alt+g");
    stable(&mut setup.scenario);
    let first = setup
        .scenario
        .surface()
        .document()
        .unwrap()
        .file_keys()
        .next()
        .cloned()
        .unwrap();
    setup.scenario.key("r");
    let screen = stable(&mut setup.scenario);
    assert!(screen.contains("not available yet"), "{}", screen.text());
    let doc = setup.scenario.surface().document().unwrap();
    assert!(
        !doc.viewed_state(&first)
            .is_some_and(bcode_code_review::viewed::ViewedState::is_viewed),
        "`r` in the guide marks no file viewed"
    );
}

/// The panel component alone at `width` x `height`.
fn panel_screen(model: &crate::guide::PanelModel, beside: bool, width: u16, height: u16) -> Screen {
    let theme = ReviewTheme::derive(None, IconStyle::Unicode);
    let scroll = Cell::new(0);
    let clicks: RefCell<Vec<PanelClick>> = RefCell::new(Vec::new());
    let split = RefCell::new(panel::split_state());
    let view = GuidePanelView::new("guide", model, &theme, beside, &scroll, &clicks, &split);
    paint(width, height, |area, cx| {
        let node = view.layout(
            Constraints::tight(Size::new(area.width, area.height)),
            &mut LayoutCx::new(),
        );
        view.paint(&node, cx);
    })
}

#[test]
fn the_panel_fits_every_width_and_registers_hits_only_for_painted_rows() {
    let (setup, _, _fixture) = guided(150, 40);
    let guide = setup.scenario.surface().guide().expect("guide").clone();
    let theme = ReviewTheme::derive(None, IconStyle::Unicode);
    for placement in [Placement::Beside, Placement::Under] {
        let model = panel::build_model(
            &guide,
            &std::collections::BTreeSet::new(),
            &theme,
            placement,
            1,
        );
        for width in [0_u16, 1, 2, 5, 11, 12, 23, 24, 30, 36, 60] {
            for height in [0_u16, 1, 3, 6, 8, 12, 30] {
                let screen = panel_screen(&model, placement == Placement::Beside, width, height);
                for y in 0..height {
                    let row = screen.row_text(y);
                    assert!(
                        bmux_tui::text_width::display_width(row.trim_end()) <= usize::from(width),
                        "{placement:?} {width}x{height} row {y}: {row:?}"
                    );
                }
                let hits = guide_hits(&screen);
                for (id, rect) in &hits {
                    assert!(
                        rect.right() <= width && rect.bottom() <= height && rect.width > 0,
                        "{id} {rect:?} outside {width}x{height}"
                    );
                }
                let rows = hits.iter().filter(|(id, _)| id.contains(".row.")).count();
                assert!(rows <= usize::from(height), "{width}x{height}");
                if width >= panel::OUTLINE_ONLY_WIDTH && placement == Placement::Under {
                    assert_eq!(rows, 0, "no outline under the document");
                }
            }
        }
    }
    // A tall panel beside the document shows the whole outline, one hit per row; a short one
    // shows a window of it around the current section, and only those rows are hits.
    let model = panel::build_model(
        &guide,
        &std::collections::BTreeSet::new(),
        &theme,
        Placement::Beside,
        1,
    );
    let tall = panel_screen(&model, true, 60, 40);
    assert_eq!(
        guide_hits(&tall)
            .iter()
            .filter(|(id, _)| id.contains(".row."))
            .count(),
        guide.entries.len()
    );
    let short = panel_screen(&model, true, 60, 8);
    let shown = guide_hits(&short)
        .iter()
        .filter(|(id, _)| id.contains(".row."))
        .count();
    assert!(shown > 0 && shown < guide.entries.len(), "{shown}");
    // The CJK and emoji title is fitted by cells, never split inside a wide grapheme.
    let mut wide = guide;
    wide.select(entry_index(&wide, "s2.1"));
    let model = panel::build_model(
        &wide,
        &std::collections::BTreeSet::new(),
        &theme,
        Placement::Beside,
        2,
    );
    for width in 24..60_u16 {
        let screen = panel_screen(&model, true, width, 20);
        assert!(
            (0..20).any(|y| screen.row_text(y).contains('変')),
            "{width}: {}",
            screen.text()
        );
    }
}

fn entry_index(guide: &crate::guide::ActiveGuide, id: &str) -> usize {
    guide
        .entries
        .iter()
        .position(|entry| entry.id == id)
        .expect("section")
}

#[test]
fn reviewed_verified_and_approved_glyphs_stay_distinct() {
    for icons in [Icons::UNICODE, Icons::ASCII] {
        let guide = [
            icons.guide_verified,
            icons.guide_unverified,
            icons.guide_stale,
        ];
        let review = [icons.viewed, icons.unviewed, icons.approved];
        for glyph in guide {
            assert!(!review.contains(&glyph), "{glyph:?} in {review:?}");
        }
        assert_ne!(icons.guide_verified, icons.guide_unverified);
    }
    let (mut setup, _, _fixture) = guided(160, 40);
    setup.scenario.key("alt+g");
    let screen = step_to(&mut setup.scenario, "s2.1");
    let pane = area(&setup.scenario, GUIDE_PANE_ID).expect("guide");
    let text: String = (pane.y..pane.bottom())
        .map(|y| screen.text_in(pane, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("[1]◈"), "a verified citation: {text}");
    // Nothing is reviewed yet: the reviewed mark is the unviewed half of the viewed pair.
    assert!(text.contains(Icons::UNICODE.unviewed), "{text}");
    assert!(
        !text.contains(Icons::UNICODE.guide_unverified) || text.contains("[2]"),
        "the unverified glyph only marks unverified citations: {text}"
    );
}

/// Give citation `id` of the shown guide the text at its lines in `fixture`'s checkout, so its
/// freshness can be told after the file changes.
pub(super) fn quote_citation(
    setup: &mut Setup,
    fixture: &crate::testing::fixture::GitFixture,
    id: &str,
) {
    let repo_path = setup.workspace.repo_root.clone();
    let record = setup
        .backend
        .call_now(bcode_code_review_models::GetGuideVersionRequest {
            repo_path: repo_path.clone(),
            guide_id: "guide-1".to_owned(),
            version: None,
        })
        .version
        .expect("version");
    let mut version = record.version;
    let citation = version.citations.get_mut(id).expect("citation");
    let text = std::fs::read_to_string(fixture.root().join(&citation.path)).expect("file");
    let lines: Vec<&str> = text.lines().collect();
    let range = usize::try_from(citation.lines.start - 1).expect("start")
        ..usize::try_from(citation.lines.end).expect("end");
    citation.excerpt = lines[range].join("\n");
    let _ = setup
        .backend
        .call_now(bcode_code_review_models::SaveGuideVersionRequest {
            repo_path,
            version,
            expected_revision: record.revision,
        });
    setup.scenario.surface_mut().reload_guide();
    stable(&mut setup.scenario);
}

#[test]
fn a_refresh_tells_citation_freshness_against_the_new_head() {
    let (mut setup, cited, fixture) = guided(160, 40);
    quote_citation(&mut setup, &fixture, "c1");
    let freshness = |setup: &Setup| {
        setup
            .scenario
            .surface()
            .guide()
            .and_then(|guide| guide.freshness("c1").cloned())
    };
    assert_eq!(
        freshness(&setup),
        Some(bcode_code_review_models::Freshness::Fresh)
    );
    let (file, line) = &cited.changed;
    let path = fixture.root().join(&file.path);
    let text = std::fs::read_to_string(&path).expect("file");
    std::fs::write(&path, format!("{}{text}", "pushed\n".repeat(10))).expect("write");
    fixture.run(&["commit", "-qam", "push lines above the cited change"]);
    setup.scenario.key("r");
    stable(&mut setup.scenario);
    assert_eq!(
        freshness(&setup),
        Some(bcode_code_review_models::Freshness::Moved {
            lines: ReviewLineInterval {
                start: line + 10,
                end: line + 10,
            }
        }),
        "freshness is told against the head the refresh resolved"
    );
}

#[test]
fn a_peek_of_moved_lines_shows_them_where_they_are_now() {
    let (mut setup, cited, fixture) = guided(150, 40);
    quote_citation(&mut setup, &fixture, "c3");
    let (file, _) = &cited.changed;
    let path = fixture.root().join(&file.path);
    let text = std::fs::read_to_string(&path).expect("file");
    let cited_text: Vec<String> = text.lines().take(3).map(str::to_owned).collect();
    std::fs::write(&path, format!("{}{text}", "pushed\n".repeat(10))).expect("write");
    fixture.run(&["commit", "-qam", "push lines above the cited context"]);
    setup.scenario.key("r");
    stable(&mut setup.scenario);
    setup.scenario.key("alt+g");
    stable(&mut setup.scenario);
    let outside = entry_of(&setup.scenario, "s2.3");
    for _ in 0..16 {
        if setup.scenario.surface().guide().unwrap().selected == outside {
            break;
        }
        setup.scenario.key("j");
    }
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    let peek = setup.scenario.surface().guide_peek().expect("peek");
    assert_eq!(peek.lines, (11, 13), "{}", peek.title);
    let content = peek
        .content
        .as_ref()
        .expect("read")
        .as_ref()
        .expect("content");
    assert_eq!(
        content[10..13],
        cited_text[..],
        "the moved lines are read where they are now"
    );
}

/// Give section `id` of `sections` (at any depth) `body`.
fn set_body(sections: &mut [Section], id: &str, body: &str) {
    for section in sections {
        if section.id == id {
            body.clone_into(&mut section.body);
        }
        set_body(&mut section.children, id, body);
    }
}

#[test]
fn a_section_body_shows_as_markdown_with_its_badges() {
    let (setup, _, _fixture) = guided(150, 40);
    let mut guide = setup.scenario.surface().guide().expect("guide").clone();
    let current = guide.current().expect("a section").id.clone();
    let body = "Uses **a token bucket** [[c1]]:\n\n- refills on `success`\n- 変更 drains on failure [[c2]]";
    set_body(&mut guide.version.outline, &current, body);
    let theme = ReviewTheme::derive(None, IconStyle::Unicode);
    let model = panel::build_model(
        &guide,
        &std::collections::BTreeSet::new(),
        &theme,
        Placement::Under,
        1,
    );
    let screen = panel_screen(&model, false, 60, 20);
    let text = screen.text();
    assert!(text.contains("a token bucket"), "{text}");
    assert!(
        !text.contains("**") && !text.contains('`') && !text.contains("[[c"),
        "markdown and tokens are rendered: {text}"
    );
    assert!(text.contains("success") && text.contains("変更"), "{text}");
    let badge = screen.find("[1]").expect("the badge is shown");
    assert_eq!(screen.style_at(badge.x, badge.y), theme.guide.badge);
    for width in [1_u16, 2, 5, 12, 23] {
        let screen = panel_screen(&model, false, width, 20);
        for y in 0..20 {
            let row = screen.row_text(y);
            assert!(
                bmux_tui::text_width::display_width(row.trim_end()) <= usize::from(width),
                "{width} row {y}: {row:?}"
            );
        }
    }
}

#[test]
fn dragging_the_rule_resizes_the_outline_within_its_limits() {
    use bmux_tui::event::{MouseButton, MouseEventKind};
    let (mut setup, _, _fixture) = guided(170, 40);
    setup.scenario.key("alt+g");
    let screen = stable(&mut setup.scenario);
    let outline_rows = |screen: &Screen| {
        let mut rows: Vec<Rect> = guide_hits(screen)
            .into_iter()
            .filter(|(id, _)| id.contains(".row."))
            .map(|(_, area)| area)
            .collect();
        rows.sort_by_key(|area| area.y);
        rows
    };
    let rows = outline_rows(&screen);
    assert_eq!(rows.len(), 6, "the whole outline shows: {}", screen.text());
    let rule = rows[5].y + 1;
    let x = rows[5].x + 2;
    let drag = |setup: &mut Setup, from: u16, to: u16| {
        setup
            .scenario
            .mouse(MouseEventKind::Down(MouseButton::Left), x, from);
        setup
            .scenario
            .mouse(MouseEventKind::Drag(MouseButton::Left), x, to);
        setup
            .scenario
            .mouse(MouseEventKind::Up(MouseButton::Left), x, to);
        stable(&mut setup.scenario)
    };
    let screen = drag(&mut setup, rule, rule - 3);
    let rows = outline_rows(&screen);
    assert_eq!(rows.len(), 3, "the outline shrank: {}", screen.text());
    // It never grows past its rows, however far the rule is dragged.
    let rule = rows[2].y + 1;
    let screen = drag(&mut setup, rule, rule + 20);
    assert_eq!(outline_rows(&screen).len(), 6, "{}", screen.text());
    // A resize keeps a dragged split within the panel.
    setup.scenario.resize(120, 24);
    let screen = stable(&mut setup.scenario);
    let guide = area(&setup.scenario, GUIDE_PANE_ID).expect("guide");
    for (id, hit) in guide_hits(&screen) {
        assert!(
            hit.bottom() <= guide.bottom() && hit.right() <= guide.right(),
            "{id} {hit:?} outside {guide:?}"
        );
    }
}
