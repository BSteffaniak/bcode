//! File separation: headers set apart from the chrome and the content, torn pages between
//! files, the details row's buttons, collapsing, and the densities by size and configuration.

use super::{stable, surface};
use crate::ReviewSurface;
use crate::config::ReviewViewConfig;
use crate::config::view::{FileHeaderDensity, FileSeparator};
use crate::testing::fixture::{FixtureSpec, GitFixture};
use crate::testing::harness::{Scenario, Screen};
use bcode_code_review::document::{FileKey, ItemKey};
use bcode_code_review_models::ReviewTarget;

fn open_with(
    fixture: &GitFixture,
    config: ReviewViewConfig,
    width: u16,
    height: u16,
) -> (Scenario<ReviewSurface>, Screen) {
    let mut scenario = Scenario::new(
        surface(fixture, ReviewTarget::LastCommit, config),
        width,
        height,
    );
    let screen = stable(&mut scenario);
    (scenario, screen)
}

fn click(scenario: &mut Scenario<ReviewSurface>, needle: &str) -> Screen {
    let screen = scenario.frame();
    let at = screen
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} is not painted:\n{}", screen.text()));
    scenario.click(at.x, at.y);
    stable(scenario)
}

#[test]
fn file_headers_stand_apart_and_pages_tear_between_files() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let (_, screen) = open_with(&fixture, ReviewViewConfig::default(), 80, 20);
    let header = screen.find(".gitmodules").expect("first file header");
    let header_bg = screen.style_at(header.x, header.y).bg;
    // Not the header bar's fill, and not the content's.
    assert_ne!(header_bg, screen.style_at(1, 0).bg, "{}", screen.text());
    let content = screen.find("[submodule").expect("content");
    assert_ne!(header_bg, screen.style_at(content.x, content.y).bg);
    // The name is bold; the status is a pill.
    assert!(
        screen
            .style_at(header.x, header.y)
            .modifiers
            .contains(bmux_tui::style::Modifier::BOLD)
    );
    // Compact: one torn row between the files, the page color over the desk.
    let next = screen.find("added/new_000.rs").expect("second file header");
    let tear = screen.row_text(next.y - 1);
    assert!(tear.contains('▀'), "{}", screen.text());
    let paper = screen
        .find("▀")
        .map(|at| screen.style_at(at.x, at.y))
        .expect("tear");
    assert!(paper.bg.is_some() && paper.fg.is_some() && paper.fg != paper.bg);
}

#[test]
fn comfortable_files_have_details_and_buttons_that_work() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let config = ReviewViewConfig {
        file_header: FileHeaderDensity::Comfortable,
        ..ReviewViewConfig::default()
    };
    let (mut scenario, screen) = open_with(&fixture, config, 140, 40);
    for part in [
        "3 lines",
        "1 hunk",
        "Comment",
        "Ask Bcode",
        "Whole file",
        "Collapse",
    ] {
        assert!(screen.contains(part), "{part:?}:\n{}", screen.text());
    }
    // Two torn rows: this page's bottom and the next one's top.
    let next = screen.find("added/new_000.rs").expect("second file");
    assert!(
        screen.row_text(next.y - 1).contains('▄'),
        "{}",
        screen.text()
    );
    assert!(
        screen.row_text(next.y - 2).contains('▀'),
        "{}",
        screen.text()
    );

    // Collapse folds the file to its header; Expand brings its content back.
    let screen = click(&mut scenario, "Collapse");
    assert!(!screen.contains("[submodule"), "{}", screen.text());
    assert!(screen.contains("collapsed"));
    let file = FileKey::new("source-1".to_owned(), None, ".gitmodules".to_owned());
    assert!(scenario.surface().document().unwrap().is_collapsed(&file));
    let screen = click(&mut scenario, "Expand");
    assert!(screen.contains("[submodule"), "{}", screen.text());

    // The viewed mark in the header is a button.
    let screen = scenario.frame();
    let header = screen.find(".gitmodules").expect("header");
    let row = screen.row_text(header.y);
    let mark = row
        .chars()
        .collect::<Vec<_>>()
        .iter()
        .rposition(|cell| *cell == '○')
        .expect("viewed mark");
    scenario.click(u16::try_from(mark).unwrap(), header.y);
    stable(&mut scenario);
    let doc = scenario.surface().document().unwrap();
    assert!(
        doc.viewed_state(&file)
            .is_some_and(bcode_code_review::viewed::ViewedState::is_viewed),
        "{}",
        scenario.frame().text()
    );
}

#[test]
fn spacious_headers_have_two_rows_and_file_steps() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let config = ReviewViewConfig {
        file_header: FileHeaderDensity::Spacious,
        ..ReviewViewConfig::default()
    };
    let (mut scenario, screen) = open_with(&fixture, config, 160, 48);
    assert!(screen.contains("1 of 15 files"), "{}", screen.text());
    assert!(screen.contains("○ Viewed"));
    assert!(screen.contains("next ↓"));
    click(&mut scenario, "next ↓");
    let cursor = scenario.surface().cursor().cloned();
    assert!(
        matches!(&cursor, Some(ItemKey::FileHeader(file)) if file.path != ".gitmodules"),
        "{cursor:?}"
    );
}

#[test]
fn the_separator_and_density_follow_configuration() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let rule = ReviewViewConfig {
        file_separator: FileSeparator::Rule,
        ..ReviewViewConfig::default()
    };
    let (_, screen) = open_with(&fixture, rule, 80, 20);
    let next = screen.find("added/new_000.rs").expect("second file");
    assert!(
        screen.row_text(next.y - 1).contains('┄'),
        "{}",
        screen.text()
    );

    let none = ReviewViewConfig {
        file_separator: FileSeparator::None,
        ..ReviewViewConfig::default()
    };
    let (_, screen) = open_with(&fixture, none, 80, 20);
    let next = screen.find("added/new_000.rs").expect("second file");
    assert!(
        screen.row_text(next.y - 1).contains("url = "),
        "{}",
        screen.text()
    );

    // Auto: a small pane is compact (no details), a big one is not.
    let (_, small) = open_with(&fixture, ReviewViewConfig::default(), 80, 20);
    assert!(!small.contains("Whole file"));
    let (_, big) = open_with(&fixture, ReviewViewConfig::default(), 200, 60);
    assert!(big.contains("Whole file"), "{}", big.text());
}

#[test]
fn an_edge_sets_the_pinned_headers_apart_from_the_content_under_them() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let (mut scenario, _) = open_with(&fixture, ReviewViewConfig::default(), 80, 20);
    // Into the second file, far enough that its header and hunk header pin.
    scenario.key("n");
    for _ in 0..15 {
        scenario.key("j");
    }
    let screen = stable(&mut scenario);
    let header = screen.find("added/new_000.rs").expect("pinned file header");
    let hunk = header.y + 1;
    assert!(screen.row_text(hunk).contains("@@"), "{}", screen.text());
    let underlined = |y: u16| {
        screen
            .style_at(header.x, y)
            .modifiers
            .contains(bmux_tui::style::Modifier::UNDERLINE)
    };
    // The edge is under the deepest pinned header only.
    assert!(underlined(hunk), "{}", screen.text());
    assert!(!underlined(header.y));
    assert!(!underlined(hunk + 1));
}
