//! File separation and folding: headers set apart from the chrome and the content, files as
//! cards on a desk, the details row's buttons, folding (by hand and by the viewed mark), and
//! the densities and separators by size, configuration, and theme.

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

/// A row's text inside the review panel's borders, trimmed.
fn inner(row: &str) -> String {
    row.trim_matches(|cell: char| cell == '│' || cell == '█' || cell.is_whitespace())
        .to_owned()
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
fn file_headers_stand_apart_and_files_are_cards_on_a_desk() {
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
    // Compact: one row between the files, the page's straight bottom edge over the desk.
    let next = screen.find("added/new_000.rs").expect("second file header");
    let edge = inner(&screen.row_text(next.y - 1));
    assert!(edge.chars().all(|cell| cell == '▀'), "{}", screen.text());
    let paper = screen.style_at(header.x, next.y - 1);
    assert!(paper.bg.is_some() && paper.fg.is_some() && paper.fg != paper.bg);
    // The page above the edge is the page's own color (the hunk row is on it).
    let hunk = screen.find("@@").expect("hunk header");
    assert_eq!(
        paper.fg,
        screen.style_at(hunk.x, hunk.y).bg,
        "{paper:?} {:?}",
        screen.style_at(hunk.x, hunk.y)
    );
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
    // Two rows: this card's bottom edge and the next card's lip.
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

    // Collapse folds the file to its header (no details either); its chevron opens it again.
    let screen = click(&mut scenario, "Collapse");
    assert!(!screen.contains("[submodule"), "{}", screen.text());
    let file = FileKey::new("source-1".to_owned(), None, ".gitmodules".to_owned());
    assert!(scenario.surface().document().unwrap().is_collapsed(&file));
    let header = screen.find(".gitmodules").expect("header");
    assert!(screen.row_text(header.y).contains('▸'), "{}", screen.text());
    assert!(!screen.row_text(header.y + 1).contains("1 hunk"));
    let screen = click(&mut scenario, "▸");
    assert!(screen.contains("[submodule"), "{}", screen.text());
    assert!(screen.contains("Collapse"));

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
    // A rule naming the next file's position.
    let rule_row = screen.row_text(next.y - 1);
    assert!(
        rule_row.contains('─') && rule_row.contains(" 2 of 15 "),
        "{}",
        screen.text()
    );
    let unlabeled = ReviewViewConfig {
        file_separator: FileSeparator::Rule,
        file_separator_label: Some(false),
        file_separator_rows: Some(3),
        ..ReviewViewConfig::default()
    };
    let (_, screen) = open_with(&fixture, unlabeled, 80, 20);
    let next = screen.find("added/new_000.rs").expect("second file");
    assert_eq!(inner(&screen.row_text(next.y - 1)), "");
    let rule_row = screen.row_text(next.y - 2);
    assert!(
        rule_row.contains('─') && !rule_row.contains(" of "),
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

#[test]
fn viewed_files_fold_and_open_by_hand_like_on_the_forge() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let (mut scenario, _) = open_with(&fixture, ReviewViewConfig::default(), 80, 30);
    let file = FileKey::new("source-1".to_owned(), None, ".gitmodules".to_owned());
    let doc = |scenario: &Scenario<ReviewSurface>| scenario.surface().document().unwrap().clone();
    let viewed = |scenario: &Scenario<ReviewSurface>| {
        doc(scenario)
            .viewed_state(&file)
            .is_some_and(bcode_code_review::viewed::ViewedState::is_viewed)
    };
    // Marking viewed folds it.
    scenario.key("g");
    scenario.key("v");
    let screen = stable(&mut scenario);
    assert!(viewed(&scenario));
    assert!(doc(&scenario).is_collapsed(&file), "{}", screen.text());
    assert!(!screen.contains("[submodule"), "{}", screen.text());
    // Opened by hand, it stays viewed and open.
    scenario.key("g");
    stable(&mut scenario);
    let screen = click(&mut scenario, "▸");
    assert!(screen.contains("[submodule"), "{}", screen.text());
    assert!(viewed(&scenario));
    assert!(!doc(&scenario).is_collapsed(&file));
    // Unmarked it stays open; marked again, it folds again.
    scenario.key("g");
    scenario.key("v");
    stable(&mut scenario);
    assert!(!viewed(&scenario) && !doc(&scenario).is_collapsed(&file));
    scenario.key("g");
    scenario.key("v");
    stable(&mut scenario);
    assert!(viewed(&scenario) && doc(&scenario).is_collapsed(&file));
    // Folding by hand marks nothing.
    let next = FileKey::new("source-1".to_owned(), None, "added/new_000.rs".to_owned());
    let screen = scenario.frame();
    let header = screen.find("added/new_000.rs").expect("second header");
    let row = screen.row_text(header.y);
    let chevron = row.chars().position(|cell| cell == '▾').expect("chevron");
    scenario.click(u16::try_from(chevron).unwrap(), header.y);
    stable(&mut scenario);
    let doc_now = doc(&scenario);
    assert!(doc_now.is_collapsed(&next));
    assert!(
        !doc_now
            .viewed_state(&next)
            .is_some_and(bcode_code_review::viewed::ViewedState::is_viewed)
    );
}

#[test]
fn folded_files_read_as_a_list_and_motions_open_them() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let (mut scenario, _) = open_with(&fixture, ReviewViewConfig::default(), 80, 30);
    // `shift+z` on a header folds every file: headers one desk row apart, no edges.
    scenario.key("g");
    scenario.key("shift+z");
    let screen = stable(&mut scenario);
    let first = screen.find(".gitmodules").expect("first");
    let second = screen.find("added/new_000.rs").expect("second");
    assert_eq!(second.y - first.y, 2, "{}", screen.text());
    assert_eq!(
        inner(&screen.row_text(first.y + 1)),
        "",
        "{}",
        screen.text()
    );
    // A hunk motion goes into the first file, opening it.
    let file = FileKey::new("source-1".to_owned(), None, ".gitmodules".to_owned());
    scenario.key("}");
    stable(&mut scenario);
    assert!(!scenario.surface().document().unwrap().is_collapsed(&file));
    assert!(
        matches!(scenario.surface().cursor(), Some(ItemKey::HunkHeader { file: at, .. }) if at.path == ".gitmodules"),
        "{:?}",
        scenario.surface().cursor()
    );
    // And again: every file opens.
    scenario.key("g");
    scenario.key("shift+z");
    scenario.key("shift+z");
    let screen = stable(&mut scenario);
    assert!(screen.contains("[submodule"), "{}", screen.text());
}

#[test]
fn a_theme_sets_the_file_roles() {
    use bmux_tui::style::{Color, Style};
    let mut styles = super::chrome::host_theme(super::chrome::rgb(200, 120, 250));
    let derived =
        crate::theme::ReviewTheme::derive(Some(&styles), crate::theme::IconStyle::Unicode);
    // Derived: a desk close to the page but told apart from it.
    let page = derived.files.page_edge.fg;
    assert!(derived.files.desk.bg.is_some() && derived.files.desk.bg != page);
    styles.files.desk = Some(Style::new().bg(Color::Rgb(1, 2, 3)));
    styles.files.header = Some(Style::new().bg(Color::Rgb(4, 5, 6)));
    styles.files.label = Some(Style::new().fg(Color::Rgb(7, 8, 9)));
    let theme = crate::theme::ReviewTheme::derive(Some(&styles), crate::theme::IconStyle::Unicode);
    assert_eq!(theme.files.desk.bg, Some(Color::Rgb(1, 2, 3)));
    assert_eq!(theme.files.header.bg, Some(Color::Rgb(4, 5, 6)));
    assert_eq!(theme.files.label.fg, Some(Color::Rgb(7, 8, 9)));
    // The lip is the header color over the desk.
    assert_eq!(
        (theme.files.lip.fg, theme.files.lip.bg),
        (Some(Color::Rgb(4, 5, 6)), Some(Color::Rgb(1, 2, 3)))
    );
}
