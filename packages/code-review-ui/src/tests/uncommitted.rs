//! Reviews of files and folders as they are now: uncommitted changes marked in whole files (on
//! by default, off by configuration, toggled live and kept with the review), ignored files
//! listed on request, and folders listed again whenever the review opens.

use super::app::{create, fixture, open, settled, stored};
use crate::app::ReviewApp;
use crate::config::ReviewViewConfig;
use crate::config::view::FilesConfig;
use crate::entry::Entry;
use crate::testing::backend::TestBackend;
use crate::testing::fixture::GitFixture;
use crate::testing::harness::{Scenario, Screen};
use bcode_code_review_models::{PathSelector, ReviewSourceKind};
use std::sync::Arc;

/// A folder of the fixture with a committed file changed in the working tree (one line
/// changed, one removed, one added) and an ignored file.
fn changed_fixture() -> GitFixture {
    let fixture = fixture();
    let folder = fixture.root().join("notes");
    std::fs::create_dir_all(&folder).expect("mkdir");
    std::fs::write(folder.join("plan.md"), "one\ntwo\nthree\nfour\n").expect("write");
    std::fs::write(fixture.root().join(".gitignore"), "*.log\n").expect("write");
    fixture.run(&["add", "notes/plan.md", ".gitignore"]);
    fixture.run(&["commit", "-q", "-m", "notes"]);
    std::fs::write(folder.join("plan.md"), "one\nTWO\nfour\nfive\n").expect("write");
    std::fs::write(folder.join("debug.log"), "noise\n").expect("write");
    fixture
}

fn notes() -> ReviewSourceKind {
    ReviewSourceKind::FileSet {
        include: vec![PathSelector::Folder {
            path: "notes".to_owned(),
        }],
        exclude: Vec::new(),
    }
}

fn open_with(
    fixture: &GitFixture,
    backend: &Arc<TestBackend>,
    config: ReviewViewConfig,
    entry: Entry,
) -> (Scenario<ReviewApp>, Screen) {
    let app = ReviewApp::new(
        backend.services(),
        fixture.root().to_path_buf(),
        config,
        entry,
    )
    .with_scroll_motion(std::time::Duration::ZERO);
    let mut scenario = Scenario::with_shared_host(app, backend.host(), 140, 40);
    let screen = settled(&mut scenario);
    (scenario, screen)
}

#[test]
fn uncommitted_changes_are_marked_by_default_toggle_live_and_stay_with_the_review() {
    let fixture = changed_fixture();
    let backend = Arc::new(TestBackend::default());
    let workspace = create(&backend, &fixture, "Notes", vec![notes()]);
    let id = workspace.id.clone();
    let (mut scenario, screen) = open(
        &fixture,
        &backend,
        Entry::Workspace {
            workspace: Box::new(workspace),
        },
        140,
        40,
    );
    assert!(screen.contains("uncommitted +2"), "{}", screen.text());
    assert!(
        screen.contains("1 line removed since HEAD"),
        "{}",
        screen.text()
    );
    // The changed line carries the mark's bar; an unchanged one does not.
    let review = scenario.surface().review().expect("review");
    assert!(review.uncommitted_marks());
    // `w` hides the marks, and the choice is saved with the review.
    scenario.key("w");
    let screen = settled(&mut scenario);
    assert!(!screen.contains("uncommitted +2"), "{}", screen.text());
    assert!(!screen.contains("removed since HEAD"), "{}", screen.text());
    assert!(!scenario.surface().review().unwrap().uncommitted_marks());
    scenario.key("shift+l");
    settled(&mut scenario);
    let saved = stored(&backend, &fixture, &id);
    assert_eq!(
        saved
            .presentation
            .as_ref()
            .and_then(|presentation| presentation.uncommitted_marks),
        Some(false)
    );
    let (scenario, screen) = open(
        &fixture,
        &backend,
        Entry::Workspace {
            workspace: Box::new(saved),
        },
        140,
        40,
    );
    assert!(!screen.contains("uncommitted +2"), "{}", screen.text());
    assert!(!scenario.surface().review().unwrap().uncommitted_marks());
}

#[test]
fn configuration_turns_the_marks_off_and_the_toggle_back_on() {
    let fixture = changed_fixture();
    let backend = Arc::new(TestBackend::default());
    let config = ReviewViewConfig {
        files: FilesConfig {
            uncommitted_marks: false,
            ..FilesConfig::default()
        },
        ..ReviewViewConfig::default()
    };
    let (mut scenario, screen) = open_with(
        &fixture,
        &backend,
        config,
        Entry::Sources {
            title: None,
            sources: vec![notes()],
        },
    );
    assert!(screen.contains("plan.md"), "{}", screen.text());
    assert!(!screen.contains("uncommitted +"), "{}", screen.text());
    scenario.key("w");
    let screen = settled(&mut scenario);
    assert!(screen.contains("uncommitted +2"), "{}", screen.text());
}

#[test]
fn ignored_files_are_listed_on_request_and_folders_list_again_on_reopen() {
    let fixture = changed_fixture();
    let backend = Arc::new(TestBackend::default());
    let workspace = create(&backend, &fixture, "Notes", vec![notes()]);
    let (mut scenario, screen) = open(
        &fixture,
        &backend,
        Entry::Workspace {
            workspace: Box::new(workspace.clone()),
        },
        140,
        40,
    );
    assert!(!screen.contains("debug.log"), "{}", screen.text());
    scenario.key("alt+i");
    let screen = settled(&mut scenario);
    assert!(screen.contains("debug.log"), "{}", screen.text());
    assert!(screen.contains("ignored"), "{}", screen.text());
    assert!(scenario.surface().review().unwrap().show_ignored());
    scenario.key("alt+i");
    let screen = settled(&mut scenario);
    assert!(!screen.contains("debug.log"), "{}", screen.text());
    scenario.key("shift+l");
    settled(&mut scenario);
    // A file added to the folder after the review was created is reviewed when it reopens.
    std::fs::write(fixture.root().join("notes/added.md"), "added\n").expect("write");
    let (_, screen) = open(
        &fixture,
        &backend,
        Entry::Workspace {
            workspace: Box::new(stored(&backend, &fixture, &workspace.id)),
        },
        140,
        40,
    );
    assert!(screen.contains("added.md"), "{}", screen.text());
    assert!(screen.contains("untracked"), "{}", screen.text());
}

#[test]
fn renamed_binary_and_spaced_files_show_their_uncommitted_change() {
    let fixture = fixture();
    let root = fixture.root();
    let folder = root.join("notes");
    std::fs::create_dir_all(&folder).expect("mkdir");
    std::fs::write(folder.join("my plan.md"), "one\ntwo\n").expect("write");
    std::fs::write(folder.join("logo.png"), [0_u8, 1, 2, 0, 4]).expect("write");
    std::fs::write(folder.join("old.md"), "kept\n").expect("write");
    fixture.run(&["add", "notes"]);
    fixture.run(&["commit", "-q", "-m", "notes"]);
    std::fs::write(folder.join("my plan.md"), "one\nTWO\nthree\n").expect("write");
    std::fs::write(folder.join("logo.png"), [0_u8, 9, 9, 0, 4]).expect("write");
    fixture.run(&["mv", "notes/old.md", "notes/new.md"]);
    let backend = Arc::new(TestBackend::default());
    let (_, screen) = open_with(
        &fixture,
        &backend,
        ReviewViewConfig::default(),
        Entry::Sources {
            title: None,
            sources: vec![notes()],
        },
    );
    let line = |name: &str| {
        screen
            .text()
            .lines()
            .find(|line| line.contains(name) && line.contains("uncommitted"))
            .map_or_else(
                || panic!("no uncommitted header for {name}:\n{}", screen.text()),
                str::to_owned,
            )
    };
    assert!(line("my plan.md").contains("uncommitted +2"));
    assert!(line("logo.png").contains("uncommitted binary"));
    let renamed = line("new.md");
    assert!(renamed.contains("uncommitted renamed"), "{renamed}");
    assert!(
        !renamed.contains("+1"),
        "a pure rename has no new lines: {renamed}"
    );
}

#[test]
fn a_listing_cut_at_the_file_limit_says_so_once() {
    let fixture = changed_fixture();
    for name in ["a.md", "b.md", "c.md"] {
        std::fs::write(fixture.root().join("notes").join(name), "x\n").expect("write");
    }
    let backend = Arc::new(TestBackend::default());
    let config = ReviewViewConfig {
        files: FilesConfig {
            max_files: Some(2),
            ..FilesConfig::default()
        },
        ..ReviewViewConfig::default()
    };
    let (mut scenario, screen) = open_with(
        &fixture,
        &backend,
        config,
        Entry::Sources {
            title: None,
            sources: vec![notes()],
        },
    );
    assert!(
        screen.contains("Some files are not listed"),
        "{}",
        screen.text()
    );
    assert!(
        screen.contains("resolves to 4 files; showing"),
        "{}",
        screen.text()
    );
    let surface = scenario.surface().review().expect("review");
    assert_eq!(surface.file_tree().expect("tree").file_count(), 2);
    let warnings = |surface: &crate::surface::ReviewSurface| {
        surface
            .toasts()
            .into_iter()
            .filter(|toast| *toast == "Some files are not listed")
            .count()
    };
    assert_eq!(warnings(surface), 1);
    // A refresh lists the same cut again without repeating the warning.
    scenario.key("r");
    settled(&mut scenario);
    assert_eq!(warnings(scenario.surface().review().expect("review")), 1);
}

#[test]
fn the_toggles_say_when_a_review_has_nothing_they_change() {
    let fixture = changed_fixture();
    let backend = Arc::new(TestBackend::default());
    let (mut scenario, _) = open_with(
        &fixture,
        &backend,
        ReviewViewConfig::default(),
        Entry::Sources {
            title: None,
            sources: vec![ReviewSourceKind::LastCommit],
        },
    );
    let status = |scenario: &Scenario<ReviewApp>| {
        scenario
            .surface()
            .review()
            .and_then(|review| review.status().map(str::to_owned))
            .unwrap_or_default()
    };
    scenario.key("w");
    settled(&mut scenario);
    assert!(
        status(&scenario).contains("no whole working-tree files"),
        "{}",
        status(&scenario)
    );
    assert!(scenario.surface().review().unwrap().uncommitted_marks());
    scenario.key("alt+i");
    settled(&mut scenario);
    assert!(
        status(&scenario).contains("no folder, glob or repository sources"),
        "{}",
        status(&scenario)
    );
    assert!(!scenario.surface().review().unwrap().show_ignored());
}
