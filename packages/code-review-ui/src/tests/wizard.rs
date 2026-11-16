//! The new-review wizard through the app: every source type creates a durable review
//! from pickers and opens it, composite reviews collect several sources, and the pull request
//! and stack types explain a missing or failing provider while the other types keep working.

use super::app::{fixture, listed, open, settled};
use crate::app::{ReviewApp, RouteName};
use crate::entry::{Entry, NO_PROVIDER};
use crate::testing::backend::TestBackend;
use crate::testing::fake_provider::{FAKE_PROVIDER, FAKE_REPOSITORY, FakeProvider};
use crate::testing::fixture::{BASE_BRANCH, GitFixture, HEAD_BRANCH, UNICODE_FILE};
use crate::testing::harness::{Scenario, Screen};
use bcode_code_review_models::{
    PathSelector, REVIEW_SOURCE_ERROR_UNAUTHENTICATED, ReviewSourceKind, ReviewWorkspace,
};
use bmux_tui::event::{MouseButton, MouseEventKind};
use std::sync::Arc;

/// Open home and start the wizard.
fn wizard(fixture: &GitFixture, backend: &Arc<TestBackend>) -> Scenario<ReviewApp> {
    let (mut scenario, _) = open(fixture, backend, Entry::Home, 140, 40);
    scenario.key("n");
    settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::NewReview);
    scenario
}

/// Pick the source type at `index` of step 1.
fn pick_type(scenario: &mut Scenario<ReviewApp>, index: usize) -> Screen {
    for _ in 0..index {
        scenario.key("down");
    }
    scenario.key("enter");
    settled(scenario)
}

fn step(scenario: &Scenario<ReviewApp>) -> usize {
    scenario.surface().wizard().expect("wizard").step_number()
}

/// Create from step 3 and return the workspace the review opened.
fn create(
    scenario: &mut Scenario<ReviewApp>,
    backend: &TestBackend,
    fixture: &GitFixture,
) -> ReviewWorkspace {
    assert_eq!(step(scenario), 3);
    scenario.key("enter");
    settled(scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Review);
    let review = scenario.surface().review().unwrap();
    assert!(!review.is_transient(), "the wizard creates durable reviews");
    let id = review.workspace().unwrap().id.clone();
    listed(backend, fixture)
        .into_iter()
        .find(|workspace| workspace.id == id)
        .expect("created workspace listed")
}

fn kinds(workspace: &ReviewWorkspace) -> Vec<ReviewSourceKind> {
    workspace
        .sources
        .iter()
        .map(|source| source.kind.clone())
        .collect()
}

#[test]
fn working_tree_with_counts_creates_and_opens_a_review() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    let screen = scenario.render();
    for text in [
        "Source type",
        "Details",
        "Title & create",
        "Working tree",
        "Pull request",
    ] {
        assert!(screen.contains(text), "{text}:\n{}", screen.text());
    }
    let screen = pick_type(&mut scenario, 0);
    assert_eq!(step(&scenario), 2);
    assert!(screen.contains("Unstaged changes"));
    assert!(
        screen.contains("files · +"),
        "counts shown:\n{}",
        screen.text()
    );
    scenario.key("down");
    scenario.key("enter");
    let screen = settled(&mut scenario);
    assert_eq!(step(&scenario), 3);
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().query(),
        "Staged changes"
    );
    assert!(screen.contains("Create review"));
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(created.title, "Staged changes");
    assert_eq!(kinds(&created), [ReviewSourceKind::IndexStaged]);
    // Leaving the review lands on home, which lists it.
    scenario.key("q");
    settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Home);
    assert!(
        scenario
            .surface()
            .home()
            .unwrap()
            .row_labels()
            .contains(&"Staged changes".to_owned())
    );
}

#[test]
fn current_branch_prefills_the_detected_base_and_compare_picks_two_references() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 1);
    let current = scenario.surface().wizard().unwrap();
    assert_eq!(current.picker().query(), "origin/main");
    assert_eq!(current.picker().focused(), Some("origin/main"));
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(created.title, HEAD_BRANCH);
    // The detected base stays automatic.
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::CurrentBranch { base: None }]
    );

    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 2);
    scenario.paste(BASE_BRANCH);
    settled(&mut scenario);
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().focused(),
        Some(BASE_BRANCH)
    );
    scenario.key("enter");
    let screen = settled(&mut scenario);
    assert!(screen.contains(&format!("base {BASE_BRANCH}")));
    scenario.paste(HEAD_BRANCH);
    settled(&mut scenario);
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::BranchCompare {
            base_branch: BASE_BRANCH.to_owned(),
            head_branch: HEAD_BRANCH.to_owned(),
            merge_base: true,
        }]
    );
}

#[test]
fn typed_revisions_work_when_no_reference_matches() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 2);
    scenario.paste("HEAD~1");
    let screen = settled(&mut scenario);
    assert!(
        screen.contains("No matching references"),
        "{}",
        screen.text()
    );
    scenario.key("enter");
    settled(&mut scenario);
    scenario.paste(HEAD_BRANCH);
    settled(&mut scenario);
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::BranchCompare {
            base_branch: "HEAD~1".to_owned(),
            head_branch: HEAD_BRANCH.to_owned(),
            merge_base: true,
        }]
    );
    // The review opened against the typed base.
    assert!(
        scenario
            .surface()
            .review()
            .unwrap()
            .document()
            .is_some_and(|doc| !doc.is_empty())
    );
}

#[test]
fn commits_mark_single_ranges_and_selections() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let head = fixture.git(&["rev-parse", "HEAD"]).trim().to_owned();
    let base = fixture.git(&["rev-parse", BASE_BRANCH]).trim().to_owned();
    // The focused commit alone.
    let mut scenario = wizard(&fixture, &backend);
    let screen = pick_type(&mut scenario, 3);
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().focused(),
        Some(head.as_str())
    );
    assert!(screen.contains(&head[..7]));
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::Commit { rev: head.clone() }]
    );

    // A contiguous range: mark the newest, move down, mark the range.
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 3);
    scenario.key("tab");
    scenario.key("down");
    scenario.key("shift+tab");
    settled(&mut scenario);
    assert_eq!(scenario.surface().wizard().unwrap().marked().len(), 2);
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::Commits {
            revs: vec![base, head],
        }]
    );

    // A search narrows the log; unmarking with tab.
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 3);
    scenario.key("tab");
    scenario.key("tab");
    assert_eq!(
        scenario.surface().wizard().unwrap().marked(),
        Vec::<String>::new()
    );
    scenario.paste("no commit says this");
    let screen = settled(&mut scenario);
    assert!(screen.contains("No matching commits"), "{}", screen.text());
}

/// The tree picker of the wizard's paths step.
fn paths(scenario: &Scenario<ReviewApp>) -> &crate::path_picker::PathPicker {
    scenario
        .surface()
        .wizard()
        .and_then(crate::wizard::Wizard::path_picker)
        .expect("tree picker")
}

/// Move the tree picker's focus down until it is on `path`.
fn focus_path(scenario: &mut Scenario<ReviewApp>, path: &str) {
    for _ in 0..200 {
        if paths(scenario).focused().as_deref() == Some(path) {
            return;
        }
        scenario.key("down");
        settled(scenario);
    }
    panic!("{path} is not in the tree:\n{}", settled(scenario).text());
}

/// The fixture with a folder of CJK, emoji, and combining-mark names (untracked), and an
/// ignored folder.
fn paths_fixture() -> GitFixture {
    let fixture = fixture();
    for (path, text) in [
        ("名前/説明書.txt", "説明\n"),
        ("名前/🎉 party.txt", "party\n"),
        ("名前/cafe\u{301}.txt", "cafe\n"),
        ("ignored-dir/secret.txt", "secret\n"),
        (".gitignore", "ignored-dir/\n"),
    ] {
        let full = fixture.root().join(path);
        std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        std::fs::write(full, text).expect("write");
    }
    fixture
}

#[test]
fn files_pick_line_ranges_and_globs_from_the_filter() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 6);
    scenario.paste(&format!("{UNICODE_FILE}:2-4"));
    let screen = settled(&mut scenario);
    assert!(
        screen.contains(&format!("Add lines 2–4 of {UNICODE_FILE}")),
        "{}",
        screen.text()
    );
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::FileRange {
            path: UNICODE_FILE.to_owned(),
            start: 2,
            end: 4,
        }]
    );

    // A glob filter offers its pattern, counted now.
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 6);
    scenario.paste("docs/*.txt");
    let screen = settled(&mut scenario);
    let row = screen.find("Select pattern").expect("pattern row");
    assert!(screen.row_text(row.y).contains("now"), "{}", screen.text());
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::FileSet {
            include: vec![PathSelector::Glob {
                pattern: "docs/*.txt".to_owned()
            }],
            exclude: Vec::new(),
        }]
    );
}

#[test]
fn the_tree_selects_folders_files_and_exclusions_with_tri_state_checks() {
    let fixture = paths_fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 6);
    // Untracked folders are listed; ignored ones are not until shown.
    let screen = settled(&mut scenario);
    assert!(screen.contains("名前/"), "{}", screen.text());
    assert!(!screen.contains("ignored-dir"), "{}", screen.text());
    // Select docs/, expand it, and unselect its first file: the folder is partly selected.
    focus_path(&mut scenario, "docs/");
    scenario.key("space");
    scenario.key("l");
    settled(&mut scenario);
    scenario.key("l");
    let screen = settled(&mut scenario);
    let child = paths(&scenario).focused().expect("first child");
    assert!(child.starts_with("docs/"), "{child}");
    assert!(screen.contains("[x] docs/"), "{}", screen.text());
    scenario.key("space");
    let screen = settled(&mut scenario);
    assert!(screen.contains("[-] docs/"), "{}", screen.text());
    assert_eq!(
        paths(&scenario).exclude(),
        [PathSelector::File {
            path: child.clone()
        }]
    );
    // `h` goes back to the folder and collapses it.
    scenario.keys(&["h", "h"]);
    settled(&mut scenario);
    assert_eq!(paths(&scenario).focused().as_deref(), Some("docs/"));
    // Select one file of the untracked folder too; the selection counts files now.
    focus_path(&mut scenario, "名前/");
    scenario.key("l");
    settled(&mut scenario);
    scenario.key("l");
    settled(&mut scenario);
    let file = paths(&scenario).focused().expect("file");
    scenario.key("space");
    let screen = settled(&mut scenario);
    assert!(screen.contains("[-] 名前/"), "{}", screen.text());
    assert!(screen.contains("Selected: 3 items"), "{}", screen.text());
    // docs/ has one file, excluded; the untracked file is the one counted.
    assert_eq!(paths(&scenario).total(), Some(1));
    // The selection list removes entries.
    scenario.key("alt+l");
    let screen = settled(&mut scenario);
    assert!(paths(&scenario).showing_selection());
    assert!(
        screen.contains(&format!("{} docs/", "[x]")),
        "{}",
        screen.text()
    );
    scenario.key("alt+l");
    settled(&mut scenario);
    scenario.key("enter");
    let screen = settled(&mut scenario);
    // Step 3 says how many files the source selected.
    assert!(screen.contains("1 file now"), "{}", screen.text());
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::FileSet {
            include: vec![
                PathSelector::Folder {
                    path: "docs".to_owned()
                },
                PathSelector::File { path: file },
            ],
            exclude: vec![PathSelector::File { path: child }],
        }]
    );
}

#[test]
fn the_tree_shows_ignored_files_on_request_and_removes_selected_items() {
    let fixture = paths_fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 6);
    scenario.key("alt+i");
    let screen = settled(&mut scenario);
    assert!(paths(&scenario).shows_ignored());
    assert!(screen.contains("ignored-dir/"), "{}", screen.text());
    assert!(
        screen.contains("[x] Show ignored files"),
        "{}",
        screen.text()
    );
    // A click on the toggle hides them again.
    let point = screen.find("Show ignored files").expect("toggle");
    scenario.click(point.x, point.y);
    let screen = settled(&mut scenario);
    assert!(!paths(&scenario).shows_ignored());
    assert!(!screen.contains("ignored-dir/"), "{}", screen.text());
    // Select two folders, then remove one from the selection list.
    focus_path(&mut scenario, "docs/");
    scenario.key("space");
    focus_path(&mut scenario, "名前/");
    scenario.key("space");
    let screen = settled(&mut scenario);
    let point = screen.find("Selected: 2 items").expect("summary");
    scenario.click(point.x, point.y);
    settled(&mut scenario);
    assert!(paths(&scenario).showing_selection());
    scenario.key("x");
    settled(&mut scenario);
    assert_eq!(
        paths(&scenario).include(),
        [PathSelector::Folder {
            path: "名前".to_owned()
        }]
    );
    // Esc goes back to the tree, then back to step 1.
    scenario.key("esc");
    settled(&mut scenario);
    assert!(!paths(&scenario).showing_selection());
    scenario.key("esc");
    settled(&mut scenario);
    assert_eq!(step(&scenario), 1);
}

#[test]
fn the_ignored_files_choice_follows_configuration_and_reaches_the_created_review() {
    use crate::config::ReviewViewConfig;
    use crate::config::view::FilesConfig;
    let fixture = paths_fixture();
    let backend = Arc::new(TestBackend::default());
    // Configured to show ignored files: the picker opens showing them, and a review created
    // with them shown needs no stored choice.
    let config = ReviewViewConfig {
        files: FilesConfig {
            show_ignored: true,
            ..FilesConfig::default()
        },
        ..ReviewViewConfig::default()
    };
    let app = ReviewApp::new(
        backend.services(),
        fixture.root().to_path_buf(),
        config,
        Entry::Home,
    );
    let mut scenario = Scenario::with_shared_host(app, backend.host(), 140, 40);
    settled(&mut scenario);
    scenario.key("n");
    settled(&mut scenario);
    let screen = pick_type(&mut scenario, 6);
    assert!(paths(&scenario).shows_ignored());
    assert!(screen.contains("ignored-dir/"), "{}", screen.text());

    // By default they are hidden; shown in the picker, the review keeps them shown.
    let mut scenario = wizard(&fixture, &backend);
    let screen = pick_type(&mut scenario, 6);
    assert!(!screen.contains("ignored-dir/"), "{}", screen.text());
    scenario.key("alt+i");
    settled(&mut scenario);
    focus_path(&mut scenario, "ignored-dir/");
    scenario.key("space");
    let screen = settled(&mut scenario);
    assert!(screen.contains("1 item · 1 file"), "{}", screen.text());
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(
        created
            .presentation
            .as_ref()
            .and_then(|presentation| presentation.show_ignored),
        Some(true)
    );
    let screen = settled(&mut scenario);
    assert!(screen.contains("secret.txt"), "{}", screen.text());
    assert!(scenario.surface().review().unwrap().show_ignored());
}

#[test]
fn clicks_on_tree_rows_expand_folders_and_select_files_at_every_width() {
    let fixture = paths_fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 6);
    for (width, height) in [(120, 30), (60, 20), (20, 12), (8, 6), (0, 0), (140, 40)] {
        scenario.resize(width, height);
        let screen = settled(&mut scenario);
        if width == 0 {
            // Nothing painted: a click lands on nothing.
            scenario.click(0, 0);
            settled(&mut scenario);
            continue;
        }
        if width < 60 {
            continue;
        }
        // The row painted with the folder's name is the row a click expands.
        let Some(point) = screen.find("名前/") else {
            panic!("{width}x{height}:\n{}", screen.text());
        };
        scenario.click(point.x, point.y);
        let screen = settled(&mut scenario);
        let file = screen
            .find("説明書.txt")
            .unwrap_or_else(|| panic!("{width}x{height}:\n{}", screen.text()));
        scenario.click(file.x, file.y);
        let screen = settled(&mut scenario);
        assert_eq!(
            paths(&scenario).include(),
            [PathSelector::File {
                path: "名前/説明書.txt".to_owned()
            }],
            "{width}x{height}:\n{}",
            screen.text()
        );
        assert!(screen.contains("[x] 説明書.txt"), "{}", screen.text());
        // Clicking the file again unselects it; clicking the folder collapses it.
        scenario.click(file.x, file.y);
        let screen = settled(&mut scenario);
        assert_eq!(paths(&scenario).include(), []);
        // Expanding scrolled the rows: the folder is clicked where it is painted now.
        let point = screen.find("名前/").expect("folder row");
        scenario.click(point.x, point.y);
        let screen = settled(&mut scenario);
        assert!(!screen.contains("説明書.txt"), "{}", screen.text());
        // A right click selects the folder itself, and again unselects it.
        let point = screen.find("名前/").expect("folder row");
        scenario.mouse(MouseEventKind::Down(MouseButton::Right), point.x, point.y);
        let screen = settled(&mut scenario);
        assert_eq!(
            paths(&scenario).include(),
            [PathSelector::Folder {
                path: "名前".to_owned()
            }],
            "{width}x{height}:\n{}",
            screen.text()
        );
        assert!(screen.contains("[x] 名前/"), "{}", screen.text());
        scenario.mouse(MouseEventKind::Down(MouseButton::Right), point.x, point.y);
        settled(&mut scenario);
        assert_eq!(paths(&scenario).include(), []);
    }
    // At a tiny width the keyboard still selects, and emoji names stay inside the frame.
    scenario.resize(20, 12);
    focus_path(&mut scenario, "名前/");
    scenario.key("space");
    let screen = settled(&mut scenario);
    for y in 0..12 {
        assert!(
            bmux_tui::text::Line::raw(screen.row_text(y)).width() <= 20,
            "{}",
            screen.text()
        );
    }
    assert_eq!(
        paths(&scenario).include(),
        [PathSelector::Folder {
            path: "名前".to_owned()
        }]
    );
}

#[test]
fn a_filter_with_more_matches_than_shown_says_how_many() {
    let fixture = fixture();
    let many = fixture.root().join("many");
    std::fs::create_dir_all(&many).expect("mkdir");
    for index in 0..520 {
        std::fs::write(many.join(format!("item-{index:03}.txt")), "x\n").expect("write");
    }
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 6);
    scenario.paste("item-");
    let screen = settled(&mut scenario);
    assert!(screen.contains("500 of 520 matches"), "{}", screen.text());
    // Narrowed so every match is listed, the frame no longer says so.
    scenario.paste("51");
    let screen = settled(&mut scenario);
    assert!(screen.contains("item-519.txt"), "{}", screen.text());
    assert!(!screen.contains(" matches"), "{}", screen.text());
}

#[test]
fn uncommitted_paths_count_the_changed_files_of_the_selection() {
    let fixture = fixture();
    std::fs::write(fixture.root().join("docs/zz-new.txt"), "new\n").expect("write");
    let changed = fixture
        .git(&["status", "--porcelain", "-uall", "--", "docs"])
        .lines()
        .count();
    let files = fixture.git(&["ls-files", "--", "docs"]).lines().count() + 1;
    assert!(changed < files, "the fixture has unchanged files in docs/");
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 0);
    scenario.keys(&["down", "down", "down", "enter"]);
    settled(&mut scenario);
    focus_path(&mut scenario, "docs/");
    scenario.key("space");
    let screen = settled(&mut scenario);
    assert_eq!(
        paths(&scenario).total(),
        Some(u32::try_from(changed).expect("count"))
    );
    let summary = format!(
        "1 item · {changed} changed file{}",
        if changed == 1 { "" } else { "s" }
    );
    assert!(screen.contains(&summary), "{summary}:\n{}", screen.text());
}

#[test]
fn repository_reviews_are_whole_or_limited_to_folders() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    let screen = pick_type(&mut scenario, 7);
    assert!(screen.contains("Whole repository"), "{}", screen.text());
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(kinds(&created), [ReviewSourceKind::Repository]);

    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 7);
    scenario.keys(&["down", "enter"]);
    let screen = settled(&mut scenario);
    // Only folders are listed.
    assert!(!screen.contains("README"), "{}", screen.text());
    focus_path(&mut scenario, "docs/");
    scenario.keys(&["space", "enter"]);
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::FileSet {
            include: vec![PathSelector::Folder {
                path: "docs".to_owned()
            }],
            exclude: Vec::new(),
        }]
    );
}

#[test]
fn uncommitted_changes_limit_to_paths_and_sources_edit_in_the_tree() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 0);
    scenario.keys(&["down", "down", "down", "enter"]);
    settled(&mut scenario);
    focus_path(&mut scenario, "docs/");
    scenario.keys(&["space", "enter"]);
    let screen = settled(&mut scenario);
    assert!(
        screen.contains("Which changes in those paths?"),
        "{}",
        screen.text()
    );
    scenario.keys(&["down", "enter"]);
    settled(&mut scenario);
    assert_eq!(step(&scenario), 3);
    // ctrl+e on the source opens it in the tree; adding a folder replaces it.
    scenario.keys(&["down", "down"]);
    scenario.key("ctrl+e");
    settled(&mut scenario);
    assert_eq!(
        paths(&scenario).include(),
        [PathSelector::Folder {
            path: "docs".to_owned()
        }]
    );
    focus_path(&mut scenario, "large/");
    scenario.keys(&["space", "enter"]);
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::WorkingTreePaths {
            paths: vec!["docs".to_owned(), "large".to_owned()],
            staged: false,
            unstaged: true,
        }]
    );
}

#[test]
fn composite_reviews_add_and_remove_sources() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 0);
    scenario.key("enter");
    settled(&mut scenario);
    // A typed title survives adding another source.
    scenario.paste(" and more");
    scenario.key("down");
    scenario.key("enter");
    settled(&mut scenario);
    assert_eq!(step(&scenario), 1);
    pick_type(&mut scenario, 0);
    scenario.key("down");
    scenario.key("enter");
    let screen = settled(&mut scenario);
    let wizard = scenario.surface().wizard().unwrap();
    assert_eq!(
        wizard.source_labels(),
        ["Unstaged changes", "Staged changes"]
    );
    assert_eq!(wizard.picker().query(), "Unstaged changes and more");
    assert!(screen.contains("Sources (2)"));
    // Removing a source keeps the rest; esc on step 1 with sources returns to step 3.
    scenario.key("end");
    scenario.key("enter");
    settled(&mut scenario);
    assert_eq!(
        scenario.surface().wizard().unwrap().source_labels(),
        ["Unstaged changes"]
    );
    scenario.key("esc");
    settled(&mut scenario);
    assert_eq!(step(&scenario), 1);
    scenario.key("esc");
    settled(&mut scenario);
    assert_eq!(step(&scenario), 3);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(created.title, "Unstaged changes and more");
    assert_eq!(kinds(&created), [ReviewSourceKind::WorkingTreeUnstaged]);
}

#[test]
fn pull_requests_and_stacks_come_from_the_provider() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let mut scenario = wizard(&fixture, &backend);
    let screen = pick_type(&mut scenario, 4);
    assert!(screen.contains("Review requested"), "{}", screen.text());
    // Review requested from me first.
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().focused(),
        Some("102")
    );
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(created.title, "#102 Layer 102");
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::PullRequest {
            provider: FAKE_PROVIDER.to_owned(),
            repository: FAKE_REPOSITORY.to_owned(),
            number: 102,
        }]
    );
    assert!(
        scenario
            .surface()
            .review()
            .unwrap()
            .document()
            .is_some_and(|doc| !doc.is_empty())
    );

    // Stack: the current branch's pull request first, then the mode.
    let mut scenario = wizard(&fixture, &backend);
    let screen = pick_type(&mut scenario, 5);
    assert!(screen.contains("Your branch"));
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().focused(),
        Some("100")
    );
    scenario.key("enter");
    let screen = settled(&mut scenario);
    for text in [
        "Whole stack",
        "Step through every layer",
        "One layer",
        "#103 Layer 103",
    ] {
        assert!(screen.contains(text), "{text}:\n{}", screen.text());
    }
    scenario.key("down");
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    let layer = |number| ReviewSourceKind::StackLayer {
        provider: FAKE_PROVIDER.to_owned(),
        repository: FAKE_REPOSITORY.to_owned(),
        number,
    };
    assert_eq!(
        kinds(&created),
        [layer(100), layer(101), layer(102), layer(103)]
    );

    // Whole stack and a single layer.
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 5);
    scenario.key("enter");
    settled(&mut scenario);
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::Stack {
            provider: FAKE_PROVIDER.to_owned(),
            repository: FAKE_REPOSITORY.to_owned(),
            top: 103,
        }]
    );
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 5);
    scenario.key("enter");
    settled(&mut scenario);
    scenario.key("end");
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(kinds(&created), [layer(103)]);
}

#[test]
fn stacks_over_the_limit_warn_and_the_warnings_can_be_dismissed() {
    use bcode_code_review_models::{ReviewStackLimit, StackOverLimit};
    let fixture = fixture();
    let backend = Arc::new(
        TestBackend::default()
            .with_provider(FakeProvider::new(fixture.root()))
            .with_stack_limit(ReviewStackLimit {
                max_layers: 2,
                over_limit: StackOverLimit::Warn,
                ..ReviewStackLimit::default()
            }),
    );
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 5);
    scenario.key("enter");
    let screen = settled(&mut scenario);
    let warning = "4 layers, over the 2-layer limit";
    assert!(screen.contains(warning), "{}", screen.text());
    // The warning does not block the choice; `enter` on it dismisses it.
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().focused(),
        Some("stack-limit-warning")
    );
    scenario.key("enter");
    let screen = settled(&mut scenario);
    assert!(!screen.contains(warning), "{}", screen.text());
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().focused(),
        Some("whole")
    );
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(created.sources.len(), 1);
    // Every layer is in the review, and a warning that stays until closed says so.
    let toast = "Stack of 4 layers";
    assert!(
        scenario
            .surface()
            .review()
            .unwrap()
            .toasts()
            .contains(&toast),
        "{:?}",
        scenario.surface().review().unwrap().toasts()
    );
    let screen = settled(&mut scenario);
    let close = screen.find("×").expect("toast close button");
    scenario.click(close.x, close.y);
    settled(&mut scenario);
    assert!(
        !scenario
            .surface()
            .review()
            .unwrap()
            .toasts()
            .contains(&toast),
        "{:?}",
        scenario.surface().review().unwrap().toasts()
    );
}

#[test]
fn pull_requests_missing_from_the_recent_list_are_looked_up() {
    use crate::testing::fixture::STACK_BRANCHES;
    let fixture = fixture();
    // #103 is too old for the provider's recent list.
    let backend = Arc::new(
        TestBackend::default().with_provider(FakeProvider::new(fixture.root()).unlisted(&[103])),
    );
    let mut scenario = wizard(&fixture, &backend);
    let screen = pick_type(&mut scenario, 5);
    assert!(!screen.contains("#103"), "{}", screen.text());
    scenario.paste(STACK_BRANCHES[2]);
    let screen = settled(&mut scenario);
    let offer = format!("Look up {} on", STACK_BRANCHES[2]);
    assert!(screen.contains(&offer), "{}", screen.text());
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().focused(),
        Some("pull-lookup")
    );
    scenario.key("enter");
    let screen = settled(&mut scenario);
    assert!(screen.contains("#103 Layer 103"), "{}", screen.text());
    assert!(!screen.contains(&offer), "{}", screen.text());
    assert!(
        backend
            .provider()
            .unwrap()
            .calls()
            .contains(&format!("review_source.pulls.list {}", STACK_BRANCHES[2]))
    );
    // The found pull request continues like a listed one: its stack resolves.
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().focused(),
        Some("103")
    );
    scenario.key("enter");
    let screen = settled(&mut scenario);
    assert!(screen.contains("Whole stack"), "{}", screen.text());

    // A lookup that finds nothing says so.
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 5);
    scenario.paste("no-such-branch");
    settled(&mut scenario);
    scenario.key("enter");
    let screen = settled(&mut scenario);
    assert!(
        screen.contains("has no open pull request no-such-branch"),
        "{}",
        screen.text()
    );
}

#[test]
fn a_stack_chosen_in_the_wizard_opens_without_resolving_it_again() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 5);
    scenario.key("enter");
    settled(&mut scenario);
    // "Step through every layer", then create and open the review.
    scenario.key("down");
    scenario.key("enter");
    settled(&mut scenario);
    create(&mut scenario, &backend, &fixture);
    let resolves = backend
        .provider()
        .unwrap()
        .calls()
        .iter()
        .filter(|call| call.starts_with(bcode_code_review_models::OP_REVIEW_SOURCE_STACK_RESOLVE))
        .count();
    assert_eq!(resolves, 1, "{:?}", backend.provider().unwrap().calls());
}

#[test]
fn missing_or_failing_providers_are_explained_and_other_types_still_work() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    let screen = pick_type(&mut scenario, 4);
    assert!(
        screen.contains("No pull request provider"),
        "{}",
        screen.text()
    );
    let message = scenario
        .surface()
        .wizard()
        .unwrap()
        .picker()
        .message()
        .unwrap();
    assert_eq!(message.body, NO_PROVIDER);
    // Back to step 1: another type still works.
    scenario.key("esc");
    settled(&mut scenario);
    assert_eq!(step(&scenario), 1);
    // The pull request type stays focused; the working tree is four rows up.
    scenario.keys(&["up", "up", "up", "up"]);
    pick_type(&mut scenario, 0);
    scenario.key("enter");
    settled(&mut scenario);
    assert_eq!(step(&scenario), 3);

    let backend = Arc::new(
        TestBackend::default().with_provider(
            FakeProvider::new(fixture.root())
                .failing_pulls(REVIEW_SOURCE_ERROR_UNAUTHENTICATED, "no token"),
        ),
    );
    let mut scenario = wizard(&fixture, &backend);
    let screen = pick_type(&mut scenario, 5);
    assert!(
        screen.contains("Could not list pull requests"),
        "{}",
        screen.text()
    );
    let message = scenario
        .surface()
        .wizard()
        .unwrap()
        .picker()
        .message()
        .unwrap();
    assert!(message.error && message.body.contains("not signed in"));
    // Esc twice leaves the wizard for home.
    scenario.key("esc");
    scenario.key("esc");
    settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Home);
}

#[test]
fn the_wizard_degrades_at_narrow_sizes() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    for (width, height) in [(40, 12), (20, 6), (8, 3), (0, 0), (140, 40)] {
        scenario.resize(width, height);
        let screen = settled(&mut scenario);
        if width >= 40 {
            assert!(
                screen.contains("Working tree"),
                "{width}x{height}:\n{}",
                screen.text()
            );
        }
    }
    // Clicking a row picks it.
    let screen = settled(&mut scenario);
    let point = screen.find("Commits").expect("commits row");
    scenario.click(point.x, point.y);
    settled(&mut scenario);
    assert_eq!(step(&scenario), 2);
}

#[test]
fn enter_typed_before_the_listing_answers_picks_from_the_new_answer() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 2);
    // The unfiltered listing is on screen; type a reference and press enter at once.
    scenario.paste(HEAD_BRANCH);
    scenario.key("enter");
    let screen = settled(&mut scenario);
    assert!(
        screen.contains(&format!("base {HEAD_BRANCH}")),
        "{}",
        screen.text()
    );
    // Another key before the answer cancels a deferred enter.
    scenario.paste(BASE_BRANCH);
    scenario.key("enter");
    scenario.key("backspace");
    let screen = settled(&mut scenario);
    assert!(
        screen.contains(&format!("base {HEAD_BRANCH}")),
        "still choosing the head: {}",
        screen.text()
    );
    assert_eq!(step(&scenario), 2);
    // Going back to step 1 keeps the type that was being configured focused.
    scenario.keys(&["esc", "esc"]);
    settled(&mut scenario);
    assert_eq!(step(&scenario), 1);
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().focused(),
        Some(crate::wizard::SourceType::CompareBranches.id())
    );
}

/// Clicking an option picks it, whether or not it is the active row, wherever the picker sits.
#[test]
fn clicking_options_picks_them_on_every_step() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    let screen = settled(&mut scenario);
    // The active first row.
    let point = screen.find("Working tree").expect("row");
    scenario.click(point.x, point.y);
    let screen = settled(&mut scenario);
    assert_eq!(step(&scenario), 2);
    // A row of the smaller step-2 picker, below its first row.
    let point = screen.find("Staged changes").expect("staged row");
    scenario.click(point.x, point.y);
    settled(&mut scenario);
    assert_eq!(step(&scenario), 3);
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().query(),
        "Staged changes"
    );
}

/// Outside a git repository the working-tree counts say they are unavailable instead of
/// counting forever.
#[test]
fn working_tree_counts_outside_a_repository_are_unavailable() {
    let directory = tempfile::tempdir().expect("directory");
    let backend = Arc::new(TestBackend::default());
    let app = ReviewApp::new(
        backend.services(),
        directory.path().to_path_buf(),
        crate::config::ReviewViewConfig::default(),
        Entry::Home,
    );
    let mut scenario = Scenario::new(app, 140, 40);
    settled(&mut scenario);
    scenario.key("n");
    settled(&mut scenario);
    let screen = pick_type(&mut scenario, 0);
    assert_eq!(step(&scenario), 2);
    assert!(screen.contains("counts unavailable"), "{}", screen.text());
    assert!(!screen.contains("counting"), "{}", screen.text());
}

/// One input to the wizard.
enum Input {
    Key(&'static str),
    Paste(String),
}

#[test]
fn every_wizard_mode_can_be_created_through_create_review() {
    use crate::wizard::SourceType;
    use Input::{Key, Paste};
    use bcode_code_review::assistant::create;
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let mut created = Vec::new();
    for (index, source_type) in SourceType::ALL.into_iter().enumerate() {
        // Every detail of every mode. Exhaustive: a new mode does not compile until it is here.
        let runs: Vec<Vec<Input>> = match source_type {
            SourceType::WorkingTree => vec![
                vec![Key("enter")],
                vec![Key("down"), Key("enter")],
                vec![Key("down"), Key("down"), Key("enter")],
                // Limit to paths: the tree's first folder, both kinds of changes.
                vec![
                    Key("down"),
                    Key("down"),
                    Key("down"),
                    Key("enter"),
                    Key("space"),
                    Key("enter"),
                    Key("enter"),
                ],
            ],
            // The detected base, and the provider's first pull request.
            SourceType::CurrentBranch | SourceType::PullRequest => vec![vec![Key("enter")]],
            SourceType::CompareBranches => vec![vec![
                Paste(BASE_BRANCH.to_owned()),
                Key("enter"),
                Paste(HEAD_BRANCH.to_owned()),
                Key("enter"),
            ]],
            SourceType::Commits => vec![
                vec![Key("enter")],
                vec![Key("tab"), Key("down"), Key("shift+tab"), Key("enter")],
            ],
            SourceType::Stack => vec![
                vec![Key("enter"), Key("enter")],
                vec![Key("enter"), Key("down"), Key("enter")],
                vec![Key("enter"), Key("end"), Key("enter")],
            ],
            SourceType::Files => vec![
                // The tree's first row, a range, a glob, a single file, and a folder with an
                // exclusion and another file.
                vec![Key("enter")],
                vec![Paste(format!("{UNICODE_FILE}:2-4")), Key("enter")],
                vec![Paste("docs/*.txt".to_owned()), Key("enter")],
                vec![Paste(UNICODE_FILE.to_owned()), Key("tab"), Key("enter")],
                vec![
                    Key("space"),
                    Key("l"),
                    Key("l"),
                    Key("space"),
                    Paste(UNICODE_FILE.to_owned()),
                    Key("tab"),
                    Key("enter"),
                ],
            ],
            SourceType::Repository => vec![
                vec![Key("enter")],
                vec![Key("down"), Key("enter"), Key("space"), Key("enter")],
            ],
        };
        for run in runs {
            let mut scenario = wizard(&fixture, &backend);
            pick_type(&mut scenario, index);
            for input in run {
                match input {
                    Key(key) => {
                        scenario.key(key);
                    }
                    Paste(text) => {
                        scenario.paste(&text);
                    }
                }
                settled(&mut scenario);
            }
            created.push((
                source_type,
                kinds(&create(&mut scenario, &backend, &fixture)),
            ));
        }
    }
    // Each run creates sources of its mode's kinds (a stack's layers are several), and the
    // runs of a mode differ: every detail is exercised, not one repeated.
    let mut seen: Vec<(SourceType, Vec<ReviewSourceKind>)> = Vec::new();
    for (source_type, kinds) in &created {
        assert!(!kinds.is_empty(), "{source_type:?} created nothing");
        for kind in kinds {
            let fits = match source_type {
                SourceType::WorkingTree => matches!(
                    kind,
                    ReviewSourceKind::WorkingTreeAndIndex
                        | ReviewSourceKind::IndexStaged
                        | ReviewSourceKind::WorkingTreeUnstaged
                        | ReviewSourceKind::WorkingTreePaths { .. }
                ),
                SourceType::CurrentBranch => matches!(kind, ReviewSourceKind::CurrentBranch { .. }),
                SourceType::PullRequest => matches!(kind, ReviewSourceKind::PullRequest { .. }),
                SourceType::CompareBranches => {
                    matches!(kind, ReviewSourceKind::BranchCompare { .. })
                }
                SourceType::Commits => matches!(
                    kind,
                    ReviewSourceKind::Commit { .. }
                        | ReviewSourceKind::Commits { .. }
                        | ReviewSourceKind::CommitRange { .. }
                        | ReviewSourceKind::LastCommit
                ),
                SourceType::Stack => matches!(
                    kind,
                    ReviewSourceKind::Stack { .. } | ReviewSourceKind::StackLayer { .. }
                ),
                SourceType::Files => matches!(
                    kind,
                    ReviewSourceKind::File { .. }
                        | ReviewSourceKind::FileRange { .. }
                        | ReviewSourceKind::FileSet { .. }
                ),
                SourceType::Repository => matches!(
                    kind,
                    ReviewSourceKind::Repository | ReviewSourceKind::FileSet { .. }
                ),
            };
            assert!(fits, "{source_type:?} created {kind:?}");
        }
        assert!(
            !seen.contains(&(*source_type, kinds.clone())),
            "{source_type:?} created {kinds:?} twice"
        );
        seen.push((*source_type, kinds.clone()));
    }
    for (source_type, kinds) in created {
        for kind in kinds {
            let argument = create::source_argument(&kind, None).unwrap_or_else(|| {
                panic!("{source_type:?} creates {kind:?}, which create_review cannot")
            });
            let plan = create::plan(fixture.root(), None, vec![argument])
                .unwrap_or_else(|error| panic!("{source_type:?} {kind:?}: {error}"));
            let planned: Vec<ReviewSourceKind> =
                plan.sources.into_iter().map(|(kind, _)| kind).collect();
            assert_eq!(planned, std::slice::from_ref(&kind), "{source_type:?}");
        }
    }
}

#[test]
fn home_offers_files_reviews_and_the_file_panel_reviews_folders_as_they_are_now() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let (mut scenario, screen) = open(&fixture, &backend, Entry::Home, 140, 40);
    assert!(
        screen.contains("Review files or folders…"),
        "{}",
        screen.text()
    );
    scenario.key("shift+n");
    settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::NewReview);
    assert_eq!(paths(&scenario).include(), []);
    scenario.key("esc");
    settled(&mut scenario);
    assert_eq!(step(&scenario), 1);

    // The cursor file's folder, from a review of one file.
    let (mut scenario, _) = open(
        &fixture,
        &backend,
        Entry::Sources {
            title: None,
            sources: vec![ReviewSourceKind::File {
                path: UNICODE_FILE.to_owned(),
            }],
        },
        140,
        40,
    );
    scenario.key("alt+r");
    settled(&mut scenario);
    let review = scenario.surface().review().expect("review");
    assert_eq!(
        kinds(review.workspace().expect("workspace")),
        [ReviewSourceKind::FileSet {
            include: vec![PathSelector::Folder {
                path: "docs".to_owned()
            }],
            exclude: Vec::new(),
        }]
    );
    let screen = settled(&mut scenario);
    assert!(screen.contains("docs/ as it is now"), "{}", screen.text());

    // A folder row of the file panel (the first one, below the file rows above it).
    for downs in 0..40 {
        let (mut scenario, _) = open(
            &fixture,
            &backend,
            Entry::Sources {
                title: None,
                sources: vec![ReviewSourceKind::Repository],
            },
            140,
            40,
        );
        scenario.key("tab");
        for _ in 0..downs {
            scenario.key("down");
        }
        settled(&mut scenario);
        scenario.key("alt+r");
        settled(&mut scenario);
        let review = scenario.surface().review().expect("review");
        match kinds(review.workspace().expect("workspace")).as_slice() {
            [ReviewSourceKind::FileSet { include, .. }] => {
                let [PathSelector::Folder { path }] = include.as_slice() else {
                    panic!("{include:?}");
                };
                assert!(fixture.root().join(path).is_dir(), "{path}");
                return;
            }
            // A file row reviews that file.
            [ReviewSourceKind::File { path }] => assert!(fixture.root().join(path).exists()),
            other => panic!("{other:?}"),
        }
    }
    panic!("no folder row in the file panel");
}
