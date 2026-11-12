//! Review interaction on the review screen: comments and threads persisted through the review
//! store, the composer, AI questions and linked sessions, the threads panel, viewed
//! marks, search, the command palette and help, presentation persistence, and pointer behavior
//! (clickable actions, hover affordance, drag selection, wheel motion) with hit regions that
//! agree with painting after scrolling and resizing.

use super::stable;
use crate::action::Action;
use crate::config::ReviewViewConfig;
use crate::notes::NoteScope;
use crate::surface::THREADS_PANE_ID;
use crate::testing::backend::TestBackend;
use crate::testing::fixture::{FixtureSpec, GitFixture};
use crate::testing::fixture_branch_compare;
use crate::testing::harness::{Scenario, Screen};
use crate::view::View;
use crate::{Focus, ReviewSurface};
use bcode_code_review::document::{DocumentConfig, ItemKey, LayoutMode, ReviewItem};
use bcode_code_review_models::{
    DraftAnchor, DraftComment, GetReviewWorkspaceRequest, GetReviewWorkspaceResponse,
    ListDraftsRequest, ListDraftsResponse, ListReviewAiExchangesRequest,
    ListReviewAiExchangesResponse, ReviewAiExchangeStatus, ReviewAnchorKind, ReviewLineKind,
    ReviewThreadKind, ReviewWorkspace, SaveDraftRequest, SaveDraftResponse,
    UpdateReviewWorkspaceRequest,
};
use bcode_session_view::follow::SessionViewFollowUpdate;
use bmux_tui::event::{MouseButton, MouseEventKind};
use bmux_tui::geometry::Rect;
use std::sync::Arc;
use std::time::Duration;

pub(super) struct Setup {
    pub(super) scenario: Scenario<ReviewSurface>,
    pub(super) backend: Arc<TestBackend>,
    pub(super) workspace: ReviewWorkspace,
}

pub(super) fn fixture() -> GitFixture {
    GitFixture::generate(FixtureSpec::small())
}

fn workspace(fixture: &GitFixture) -> ReviewWorkspace {
    ReviewWorkspace {
        id: "interaction-review".to_owned(),
        ..ReviewWorkspace::from_target(fixture.root().to_path_buf(), fixture_branch_compare())
    }
}

fn build(
    backend: &Arc<TestBackend>,
    workspace: &ReviewWorkspace,
    motion: Duration,
) -> ReviewSurface {
    ReviewSurface::new(
        backend.services(),
        workspace.repo_root.clone(),
        workspace.sources.clone(),
        workspace.title.clone(),
        ReviewViewConfig::default(),
    )
    .with_scroll_motion(motion)
    .with_workspace(workspace.clone())
}

/// A durable workspace (stored first) opened at `width` x `height`.
pub(super) fn setup(fixture: &GitFixture, width: u16, height: u16) -> Setup {
    let backend = Arc::new(TestBackend::default());
    let workspace = workspace(fixture);
    let _ = backend.call_now(UpdateReviewWorkspaceRequest {
        repo_path: workspace.repo_root.clone(),
        workspace: workspace.clone(),
    });
    let mut scenario = Scenario::new(build(&backend, &workspace, Duration::ZERO), width, height);
    stable(&mut scenario);
    Setup {
        scenario,
        backend,
        workspace,
    }
}

fn drafts(setup: &Setup) -> Vec<DraftComment> {
    let scope = NoteScope::for_workspace(&setup.workspace);
    let response: ListDraftsResponse = setup.backend.call_now(ListDraftsRequest {
        repo_path: scope.repo_path,
        target: scope.target,
        scope: scope.scope,
    });
    response.drafts
}

fn stored_workspace(setup: &Setup) -> ReviewWorkspace {
    let response: GetReviewWorkspaceResponse = setup.backend.call_now(GetReviewWorkspaceRequest {
        repo_path: setup.workspace.repo_root.clone(),
        workspace_id: setup.workspace.id.clone(),
    });
    response.workspace.expect("stored workspace")
}

pub(super) fn cursor(scenario: &Scenario<ReviewSurface>) -> ItemKey {
    scenario.surface().cursor().cloned().expect("cursor")
}

pub(super) fn index_of(scenario: &Scenario<ReviewSurface>, key: &ItemKey) -> usize {
    scenario
        .surface()
        .document()
        .and_then(|doc| doc.index_of(key))
        .expect("indexed key")
}

pub(super) fn move_to(scenario: &mut Scenario<ReviewSurface>, index: usize) -> Screen {
    let current = index_of(scenario, &cursor(scenario));
    let (count, key) = if index >= current {
        (index - current, "j")
    } else {
        (current - index, "k")
    };
    if count > 0 {
        for digit in count.to_string().chars() {
            scenario.key(&digit.to_string());
        }
        scenario.key(key);
    }
    stable(scenario)
}

/// Index of the first item matching `predicate` at or after `from`.
pub(super) fn find(
    scenario: &Scenario<ReviewSurface>,
    from: usize,
    predicate: impl Fn(&ReviewItem) -> bool,
) -> usize {
    let doc = scenario.surface().document().expect("document");
    (from..doc.len())
        .find(|index| doc.item(*index).is_some_and(&predicate))
        .expect("matching item")
}

pub(super) fn added_line(item: &ReviewItem) -> bool {
    use bcode_code_review::document::LineKind;
    match item {
        ReviewItem::Line(line) => line.kind == LineKind::Added,
        ReviewItem::LinePair(pair) => pair
            .new
            .as_ref()
            .is_some_and(|side| side.kind == LineKind::Added),
        _ => false,
    }
}

/// Comment `text` on the cursor item (or reply on a thread) and wait until it is saved.
fn comment(scenario: &mut Scenario<ReviewSurface>, text: &str) -> Screen {
    scenario.key("c");
    scenario.paste(text);
    scenario.key("ctrl+s");
    stable(scenario)
}

fn thread_key(scenario: &Scenario<ReviewSurface>) -> ItemKey {
    let doc = scenario.surface().document().expect("document");
    (0..doc.len())
        .filter_map(|index| doc.item(index))
        .find(|item| matches!(item, ReviewItem::Thread(_)))
        .map(ReviewItem::key)
        .expect("a thread item")
}

fn assert_hits_inside(screen: &Screen, area: Rect) {
    for hit in screen.hits() {
        assert!(
            hit.area.x >= area.x
                && hit.area.y >= area.y
                && hit.area.right() <= area.right()
                && hit.area.bottom() <= area.bottom(),
            "{hit:?} outside {area:?}"
        );
    }
}

#[test]
fn comments_persist_in_the_shared_store_with_the_anchor_head() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 100, 40);
    let index = find(&setup.scenario, 0, added_line);
    move_to(&mut setup.scenario, index);
    let line = cursor(&setup.scenario);
    setup.scenario.key("c");
    assert_eq!(setup.scenario.surface().focus(), Focus::Composer);
    let screen = setup.scenario.render();
    assert!(screen.contains("Comment on"), "{}", screen.text());
    // Typing never runs review actions while the composer has focus.
    setup.scenario.keys(&["q", "j"]);
    assert_eq!(
        setup
            .scenario
            .surface()
            .composer()
            .map(crate::composer::Composer::text),
        Some("qj".to_owned())
    );
    setup.scenario.key("ctrl+a");
    setup.scenario.key("ctrl+k");
    setup.scenario.paste("Looks **wrong** 漢字 🙂");
    setup.scenario.key("ctrl+s");
    let screen = stable(&mut setup.scenario);
    assert!(setup.scenario.surface().composer().is_none());
    assert_eq!(setup.scenario.surface().focus(), Focus::Review);
    assert!(screen.contains("1 comment"), "{}", screen.text());
    assert!(screen.contains("漢字"), "{}", screen.text());
    let saved = drafts(&setup);
    assert_eq!(saved.len(), 1);
    let draft = &saved[0];
    assert_eq!(draft.body, "Looks **wrong** 漢字 🙂");
    let ItemKey::Line { file, line, .. } = line else {
        panic!("cursor on a line: {line:?}");
    };
    assert_eq!(draft.anchor.file_path, file.path);
    assert_eq!(draft.anchor.new_line, Some(line));
    let head = setup.scenario.surface().document().unwrap().head_oid(&file);
    assert!(head.is_some());
    assert_eq!(
        draft.anchor.head_oid, head,
        "the anchor records the reviewed head"
    );
    // The thread identity ignores the head: an anchor that records no head addresses the same
    // thread.
    let mut headless_anchor = draft.anchor.clone();
    headless_anchor.head_oid = None;
    let scope = NoteScope::for_workspace(&setup.workspace);
    let reply: SaveDraftResponse = setup.backend.call_now(SaveDraftRequest {
        repo_path: scope.repo_path,
        target: scope.target,
        scope: scope.scope,
        anchor: headless_anchor,
        body: "without a head".to_owned(),
    });
    assert_eq!(reply.draft.thread_id, draft.thread_id);
    assert_eq!(drafts(&setup).len(), 2);
}

#[test]
fn thread_actions_reply_edit_classify_resolve_and_delete() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 100, 40);
    let index = find(&setup.scenario, 0, added_line);
    move_to(&mut setup.scenario, index);
    comment(&mut setup.scenario, "first");
    let thread = thread_key(&setup.scenario);
    let at = index_of(&setup.scenario, &thread);
    move_to(&mut setup.scenario, at);
    // Reply.
    comment(&mut setup.scenario, "second");
    let bodies = |setup: &Setup| {
        let mut bodies: Vec<String> = drafts(setup).into_iter().map(|draft| draft.body).collect();
        bodies.sort();
        bodies
    };
    assert_eq!(bodies(&setup), ["first", "second"]);
    let at = index_of(&setup.scenario, &thread);
    move_to(&mut setup.scenario, at);
    // Edit the latest comment: the composer starts with its body.
    setup.scenario.key("i");
    assert_eq!(
        setup
            .scenario
            .surface()
            .composer()
            .map(crate::composer::Composer::text),
        Some("second".to_owned())
    );
    setup.scenario.paste(" (edited)");
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    assert_eq!(bodies(&setup), ["first", "second (edited)"]);
    // Kind and severity.
    let at = index_of(&setup.scenario, &thread);
    move_to(&mut setup.scenario, at);
    setup.scenario.keys(&["shift+k", "!", "!"]);
    let screen = stable(&mut setup.scenario);
    assert!(screen.contains("question"), "{}", screen.text());
    let first = drafts(&setup).remove(0);
    assert_eq!(first.thread_kind, ReviewThreadKind::Question);
    assert_eq!(
        first.severity,
        bcode_code_review_models::ReviewThreadSeverity::Warning
    );
    // Resolve, then reopen.
    setup.scenario.key("shift+r");
    let screen = stable(&mut setup.scenario);
    assert!(screen.contains("resolved"), "{}", screen.text());
    assert!(
        drafts(&setup)
            .iter()
            .all(|draft| draft.resolved_at_ms.is_some())
    );
    setup.scenario.key("shift+r");
    stable(&mut setup.scenario);
    assert!(
        drafts(&setup)
            .iter()
            .all(|draft| draft.resolved_at_ms.is_none())
    );
    // Delete the latest, then the first: the thread disappears.
    setup.scenario.key("shift+d");
    stable(&mut setup.scenario);
    assert_eq!(bodies(&setup), ["first"]);
    let at = index_of(&setup.scenario, &thread);
    move_to(&mut setup.scenario, at);
    setup.scenario.key("shift+d");
    stable(&mut setup.scenario);
    assert_eq!(drafts(&setup), [] as [DraftComment; 0]);
    let doc = setup.scenario.surface().document().unwrap();
    assert!(doc.index_of(&thread).is_none());
}

#[test]
fn failed_saves_roll_back_and_report() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 100, 40);
    let index = find(&setup.scenario, 0, added_line);
    move_to(&mut setup.scenario, index);
    setup.backend.set_failing(crate::notes::OP_DRAFT_SAVE, true);
    setup.scenario.key("c");
    setup.scenario.paste("will fail");
    setup.scenario.key("ctrl+s");
    // Optimistic: the thread shows at once, marked as saving.
    let screen = setup.scenario.render();
    assert!(screen.contains("saving"), "{}", screen.text());
    let screen = stable(&mut setup.scenario);
    assert!(setup.scenario.surface().toasts().contains(&"Not saved"));
    assert!(!screen.contains("will fail"), "{}", screen.text());
    assert_eq!(
        setup.scenario.surface().notes().drafts(),
        [] as [DraftComment; 0]
    );
    // An empty comment is discarded without a call.
    setup.scenario.key("c");
    setup.scenario.key("ctrl+s");
    stable(&mut setup.scenario);
    assert_eq!(
        setup.scenario.surface().status(),
        Some("empty comment discarded")
    );
}

#[test]
fn composer_preview_newlines_and_narrow_widths() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 100, 40);
    let index = find(&setup.scenario, 0, added_line);
    move_to(&mut setup.scenario, index);
    setup.scenario.key("c");
    setup.scenario.paste("**bold** text");
    let screen = setup.scenario.render();
    let area = setup
        .scenario
        .surface()
        .item_area(&ItemKey::Composer)
        .unwrap();
    let editing = area.height;
    assert!(screen.contains("**bold**"), "{}", screen.text());
    // Newlines grow the editor; the list re-measures the composer.
    setup
        .scenario
        .keys(&["shift+enter", "x", "shift+enter", "y", "shift+enter", "z"]);
    setup.scenario.render();
    let grown = setup
        .scenario
        .surface()
        .item_area(&ItemKey::Composer)
        .unwrap();
    assert!(grown.height > editing, "{grown:?} vs {editing}");
    // Markdown preview renders the text instead of the editor.
    setup.scenario.key("ctrl+p");
    let screen = setup.scenario.render();
    assert!(screen.contains("bold text"), "{}", screen.text());
    assert!(!screen.contains("**bold**"), "{}", screen.text());
    setup.scenario.key("ctrl+p");
    // The composer's buttons are painted where their hits are, at every width.
    for (width, height) in [(100, 40), (61, 30), (30, 20), (12, 8), (3, 3)] {
        setup.scenario.resize(width, height);
        let screen = setup.scenario.render();
        if let Some(list) = setup.scenario.surface().list_area() {
            assert_hits_inside(&screen, Rect::new(0, 0, width, height));
            for hit in screen
                .hits()
                .iter()
                .filter(|hit| hit.id.as_str().contains(".composer."))
            {
                assert!(list.contains(bmux_tui::geometry::Point::new(hit.area.x, hit.area.y)));
                let text = screen.text_in(hit.area, hit.area.y);
                assert!(!text.trim().is_empty(), "{hit:?} over blank cells");
            }
        }
    }
    setup.scenario.resize(100, 40);
    setup.scenario.render();
    setup.scenario.key("esc");
    stable(&mut setup.scenario);
    assert!(setup.scenario.surface().composer().is_none());
    assert_eq!(drafts(&setup), [] as [DraftComment; 0]);
}

#[test]
fn composer_completes_at_paths_by_key_and_click() {
    let fixture = fixture();
    std::fs::write(fixture.root().join("completion-target.txt"), "target\n").unwrap();
    std::fs::write(fixture.root().join("completion-other.txt"), "other\n").unwrap();
    let mut setup = setup(&fixture, 100, 40);
    let index = find(&setup.scenario, 0, added_line);
    move_to(&mut setup.scenario, index);
    setup.scenario.key("c");
    setup.scenario.paste("see @completion-t");
    let screen = stable(&mut setup.scenario);
    assert!(screen.contains("Suggestions"), "{}", screen.text());
    assert!(
        screen.contains("completion-target.txt"),
        "{}",
        screen.text()
    );
    // Enter accepts the suggestion instead of saving the comment.
    setup.scenario.key("enter");
    let screen = stable(&mut setup.scenario);
    let composer = setup
        .scenario
        .surface()
        .composer()
        .expect("still composing");
    assert_eq!(composer.text(), "see @completion-target.txt");
    assert!(!screen.contains("Suggestions"), "{}", screen.text());
    // Ordinary typing after the accepted path opens nothing.
    setup.scenario.paste(" ordinary");
    let screen = stable(&mut setup.scenario);
    assert!(!screen.contains("Suggestions"), "{}", screen.text());

    // A click on a suggestion accepts it.
    setup.scenario.paste(" @completion-");
    let screen = stable(&mut setup.scenario);
    let other = screen.find("completion-other.txt").expect("suggested");
    setup.scenario.click(other.x, other.y);
    stable(&mut setup.scenario);
    assert_eq!(
        setup.scenario.surface().composer().unwrap().text(),
        "see @completion-target.txt ordinary @completion-other.txt"
    );

    // Escape closes the suggestions first, then the composer.
    setup.scenario.paste(" @completion-t");
    assert!(stable(&mut setup.scenario).contains("Suggestions"));
    setup.scenario.key("esc");
    let screen = stable(&mut setup.scenario);
    assert!(!screen.contains("Suggestions"), "{}", screen.text());
    assert!(setup.scenario.surface().composer().is_some());
    setup.scenario.key("esc");
    stable(&mut setup.scenario);
    assert!(setup.scenario.surface().composer().is_none());
    assert_eq!(drafts(&setup), [] as [DraftComment; 0]);
}

/// A file whose blobs changed since it was marked but whose changes did not, and a renamed
/// file marked under its old path, show as viewed on the first frame of the review, before any
/// hunk is loaded.
#[test]
fn viewed_state_is_right_on_the_first_frame() {
    let fixture = fixture();
    // Learn the files' identities from a first opening.
    let first = setup(&fixture, 100, 40);
    let doc = first.scenario.surface().document().unwrap();
    let mut marks = Vec::new();
    let mut modified = false;
    for file in doc.file_keys() {
        let (identity, patch) = doc.viewed_identity(file).unwrap();
        if let Some(old_path) = &identity.old_path {
            // Marked before the rename, at the same content.
            marks.push(bcode_code_review_models::ViewedMark {
                path: old_path.clone(),
                old_oid: identity.old_oid.clone(),
                new_oid: identity.new_oid.clone(),
                patch_id: patch.clone(),
            });
        } else if !modified && patch.is_some() && identity.old_oid != identity.new_oid {
            modified = true;
            // Marked at other blobs with the same changes (before a rebase).
            marks.push(bcode_code_review_models::ViewedMark {
                path: identity.path.clone(),
                old_oid: Some("0".repeat(40)),
                new_oid: Some("1".repeat(40)),
                patch_id: patch.clone(),
            });
        }
    }
    assert_eq!(
        marks.len(),
        2,
        "a renamed file and a modified file: {marks:?}"
    );
    let mut workspace = first.workspace.clone();
    workspace.viewed_files = marks.iter().map(|mark| mark.path.clone()).collect();
    workspace.viewed_marks = marks;
    let _ = first.backend.call_now(UpdateReviewWorkspaceRequest {
        repo_path: workspace.repo_root.clone(),
        workspace: workspace.clone(),
    });

    let mut scenario = Scenario::new(build(&first.backend, &workspace, Duration::ZERO), 100, 40);
    while scenario.surface().document().is_none() {
        scenario.step_async();
    }
    let doc = scenario.surface().document().unwrap();
    assert_eq!(doc.viewed_progress().viewed, 2);

    // Resetting on any change to the file, only the renamed file (same blobs) stays viewed.
    let surface = ReviewSurface::new(
        first.backend.services(),
        workspace.repo_root.clone(),
        workspace.sources.clone(),
        workspace.title.clone(),
        ReviewViewConfig {
            viewed_reset: bcode_code_review::viewed::ViewedReset::File,
            ..ReviewViewConfig::default()
        },
    )
    .with_scroll_motion(Duration::ZERO)
    .with_workspace(workspace);
    let mut scenario = Scenario::new(surface, 100, 40);
    let screen = stable(&mut scenario);
    let doc = scenario.surface().document().unwrap();
    assert_eq!(doc.viewed_progress().viewed, 1, "{}", screen.text());
}

#[test]
fn drag_across_lines_selects_a_range_and_opens_a_range_composer() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 100, 40);
    let first = find(&setup.scenario, 0, added_line);
    move_to(&mut setup.scenario, first);
    let doc = setup.scenario.surface().document().unwrap();
    let keys: Vec<ItemKey> = (first..first + 3)
        .map(|index| doc.item(index).unwrap().key())
        .collect();
    let rows: Vec<Rect> = keys
        .iter()
        .map(|key| setup.scenario.surface().item_area(key).expect("visible"))
        .collect();
    let x = rows[0].x + rows[0].width / 2;
    setup
        .scenario
        .mouse(MouseEventKind::Down(MouseButton::Left), x, rows[0].y);
    setup
        .scenario
        .mouse(MouseEventKind::Drag(MouseButton::Left), x, rows[2].y);
    let screen = setup.scenario.render();
    let selection = setup.scenario.surface().theme().selection.bg;
    // Every row of the middle line is painted selected (the drag end is the cursor-free end).
    for y in rows[1].y..rows[1].bottom() {
        assert_eq!(screen.style_at(x, y).bg, selection, "row {y}");
    }
    assert_eq!(
        setup.scenario.surface().selection(),
        Some((keys[0].clone(), keys[2].clone()))
    );
    setup
        .scenario
        .mouse(MouseEventKind::Up(MouseButton::Left), x, rows[2].y);
    stable(&mut setup.scenario);
    let composer = setup.scenario.surface().composer().expect("range composer");
    let bcode_code_review::document::AnchorTarget::Lines(lines) = composer.anchor.target else {
        panic!("{:?}", composer.anchor);
    };
    assert!(lines.is_range);
    setup.scenario.paste("range note");
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    let saved = drafts(&setup);
    assert_eq!(saved[0].anchor.kind, ReviewAnchorKind::Range);
    let (start, end) = (
        saved[0].anchor.new_start.unwrap(),
        saved[0].anchor.new_end.unwrap(),
    );
    assert_eq!(end - start, 2, "{:?}", saved[0].anchor);
}

#[test]
fn drag_selection_covers_every_row_of_a_wrapped_line() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 40, 40);
    // Find three consecutive lines whose middle one wraps at this width.
    let doc_len = setup.scenario.surface().document().unwrap().len();
    let mut found = None;
    for start in 0..doc_len.saturating_sub(3) {
        let doc = setup.scenario.surface().document().unwrap();
        let lines = (start..start + 3).all(|index| {
            matches!(
                doc.item(index),
                Some(ReviewItem::Line(_) | ReviewItem::LinePair(_))
            )
        });
        if !lines {
            continue;
        }
        move_to(&mut setup.scenario, start + 1);
        let doc = setup.scenario.surface().document().unwrap();
        let keys: Vec<ItemKey> = (start..start + 3)
            .map(|index| doc.item(index).unwrap().key())
            .collect();
        let areas: Option<Vec<Rect>> = keys
            .iter()
            .map(|key| setup.scenario.surface().item_area(key))
            .collect();
        if let Some(areas) = areas
            && areas[1].height > 1
            && keys[0].file() == keys[2].file()
        {
            found = Some((keys, areas));
            break;
        }
    }
    let (keys, areas) = found.expect("a wrapped line between two lines");
    let x = areas[0].x + 3;
    setup
        .scenario
        .mouse(MouseEventKind::Down(MouseButton::Left), x, areas[0].y);
    // Drag over the wrapped line's continuation row, then onto the line below.
    setup.scenario.mouse(
        MouseEventKind::Drag(MouseButton::Left),
        x,
        areas[1].bottom() - 1,
    );
    assert_eq!(
        setup.scenario.surface().selection(),
        Some((keys[0].clone(), keys[1].clone()))
    );
    setup
        .scenario
        .mouse(MouseEventKind::Drag(MouseButton::Left), x, areas[2].y);
    let screen = setup.scenario.render();
    let selection = setup.scenario.surface().theme().selection.bg;
    for y in areas[1].y..areas[1].bottom() {
        assert_eq!(
            screen.style_at(areas[1].x + 1, y).bg,
            selection,
            "wrapped row {y}"
        );
    }
    setup
        .scenario
        .mouse(MouseEventKind::Up(MouseButton::Left), x, areas[2].y);
    stable(&mut setup.scenario);
    assert!(setup.scenario.surface().composer().is_some());
}

#[test]
fn dragging_past_the_list_edges_extends_to_the_first_uncovered_line() {
    use crate::testing::fixture::LARGE_FILE;
    let fixture = fixture();
    let mut setup = setup(&fixture, 100, 30);
    let header = find(
        &setup.scenario,
        0,
        |item| matches!(item, ReviewItem::FileHeader(header) if header.file.path == LARGE_FILE),
    );
    // Deep inside the large file, so lines of the same file lie above and below the viewport.
    move_to(&mut setup.scenario, header + 40);
    let list = setup.scenario.surface().list_area().expect("list");
    let line_at = |scenario: &Scenario<ReviewSurface>, y: u16| -> ItemKey {
        // The first diff line starting at or below row `y`.
        let surface = scenario.surface();
        let doc = surface.document().unwrap();
        (0..doc.len())
            .map(|index| doc.item(index).unwrap().key())
            .find(|key| {
                matches!(key, ItemKey::Line { .. } | ItemKey::LinePair { .. })
                    && surface.item_area(key).is_some_and(|area| area.y >= y)
            })
            .expect("a line at or below that row")
    };
    let anchor = line_at(&setup.scenario, list.y + list.height / 2);
    let middle = setup.scenario.surface().item_area(&anchor).unwrap().y;
    let x = list.x + list.width / 2;
    setup
        .scenario
        .mouse(MouseEventKind::Down(MouseButton::Left), x, middle);
    for (y, upward) in [(list.y.saturating_sub(1), true), (list.bottom(), false)] {
        setup
            .scenario
            .mouse(MouseEventKind::Drag(MouseButton::Left), x, y);
        let screen = setup.scenario.render();
        let (_, end) = setup.scenario.surface().selection().expect("selection");
        assert_eq!(end.file(), anchor.file());
        let area = setup
            .scenario
            .surface()
            .item_area(&end)
            .expect("the extended end is in the viewport");
        if upward {
            // Not hidden under the pinned file and hunk headers: the line's last row (a
            // wrapped line may start under them) is painted as the line's.
            let row = screen.row_text(area.bottom() - 1);
            assert!(
                !row.contains(LARGE_FILE) && !row.contains("@@"),
                "end under a pinned header: {row:?}\n{}",
                screen.text()
            );
        } else {
            assert_eq!(area.bottom(), list.bottom(), "end on the bottom row");
        }
    }
}

#[test]
fn hover_affordance_follows_visible_rows_and_its_hit_matches_the_paint() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 100, 40);
    let index = find(&setup.scenario, 0, added_line);
    move_to(&mut setup.scenario, index);
    // A line well below the sticky headers, so it stays visible after a wheel step.
    let list = setup.scenario.surface().list_area().unwrap();
    let key = {
        let surface = setup.scenario.surface();
        let doc = surface.document().unwrap();
        (index..doc.len())
            .map(|index| doc.item(index).unwrap().key())
            .find(|key| {
                matches!(key, ItemKey::Line { .. })
                    && surface
                        .item_area(key)
                        .is_some_and(|area| area.y >= list.y + 12)
            })
            .expect("a line mid-viewport")
    };
    let area = setup.scenario.surface().item_area(&key).unwrap();
    setup
        .scenario
        .mouse(MouseEventKind::Move, area.x + area.width / 2, area.y);
    let check = |scenario: &mut Scenario<ReviewSurface>| -> Option<Rect> {
        let screen = scenario.render();
        let hits: Vec<_> = screen
            .hits()
            .iter()
            .filter(|hit| hit.id.as_str().ends_with(".comment"))
            .collect();
        assert!(hits.len() <= 1, "{hits:?}");
        let hit = hits.first()?;
        assert_eq!(hit.area.width, 1);
        assert_eq!(
            screen.text_in(hit.area, hit.area.y),
            "+",
            "{}",
            screen.text()
        );
        Some(hit.area)
    };
    let painted = check(&mut setup.scenario).expect("hover affordance");
    assert_eq!(painted.y, area.y);
    // After scrolling the affordance moves with its row.
    setup
        .scenario
        .mouse(MouseEventKind::ScrollDown, area.x + 2, area.y);
    let after = check(&mut setup.scenario);
    let moved = setup.scenario.surface().item_area(&key);
    assert_eq!(after.map(|hit| hit.y), moved.map(|area| area.y));
    // A resize drops the hover: no stale affordance.
    setup.scenario.resize(90, 35);
    assert!(check(&mut setup.scenario).is_none());
    // Clicking the affordance opens the composer on that line.
    setup.scenario.render();
    let area = setup.scenario.surface().item_area(&key).unwrap();
    setup
        .scenario
        .mouse(MouseEventKind::Move, area.x + area.width / 2, area.y);
    let hit = check(&mut setup.scenario).expect("hover affordance again");
    setup.scenario.click(hit.x, hit.y);
    stable(&mut setup.scenario);
    let composer = setup.scenario.surface().composer().expect("composer");
    let bcode_code_review::document::AnchorTarget::Lines(lines) = composer.anchor.target else {
        panic!("line anchor");
    };
    assert!(
        key.covers_line(lines.end_side, lines.end_line()),
        "{key:?} {lines:?}"
    );
}

#[test]
fn wheel_scrolls_through_keyed_motion() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let workspace = workspace(&fixture);
    let mut scenario = Scenario::new(
        build(&backend, &workspace, Duration::from_millis(150)),
        100,
        40,
    );
    stable(&mut scenario);
    let list = scenario.surface().list_area().unwrap();
    let start = scenario.surface().scroll_offset().unwrap();
    scenario.mouse(MouseEventKind::ScrollDown, list.x + 5, list.y + 5);
    // The motion runs over several frames; polls keep asking for frames meanwhile.
    assert_eq!(
        {
            let ui = std::sync::Arc::new(crate::testing::harness::ScriptedHost::new()).context();
            scenario.surface_mut().poll(&ui)
        },
        Action::Redraw
    );
    scenario.render();
    assert!(scenario.surface().scroll_offset().unwrap() < start + 3);
    std::thread::sleep(Duration::from_millis(200));
    scenario.render();
    assert_eq!(scenario.surface().scroll_offset(), Some(start + 3));
    // Wheel steps during a motion accumulate from its target.
    scenario.mouse(MouseEventKind::ScrollDown, list.x + 5, list.y + 5);
    scenario.mouse(MouseEventKind::ScrollDown, list.x + 5, list.y + 5);
    std::thread::sleep(Duration::from_millis(200));
    scenario.render();
    assert_eq!(scenario.surface().scroll_offset(), Some(start + 9));
}

#[test]
fn thread_actions_are_clickable_where_they_are_painted() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 100, 40);
    let index = find(&setup.scenario, 0, added_line);
    move_to(&mut setup.scenario, index);
    comment(&mut setup.scenario, "click me");
    let thread = thread_key(&setup.scenario);
    let at = index_of(&setup.scenario, &thread);
    let screen = move_to(&mut setup.scenario, at);
    let area = setup.scenario.surface().item_area(&thread).unwrap();
    let (x, y) = (area.y..area.bottom())
        .find_map(|y| {
            let text = screen.text_in(area, y);
            text.find("Resolve")
                .map(|_| (screen.column_of(y, "Resolve").unwrap(), y))
        })
        .expect("Resolve action painted");
    assert!(
        screen
            .hits()
            .iter()
            .any(|hit| hit.area.y == y && hit.area.x == x && hit.id.as_str().contains(".action.")),
        "hit at the painted action"
    );
    setup.scenario.click(x + 1, y);
    stable(&mut setup.scenario);
    assert!(
        drafts(&setup)
            .iter()
            .all(|draft| draft.resolved_at_ms.is_some())
    );
}

#[test]
fn questions_quote_the_same_diff_lines_in_either_layout() {
    let fixture = fixture();
    let diff_block = |width: u16| {
        let mut setup = setup(&fixture, width, 40);
        let index = find(&setup.scenario, 0, added_line);
        move_to(&mut setup.scenario, index);
        // Select the changed line and the two rows after it.
        setup.scenario.key("shift+v");
        setup.scenario.keys(&["j", "j"]);
        setup.scenario.key("a");
        setup.scenario.paste("Why this change?");
        setup.scenario.key("enter");
        stable(&mut setup.scenario);
        let messages = setup.backend.messages();
        let prompt = &messages.first().expect("question sent").1;
        let (_, rest) = prompt.split_once("```diff\n").expect("diff block");
        let (block, _) = rest.split_once("```").expect("diff block end");
        block.to_owned()
    };
    let unified = diff_block(100);
    let side_by_side = diff_block(220);
    assert!(
        unified.lines().any(|line| line.starts_with('+')),
        "{unified}"
    );
    assert_eq!(side_by_side, unified);

    // A side-by-side row pairing a removed with an added line quotes both, removed first.
    let mut setup = setup(&fixture, 220, 40);
    let index = find(&setup.scenario, 0, |item| {
        use bcode_code_review::document::LineKind;
        matches!(item, ReviewItem::LinePair(pair)
            if pair.old.as_ref().is_some_and(|side| side.kind == LineKind::Removed)
                && pair.new.as_ref().is_some_and(|side| side.kind == LineKind::Added))
    });
    move_to(&mut setup.scenario, index);
    setup.scenario.key("shift+v");
    setup.scenario.keys(&["j"]);
    setup.scenario.key("a");
    setup.scenario.paste("Why?");
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    let prompt = setup
        .backend
        .messages()
        .first()
        .expect("question sent")
        .1
        .clone();
    let (_, rest) = prompt.split_once("```diff\n").expect("diff block");
    let (block, _) = rest.split_once("```").expect("diff block end");
    let lines: Vec<&str> = block.lines().collect();
    let last_removed = lines.iter().rposition(|line| line.starts_with('-'));
    let first_added = lines.iter().position(|line| line.starts_with('+'));
    assert!(
        matches!((last_removed, first_added), (Some(removed), Some(added)) if removed < added),
        "{block}"
    );
}

#[test]
fn a_failed_question_replaces_the_asking_status() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 100, 40);
    setup
        .backend
        .fail_sessions(Some("Bcode is not running; start bcode and try again"));
    let index = find(&setup.scenario, 0, added_line);
    move_to(&mut setup.scenario, index);
    setup.scenario.key("a");
    setup.scenario.paste("Why this change?");
    setup.scenario.key("enter");
    let screen = stable(&mut setup.scenario);
    assert_eq!(
        setup.scenario.surface().status(),
        Some("could not ask Bcode")
    );
    assert!(!screen.contains("asking Bcode"), "{}", screen.text());
    assert!(screen.contains("Bcode is not running"), "{}", screen.text());
}

#[test]
fn asking_bcode_stays_in_the_review_and_opening_the_session_restores_position() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 100, 40);
    let index = find(&setup.scenario, 0, added_line);
    move_to(&mut setup.scenario, index);
    setup.scenario.key("a");
    setup.scenario.paste("Why this change?");
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    // Asking never hands the terminal to the session.
    assert!(
        !setup
            .scenario
            .actions()
            .iter()
            .any(|action| matches!(action, Action::OpenSession { .. })),
        "{:?}",
        setup.scenario.actions()
    );
    assert!(matches!(
        cursor(&setup.scenario),
        ItemKey::AgentThread { .. }
    ));
    let sessions = setup.backend.sessions();
    assert_eq!(sessions.len(), 1);
    let session = sessions[0].0.clone();
    assert!(sessions[0].1.starts_with("Review: "), "{:?}", sessions[0]);
    let messages = setup.backend.messages();
    assert!(messages[0].1.contains("Why this change?"));
    assert!(messages[0].1.contains("```diff"));
    let scope = NoteScope::for_workspace(&setup.workspace);
    let exchanges: ListReviewAiExchangesResponse =
        setup.backend.call_now(ListReviewAiExchangesRequest {
            repo_path: scope.repo_path,
            target: scope.target,
            scope: scope.scope,
        });
    assert_eq!(exchanges.exchanges.len(), 1);
    assert_eq!(
        exchanges.exchanges[0].status,
        ReviewAiExchangeStatus::Linked
    );
    assert_eq!(
        exchanges.exchanges[0].session_id.as_deref(),
        Some(session.as_str())
    );
    // `o` on the card opens the session in Bcode; returning restores the exact position even
    // when the surface moved meanwhile.
    let before = (cursor(&setup.scenario), setup.scenario.surface().top_key());
    setup.scenario.key("o");
    let opened = setup
        .scenario
        .actions()
        .iter()
        .find_map(|action| match action {
            Action::OpenSession { session_id } => Some(session_id.to_string()),
            _ => None,
        });
    assert_eq!(opened.as_deref(), Some(session.as_str()));
    setup.scenario.keys(&["G"]);
    stable(&mut setup.scenario);
    assert_ne!(cursor(&setup.scenario), before.0);
    let id = session.parse().unwrap();
    setup
        .scenario
        .surface_mut()
        .session_navigation_finished(id, Ok(()));
    stable(&mut setup.scenario);
    assert_eq!(cursor(&setup.scenario), before.0);
    assert_eq!(setup.scenario.surface().top_key(), before.1);
    assert_eq!(
        setup.scenario.surface().status(),
        Some("returned from linked Bcode session")
    );
    // Live status from the host's session view.
    let screen = stable(&mut setup.scenario);
    assert!(screen.contains("Bcode"), "{}", screen.text());
    let sender = setup
        .scenario
        .host()
        .session_view(&session)
        .expect("subscribed");
    sender
        .try_send(SessionViewFollowUpdate::Snapshot(Box::new(
            bcode_session_view_models::SessionViewSnapshot::empty(),
        )))
        .unwrap();
    let screen = stable(&mut setup.scenario);
    assert!(screen.contains("working"), "{}", screen.text());
    sender
        .try_send(SessionViewFollowUpdate::Disconnected {
            message: "daemon went away".to_owned(),
        })
        .unwrap();
    let screen = stable(&mut setup.scenario);
    assert!(screen.contains("stream stopped"), "{}", screen.text());
    assert!(screen.contains("Retry"), "{}", screen.text());
}

#[test]
fn threads_panel_filters_and_jumps_to_threads() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 170, 40);
    let first = find(&setup.scenario, 0, added_line);
    move_to(&mut setup.scenario, first);
    comment(&mut setup.scenario, "open note");
    let after = index_of(&setup.scenario, &thread_key(&setup.scenario)) + 1;
    let second = find(&setup.scenario, after, added_line);
    move_to(&mut setup.scenario, second);
    comment(&mut setup.scenario, "to resolve");
    let threads: Vec<ItemKey> = {
        let doc = setup.scenario.surface().document().unwrap();
        (0..doc.len())
            .filter_map(|index| doc.item(index))
            .filter(|item| matches!(item, ReviewItem::Thread(_)))
            .map(ReviewItem::key)
            .collect()
    };
    assert_eq!(threads.len(), 2);
    let at = index_of(&setup.scenario, &threads[1]);
    move_to(&mut setup.scenario, at);
    setup.scenario.key("shift+r");
    setup.scenario.key("shift+c");
    setup.scenario.paste("review-level");
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    // Thread motions walk the document's threads.
    setup.scenario.key("g");
    setup.scenario.key(")");
    stable(&mut setup.scenario);
    assert!(matches!(cursor(&setup.scenario), ItemKey::Thread { .. }));
    setup.scenario.key("shift+t");
    let screen = stable(&mut setup.scenario);
    let pane = setup
        .scenario
        .surface()
        .body_area(THREADS_PANE_ID)
        .expect("threads panel");
    let rows = |screen: &Screen| -> Vec<String> {
        (pane.y + 1..pane.bottom() - 1)
            .map(|y| screen.text_in(pane, y))
            .filter(|row| row.contains("open") || row.contains("resolved"))
            .collect()
    };
    assert_eq!(rows(&screen).len(), 3, "{}", screen.text());
    while setup.scenario.surface().focus() != Focus::Threads {
        setup.scenario.key("tab");
    }
    setup.scenario.key("right");
    let screen = stable(&mut setup.scenario);
    assert_eq!(rows(&screen).len(), 2, "open only: {}", screen.text());
    setup.scenario.keys(&["right", "right"]);
    let screen = stable(&mut setup.scenario);
    let general = rows(&screen);
    assert_eq!(general.len(), 1, "general only: {}", screen.text());
    assert!(general[0].contains("review"), "{general:?}");
    setup.scenario.key("down");
    stable(&mut setup.scenario);
    assert!(
        matches!(cursor(&setup.scenario), ItemKey::Thread { .. }),
        "selecting a row jumps to its thread"
    );
}

#[test]
fn viewed_marks_persist_and_advance_to_the_next_unviewed_file() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 100, 40);
    let first_file = setup
        .scenario
        .surface()
        .document()
        .unwrap()
        .file_keys()
        .next()
        .cloned()
        .unwrap();
    setup.scenario.key("v");
    let screen = stable(&mut setup.scenario);
    assert!(screen.contains("viewed 1/"), "{}", screen.text());
    let doc = setup.scenario.surface().document().unwrap();
    assert!(doc.viewed_state(&first_file).unwrap().is_viewed());
    let ItemKey::FileHeader(next) = cursor(&setup.scenario) else {
        panic!(
            "cursor on the next file header: {:?}",
            cursor(&setup.scenario)
        );
    };
    assert_ne!(next, first_file);
    let stored = stored_workspace(&setup);
    assert!(stored.viewed_files.contains(&first_file.path));
    assert!(
        stored
            .viewed_marks
            .iter()
            .any(|mark| mark.path == first_file.path && mark.has_identity())
    );
    // `v` on a viewed file unmarks it.
    setup.scenario.key("p");
    setup.scenario.key("v");
    stable(&mut setup.scenario);
    let doc = setup.scenario.surface().document().unwrap();
    assert!(!doc.viewed_state(&first_file).unwrap().is_viewed());
    assert!(
        !stored_workspace(&setup)
            .viewed_files
            .contains(&first_file.path)
    );
}

#[test]
fn search_is_incremental_reports_unloaded_files_and_steps_with_n() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 100, 40);
    setup.scenario.key("/");
    assert_eq!(setup.scenario.surface().focus(), Focus::Search);
    // Every fixture line calls `compute`.
    let word = "compute";
    for character in word.chars() {
        setup.scenario.paste(&character.to_string());
    }
    let screen = stable(&mut setup.scenario);
    let status = setup.scenario.surface().search_status().unwrap();
    assert!(status.starts_with("1/"), "{status}");
    assert!(
        screen.row_text(screen.height() - 1).starts_with('/'),
        "the search bar shows"
    );
    let first = cursor(&setup.scenario);
    // The match is highlighted where it is painted.
    let theme_match = setup.scenario.surface().theme().search_current.bg;
    let area = setup.scenario.surface().item_area(&first).unwrap();
    let highlighted = (area.x..area.right()).any(|x| screen.style_at(x, area.y).bg == theme_match);
    assert!(highlighted, "{}", screen.text());
    setup.scenario.key("enter");
    assert_eq!(setup.scenario.surface().focus(), Focus::Review);
    setup.scenario.key("n");
    stable(&mut setup.scenario);
    let second = cursor(&setup.scenario);
    assert!(
        setup
            .scenario
            .surface()
            .search_status()
            .unwrap()
            .starts_with("2/")
    );
    assert_ne!(first, second);
    setup.scenario.key("shift+n");
    stable(&mut setup.scenario);
    assert_eq!(cursor(&setup.scenario), first);
    setup.scenario.key("esc");
    assert!(setup.scenario.surface().search_status().is_none());

    // With almost nothing resident, unloaded files are reported as pending.
    let backend = Arc::new(TestBackend::default());
    let workspace = workspace(&fixture);
    let config = DocumentConfig {
        prefetch_rows: 0,
        protected_rows: 0,
        residency_budget_bytes: 1,
        ..DocumentConfig::default()
    };
    let mut scenario = Scenario::new(
        build(&backend, &workspace, Duration::ZERO).with_document_config(config),
        100,
        12,
    );
    stable(&mut scenario);
    scenario.key("/");
    scenario.paste("fn");
    stable(&mut scenario);
    let status = scenario.surface().search_status().unwrap();
    assert!(status.contains("unloaded file"), "{status}");
}

#[test]
fn search_wraps_around_at_both_ends() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 100, 40);
    let current = |scenario: &Scenario<ReviewSurface>| {
        let status = scenario.surface().search_status().expect("search");
        let (current, _) = status.split_once('/').expect("position");
        current.parse::<usize>().expect("current")
    };
    setup.scenario.key("/");
    setup.scenario.paste("compute");
    stable(&mut setup.scenario);
    setup.scenario.key("enter");
    let first = cursor(&setup.scenario);
    assert_eq!(current(&setup.scenario), 1);

    // Stepping back from the first match lands below it, on the last.
    setup.scenario.key("shift+n");
    stable(&mut setup.scenario);
    assert!(current(&setup.scenario) > 1);
    assert!(
        index_of(&setup.scenario, &cursor(&setup.scenario)) > index_of(&setup.scenario, &first)
    );

    // Stepping forward past the last match returns to the first.
    let mut previous = index_of(&setup.scenario, &cursor(&setup.scenario));
    let mut wrapped = false;
    for _ in 0..2_000 {
        setup.scenario.key("n");
        stable(&mut setup.scenario);
        let index = index_of(&setup.scenario, &cursor(&setup.scenario));
        if index < previous {
            wrapped = true;
            break;
        }
        previous = index;
    }
    assert!(wrapped, "n never wrapped to the first match");
    assert_eq!(cursor(&setup.scenario), first);
    assert_eq!(current(&setup.scenario), 1);

    // A search started below the last match finds the first one.
    setup.scenario.key("esc");
    setup.scenario.key("shift+g");
    stable(&mut setup.scenario);
    assert!(
        index_of(&setup.scenario, &cursor(&setup.scenario)) > index_of(&setup.scenario, &first)
    );
    setup.scenario.key("/");
    setup.scenario.paste("compute");
    stable(&mut setup.scenario);
    assert_eq!(current(&setup.scenario), 1);
    assert_eq!(cursor(&setup.scenario), first);
}

#[test]
fn palette_and_help_come_from_the_keymap_and_run_actions() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 100, 40);
    setup.scenario.key(":");
    let screen = setup.scenario.render();
    let overlay = setup.scenario.surface().overlay().expect("palette");
    let labels = overlay.shown_labels();
    for action in crate::keymap::actions_for(crate::SCOPE) {
        assert!(
            labels.contains(&action.description.as_str()),
            "{}",
            action.id
        );
    }
    assert!(screen.contains("Commands"), "{}", screen.text());
    // Keys appear next to their actions.
    let row = screen.find("Move down").expect("first action shown");
    assert!(
        screen.row_text(row.y).contains("j down"),
        "{}",
        screen.text()
    );
    for character in "viewed".chars() {
        setup.scenario.key(&character.to_string());
    }
    let labels = setup
        .scenario
        .surface()
        .overlay()
        .unwrap()
        .shown_labels()
        .len();
    assert!(labels < crate::keymap::actions_for(crate::SCOPE).len());
    setup.scenario.key("enter");
    let screen = stable(&mut setup.scenario);
    assert!(setup.scenario.surface().overlay().is_none());
    assert!(screen.contains("viewed 1/"), "{}", screen.text());
    // Help lists every context, scrolls, and closes with `?`.
    setup.scenario.key("?");
    let screen = setup.scenario.render();
    let help = setup.scenario.surface().overlay().unwrap().shown_labels();
    assert!(help.contains(&"Comment composer"));
    assert!(help.contains(&"Save the comment"));
    assert!(screen.contains("Keys"), "{}", screen.text());
    assert!(help.contains(&"File panel") && help.contains(&"Threads panel"));
    assert!(help.contains(&"Next threads filter"));
    setup.scenario.key("end");
    let scrolled = setup.scenario.render();
    assert!(
        scrolled.contains("Previous threads filter"),
        "{}",
        scrolled.text()
    );
    setup.scenario.key("?");
    assert!(setup.scenario.surface().overlay().is_none());
    // ctrl+p also opens the palette; a click outside closes it.
    setup.scenario.key("ctrl+p");
    setup.scenario.render();
    setup.scenario.click(0, 0);
    assert!(setup.scenario.surface().overlay().is_none());
}

#[test]
fn presentation_persists_on_exit_and_restores_on_reopen() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 100, 40);
    setup.scenario.key("shift+s");
    stable(&mut setup.scenario);
    let gap = find(&setup.scenario, 0, |item| {
        matches!(item, ReviewItem::Gap(_))
    });
    move_to(&mut setup.scenario, gap);
    setup.scenario.key("x");
    stable(&mut setup.scenario);
    setup.scenario.key("ctrl+d");
    stable(&mut setup.scenario);
    // A last scroll right before exiting, not yet saved by the periodic check.
    setup.scenario.key("ctrl+d");
    setup.scenario.render();
    let top = setup.scenario.surface().top_key().unwrap();
    let layout = setup.scenario.surface().document().unwrap().layout();
    let expansions = setup.scenario.surface().document().unwrap().expansions();
    assert_ne!(expansions, []);
    let closed = |scenario: &Scenario<ReviewSurface>| {
        scenario
            .actions()
            .iter()
            .any(|action| matches!(action, Action::Close { .. }))
    };
    // The surface stays open until the exit save finished: the host may end the process as
    // soon as it closes.
    setup.scenario.key("q");
    assert!(!closed(&setup.scenario));
    assert!(setup.scenario.render().contains("saving"));
    setup.scenario.settle();
    assert!(closed(&setup.scenario));
    let stored = stored_workspace(&setup);
    let state = stored.presentation.clone().expect("presentation saved");
    assert!(state.layout_mode.is_some());
    assert_eq!(state.expansions, expansions);
    assert!(state.scroll_anchor.is_some());
    // Reopen from the stored workspace.
    let mut scenario = Scenario::new(build(&setup.backend, &stored, Duration::ZERO), 100, 40);
    stable(&mut scenario);
    let doc = scenario.surface().document().unwrap();
    assert_eq!(doc.layout(), layout);
    assert_eq!(doc.expansions(), expansions);
    assert_eq!(scenario.surface().top_key(), Some(top));
    assert_eq!(layout, LayoutMode::SideBySide);
}

#[test]
fn anchors_are_outdated_only_when_their_content_changed() {
    use bcode_code_review::document::{AnchorPlacement, OutdatedReason};
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let workspace = workspace(&fixture);
    let mut scenario = Scenario::new(build(&backend, &workspace, Duration::ZERO), 100, 40);
    stable(&mut scenario);
    let mut lines = vec![find(&scenario, 0, added_line)];
    for _ in 0..3 {
        let last = *lines.last().unwrap();
        lines.push(find(&scenario, last + 1, added_line));
    }
    let doc = scenario.surface().document().unwrap();
    let anchor = |index: usize| -> DraftAnchor {
        doc.anchor_for(&doc.item(index).unwrap().key())
            .unwrap()
            .to_draft_anchor()
    };
    let other = Some("0".repeat(40));
    // Made at another head on the same file content: still exact.
    let mut same_content = anchor(lines[0]);
    same_content.head_oid.clone_from(&other);
    // Made on other content of the file: outdated.
    let mut changed = anchor(lines[1]);
    changed.blob_oid.clone_from(&other);
    // Made at another head before blobs were recorded: outdated by the head.
    let mut legacy = anchor(lines[2]);
    legacy.head_oid.clone_from(&other);
    legacy.blob_oid = None;
    let current = anchor(lines[3]);
    assert!(current.head_oid.is_some() && current.blob_oid.is_some());
    assert_eq!(current.line_kind, ReviewLineKind::Added);
    let scope = NoteScope::for_workspace(&workspace);
    for (anchor, body) in [
        (same_content, "same content"),
        (changed, "changed content"),
        (legacy, "legacy"),
        (current, "made now"),
    ] {
        let _: SaveDraftResponse = backend.call_now(SaveDraftRequest {
            repo_path: scope.repo_path.clone(),
            target: scope.target.clone(),
            scope: scope.scope.clone(),
            anchor,
            body: body.to_owned(),
        });
    }
    // Reopen so the annotations load.
    let mut scenario = Scenario::new(build(&backend, &workspace, Duration::ZERO), 100, 40);
    stable(&mut scenario);
    let doc = scenario.surface().document().unwrap();
    let mut placements: Vec<_> = (0..doc.len())
        .filter_map(|index| match doc.item(index) {
            Some(ReviewItem::Thread(thread)) => Some(thread.placement),
            _ => None,
        })
        .collect();
    placements.sort_by_key(|placement| format!("{placement:?}"));
    assert_eq!(
        placements,
        [
            AnchorPlacement::Line,
            AnchorPlacement::Line,
            AnchorPlacement::Outdated(OutdatedReason::ContentChanged),
            AnchorPlacement::Outdated(OutdatedReason::RevisionChanged),
        ]
    );
}

#[test]
fn diff_text_registers_selection_fragments_matching_the_painted_cells() {
    use crate::rows::RowKey;
    use crate::testing::fixture::UNICODE_FILE;
    for width in [100, 170, 40] {
        let fixture = fixture();
        let mut setup = setup(&fixture, width, 30);
        let header = find(
            &setup.scenario,
            0,
            |item| matches!(item, ReviewItem::FileHeader(header) if header.file.path == UNICODE_FILE),
        );
        // The line with tabs, with the file's other unicode lines around it.
        let tabbed = |text: Option<&str>| text.is_some_and(|text| text.contains('\t'));
        let tab_line = find(&setup.scenario, header, |item| match item {
            ReviewItem::Line(line) => tabbed(line.content.text()),
            ReviewItem::LinePair(pair) => {
                tabbed(pair.new.as_ref().and_then(|side| side.content.text()))
            }
            _ => false,
        });
        move_to(&mut setup.scenario, tab_line);
        let screen = stable(&mut setup.scenario);
        let doc = setup.scenario.surface().document().unwrap();
        let mut texts = std::collections::BTreeMap::new();
        for index in 0..doc.len() {
            let item = doc.item(index).unwrap();
            let key = RowKey(item.key()).to_string();
            match item {
                ReviewItem::Line(line) => {
                    if let Some(text) = line.content.text() {
                        texts.insert(format!("{key}.0"), text.to_owned());
                        texts.insert(format!("{key}.1"), text.to_owned());
                    }
                }
                ReviewItem::LinePair(pair) => {
                    for (side, cell) in [(0, &pair.old), (1, &pair.new)] {
                        if let Some(text) = cell.as_ref().and_then(|cell| cell.content.text()) {
                            texts.insert(format!("{key}.{side}"), text.to_owned());
                        }
                    }
                }
                _ => {}
            }
        }
        let scene = screen.selection();
        assert!(scene.validate().is_ok(), "{:?}", scene.validate());
        let fragments: Vec<_> = scene
            .fragments()
            .iter()
            .filter(|fragment| fragment.scope_id.as_str() == crate::rows::SELECTION_SCOPE)
            .collect();
        assert!(
            fragments.len() > 10,
            "{width}: {} fragments",
            fragments.len()
        );
        let list = setup.scenario.surface().list_area().unwrap();
        let mut tabs = 0;
        for fragment in fragments {
            assert!(list.contains(bmux_tui::geometry::Point::new(
                fragment.area.x,
                fragment.area.y
            )));
            let text = &texts[fragment.content_id.as_str()];
            let source = &text[fragment.source_range.clone()];
            let painted = screen.text_in(fragment.area, fragment.area.y);
            if source == "\t" {
                // Tabs expand to the configured stops (four cells by default) as spaces, and
                // the fragment covers the whole expansion.
                tabs += 1;
                assert!((1..=4).contains(&fragment.area.width), "{fragment:?}");
                for x in fragment.area.x..fragment.area.right() {
                    let cell = screen.cell(x, fragment.area.y).expect("painted cell");
                    assert_eq!(cell.symbol, " ", "{width}: {fragment:?}");
                }
            } else if fragment.area.width > 0 && !source.chars().any(char::is_control) {
                assert_eq!(
                    painted.trim_end(),
                    source.trim_end(),
                    "{width}: {fragment:?}"
                );
                // Fragments cover exactly the cells their graphemes paint (wide ones two).
                assert_eq!(
                    usize::from(fragment.area.width),
                    bmux_tui::text_width::display_width(source),
                    "{width}: {fragment:?}"
                );
            }
        }
        assert!(tabs > 0, "{width}: the tab line is on screen");
    }
}
