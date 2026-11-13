//! Review chrome and visual system: theme roles, the file panel and its sync with the document,
//! panels and their narrow-width degradation, the header and footer, filtering, persistence,
//! icons, and hit-region agreement.

use super::{open, stable, surface};
use crate::config::ReviewViewConfig;
use crate::panels::{FILES_AUTO_BODY, FILES_MIN_BODY};
use crate::surface::{FILES_PANE_ID, REVIEW_PANE_ID, TREE_ID};
use crate::testing::fixture::{FixtureSpec, GitFixture, LARGE_FILE, UNICODE_FILE};
use crate::testing::fixture_branch_compare;
use crate::testing::harness::{Scenario, Screen};
use crate::theme::IconStyle;
use crate::{Focus, ReviewSurface};
use bcode_code_review::document::{ItemKey, ReviewItem};
use bcode_tui_theme::components::ComponentStyles;
use bmux_color::Rgb;
use bmux_tui::event::{MouseButton, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::style::{Color, Style};
use bmux_tui_components::diff_viewer::DiffViewerStyle;
use bmux_tui_components::theme::{ComponentTheme, ComponentThemeSeed};

fn fixture() -> GitFixture {
    GitFixture::generate(FixtureSpec::small())
}

pub(super) const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

/// A host theme whose roles all have distinct colors; `accent` varies it.
pub(super) fn host_theme(accent: Rgb) -> ComponentStyles {
    let component = ComponentTheme::derive(ComponentThemeSeed {
        background: rgb(16, 18, 24),
        foreground: rgb(220, 222, 230),
        accent,
        info: rgb(90, 190, 250),
        success: rgb(120, 220, 110),
        warning: rgb(240, 190, 80),
        error: rgb(250, 100, 120),
    });
    let style = |r, g, b| Style::new().fg(Color::Rgb(r, g, b));
    let syntax = bcode_syntax_render::SyntaxColor::from_tui(Color::Rgb(200, 200, 200));
    let accent_color = Color::Rgb(accent.r, accent.g, accent.b);
    ComponentStyles {
        component,
        files: bcode_tui_theme::components::FileChromeRoles::default(),
        threads: bcode_tui_theme::components::ThreadCardRoles::default(),
        diff: DiffViewerStyle {
            text: component.text,
            muted: style(110, 110, 120),
            title: component.title,
            label: component.text,
            added: style(30, 230, 90),
            removed: style(240, 60, 60),
            hunk: style(80, 160, 255),
            added_row: Style::new().bg(Color::Rgb(10, 40, 20)),
            removed_row: Style::new().bg(Color::Rgb(40, 10, 10)),
            added_emphasis: Style::new().bg(Color::Rgb(20, 70, 35)),
            removed_emphasis: Style::new().bg(Color::Rgb(70, 20, 20)),
        },
        syntax: bcode_syntax_render::SyntaxPalette {
            text: syntax,
            comment: syntax,
            keyword: syntax,
            function: syntax,
            variable: syntax,
            string: syntax,
            number: syntax,
            type_name: syntax,
            operator: syntax,
            punctuation: syntax,
            heading: syntax,
            link: syntax,
            raw: syntax,
        },
        ..bcode_tui_theme::components::component_styles(None, accent_color)
    }
}

fn area_of(scenario: &Scenario<ReviewSurface>, id: &str) -> Option<Rect> {
    scenario.surface().body_area(id)
}

/// Rows of the file panel's tree, as painted (inside the pane).
fn tree_rows(scenario: &Scenario<ReviewSurface>, screen: &Screen) -> Vec<String> {
    let visible = tree_area(scenario);
    (visible.y..visible.bottom())
        .map(|y| screen.text_in(visible, y))
        .collect()
}

/// Painted area of the tree: its layout area inside the file pane.
fn tree_area(scenario: &Scenario<ReviewSurface>) -> Rect {
    let area = area_of(scenario, TREE_ID).expect("tree");
    let pane = area_of(scenario, FILES_PANE_ID).expect("file pane");
    area.intersection(Rect::new(
        pane.x + 1,
        pane.y + 1,
        pane.width.saturating_sub(2),
        pane.height.saturating_sub(2),
    ))
}

fn tree_row<'a>(rows: &'a [String], needle: &str) -> &'a str {
    rows.iter()
        .find(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("no tree row with {needle}: {rows:#?}"))
}

/// Position of the first cell showing `needle` inside `area`.
fn find_in(screen: &Screen, area: Rect, needle: &str) -> Option<(u16, u16)> {
    (area.y..area.bottom()).find_map(|y| {
        let text = screen.text_in(area, y);
        let start = text.find(needle)?;
        // Map the byte offset inside the area's text back to a cell column.
        let mut x = area.x;
        let mut consumed = 0;
        for column in area.x..area.right() {
            if consumed >= start {
                x = column;
                break;
            }
            consumed += screen.cell(column, y).map_or(0, |cell| cell.symbol.len());
        }
        Some((x, y))
    })
}

fn cursor_file(scenario: &Scenario<ReviewSurface>) -> Option<String> {
    scenario
        .surface()
        .cursor()
        .and_then(ItemKey::file)
        .map(|file| file.path.clone())
}

fn selected_file(scenario: &Scenario<ReviewSurface>) -> Option<String> {
    let surface = scenario.surface();
    let id = surface.tree_selection()?;
    surface
        .file_tree()?
        .file_of(&id)
        .map(|file| file.path.clone())
}

#[test]
fn styles_follow_semantic_roles_under_a_theme() {
    let fixture = fixture();
    let (mut scenario, _) = open(&fixture, fixture_branch_compare(), 160, 40);
    scenario.set_theme(Some(&host_theme(rgb(190, 120, 250))));
    let screen = stable(&mut scenario);
    let theme = *scenario.surface().theme();
    let tree = tree_area(&scenario);
    // File status letters in the tree use the status roles, which differ by status.
    let status = |needle: &str| {
        let (x, y) = find_in(&screen, tree, needle).unwrap_or_else(|| panic!("{needle}"));
        screen.style_at(x, y).fg
    };
    let statuses = [
        (status("A new_000.rs"), theme.status_added.fg),
        (status("D deleted_000"), theme.status_deleted.fg),
        (status("M unicode.txt"), theme.status_modified.fg),
        (status("R renamed_000"), theme.status_renamed.fg),
    ];
    for (index, (painted, role)) in statuses.iter().enumerate() {
        assert_eq!(painted, role);
        assert!(
            statuses[index + 1..]
                .iter()
                .all(|(other, _)| other != painted)
        );
    }
    // Counts use the diff's added and removed colors.
    let (x, y) = find_in(&screen, tree, "+40").unwrap();
    assert_eq!(screen.style_at(x, y).fg, Some(Color::Rgb(30, 230, 90)));
    let (x, y) = find_in(&screen, tree, "−40").unwrap();
    assert_eq!(screen.style_at(x, y).fg, Some(Color::Rgb(240, 60, 60)));
    // The focused pane's border differs from the others; focus moves with tab.
    let border = |scenario: &Scenario<ReviewSurface>, screen: &Screen, id: &str| {
        let area = area_of(scenario, id).unwrap();
        screen.style_at(area.x + 2, area.bottom() - 1).fg
    };
    assert_eq!(
        border(&scenario, &screen, REVIEW_PANE_ID),
        theme.border_focused.fg
    );
    assert_eq!(border(&scenario, &screen, FILES_PANE_ID), theme.border.fg);
    scenario.key("tab");
    let screen = stable(&mut scenario);
    assert_eq!(scenario.surface().focus(), Focus::Files);
    assert_eq!(border(&scenario, &screen, REVIEW_PANE_ID), theme.border.fg);
    assert_eq!(
        border(&scenario, &screen, FILES_PANE_ID),
        theme.border_focused.fg
    );
    // Hunk headers use their own role, filled across the row.
    let list = scenario.surface().list_area().unwrap();
    let (x, y) = find_in(&screen, list, "@@").unwrap();
    assert_eq!(screen.style_at(x, y).fg, theme.hunk.fg);
    assert_eq!(screen.style_at(x, y).bg, theme.hunk_fill.bg);
    assert_eq!(screen.style_at(list.right() - 3, y).bg, theme.hunk_fill.bg);
    // A different host theme restyles everything without reopening.
    scenario.set_theme(Some(&host_theme(rgb(250, 160, 40))));
    let screen = stable(&mut scenario);
    let restyled = *scenario.surface().theme();
    assert_ne!(restyled.border_focused, theme.border_focused);
    assert_eq!(
        border(&scenario, &screen, FILES_PANE_ID),
        restyled.border_focused.fg
    );
}

#[test]
fn surfaces_focus_and_selection_follow_the_visual_tiers() {
    let fixture = fixture();
    let (mut scenario, _) = open(&fixture, fixture_branch_compare(), 160, 40);
    scenario.set_theme(Some(&host_theme(rgb(190, 120, 250))));
    let screen = stable(&mut scenario);
    let theme = *scenario.surface().theme();
    let tiers = theme.tiers;
    // No cell falls back to the terminal's default background: everything sits on a tier.
    for y in 0..screen.height() {
        for x in 0..screen.width() {
            assert!(
                screen.style_at(x, y).bg.is_some(),
                "({x}, {y}) has no background\n{}",
                screen.text()
            );
        }
    }
    // The header and footer bars are the `surface_alt` tier from edge to edge.
    for y in [0, screen.height() - 1] {
        assert_eq!(screen.style_at(0, y).bg, tiers.surface_alt.bg);
        assert_eq!(
            screen.style_at(screen.width() - 1, y).bg,
            tiers.surface_alt.bg
        );
    }
    // The focused pane's title takes the accent; others are muted. Counts sit at the right of
    // the title row.
    let title = |scenario: &Scenario<ReviewSurface>, screen: &Screen, id: &str, needle: &str| {
        let area = area_of(scenario, id).unwrap();
        let row = Rect::new(area.x, area.y, area.width, 1);
        let (x, y) = find_in(screen, row, needle).unwrap_or_else(|| panic!("{needle}"));
        screen.style_at(x, y)
    };
    assert_eq!(
        title(&scenario, &screen, REVIEW_PANE_ID, "Review").fg,
        tiers.accent.fg
    );
    assert_eq!(
        title(&scenario, &screen, FILES_PANE_ID, "Files").fg,
        tiers.muted.fg
    );
    let review = area_of(&scenario, REVIEW_PANE_ID).unwrap();
    let (x, _) = find_in(
        &screen,
        Rect::new(review.x, review.y, review.width, 1),
        "15 files",
    )
    .expect("file count");
    assert!(x > review.x + review.width / 2, "{}", screen.text());
    // The selected file carries the selection bar in the tree's first column, in the accent;
    // other rows leave that column blank.
    let tree = tree_area(&scenario);
    let selected = selected_file(&scenario).expect("selected file");
    let name = selected.rsplit('/').next().unwrap_or(&selected);
    let (_, row) = find_in(&screen, tree, name).expect("selected row");
    let bar = screen.cell(tree.x, row).expect("bar cell");
    assert_eq!(bar.symbol.as_str(), theme.icons.selection_bar);
    assert_eq!(bar.style.fg, tiers.accent.fg);
    for y in (tree.y..tree.bottom()).filter(|y| *y != row) {
        assert_eq!(
            screen.cell(tree.x, y).map(|cell| cell.symbol.as_str()),
            Some(" ")
        );
    }
    let screen = {
        scenario.key("tab");
        stable(&mut scenario)
    };
    assert_eq!(
        title(&scenario, &screen, FILES_PANE_ID, "Files").fg,
        tiers.accent.fg
    );
    assert_eq!(
        title(&scenario, &screen, REVIEW_PANE_ID, "Review").fg,
        tiers.muted.fg
    );
    // Overlays sit on the overlay tier, their selected row marked by the bar.
    scenario.key("tab");
    scenario.key(":");
    let screen = stable(&mut scenario);
    let (x, y) = screen
        .find("Move down")
        .map(|point| (point.x, point.y))
        .expect("palette");
    assert_eq!(screen.style_at(x, y).bg, tiers.overlay.bg);
    let bar = screen.cell(x - 2, y).expect("palette bar");
    assert_eq!(bar.symbol.as_str(), theme.icons.selection_bar);
    assert_eq!(bar.style.fg, tiers.accent.fg);
}

#[test]
fn file_tree_shows_statuses_counts_and_directory_aggregates() {
    let fixture = fixture();
    let (scenario, screen) = open(&fixture, fixture_branch_compare(), 160, 40);
    let rows = tree_rows(&scenario, &screen);
    assert!(
        tree_row(&rows, "A new_000.rs").contains("+40 −0"),
        "{rows:#?}"
    );
    assert!(
        tree_row(&rows, "D deleted_000").contains("+0 −40"),
        "{rows:#?}"
    );
    assert!(
        tree_row(&rows, "R renamed_000").contains("+1 −1"),
        "{rows:#?}"
    );
    assert!(tree_row(&rows, "M blob.bin").contains("bin"), "{rows:#?}");
    // Unviewed files carry the unviewed mark; directories aggregate their files.
    assert!(tree_row(&rows, "A new_000.rs").ends_with('○'), "{rows:#?}");
    let modules = tree_row(&rows, "src/mod_000/");
    assert!(
        modules.contains("+24 −24") && modules.ends_with("0/4"),
        "{rows:#?}"
    );
    // Change bars grow with the change; binary files show none.
    assert!(!tree_row(&rows, "M blob.bin").contains('■'), "{rows:#?}");
    assert!(
        tree_row(&rows, "A new_000.rs").contains("■■■··"),
        "{rows:#?}"
    );
    assert!(tree_row(&rows, "M huge.rs").contains("■■■■■"), "{rows:#?}");
    assert_eq!(scenario.surface().file_tree().unwrap().file_count(), 15);
}

#[test]
fn tree_selection_and_document_cursor_follow_each_other() {
    let fixture = fixture();
    let (mut scenario, _) = open(&fixture, fixture_branch_compare(), 160, 40);
    // Document → tree: walking files moves the tree selection.
    for _ in 0..3 {
        scenario.key("n");
        stable(&mut scenario);
        assert_eq!(selected_file(&scenario), cursor_file(&scenario));
    }
    // Tree → document: moving in the focused tree moves the cursor to that file's header.
    scenario.key("tab");
    let before = cursor_file(&scenario);
    for _ in 0..4 {
        scenario.key("j");
        let screen = stable(&mut scenario);
        if let Some(selected) = selected_file(&scenario) {
            assert_eq!(Some(&selected), cursor_file(&scenario).as_ref());
            assert!(matches!(
                scenario.surface().cursor(),
                Some(ItemKey::FileHeader(_))
            ));
            let list = scenario.surface().list_area().unwrap();
            assert!(
                screen.text_in(list, list.y).contains(&selected),
                "{}",
                screen.text()
            );
        }
    }
    assert_ne!(cursor_file(&scenario), before);
    assert_eq!(scenario.surface().focus(), Focus::Files);
    // Clicking a file row jumps there.
    let screen = stable(&mut scenario);
    let tree = tree_area(&scenario);
    let (x, y) = find_in(&screen, tree, "huge.rs").unwrap();
    scenario.click(x, y);
    let screen = stable(&mut scenario);
    assert_eq!(cursor_file(&scenario).as_deref(), Some(LARGE_FILE));
    assert_eq!(selected_file(&scenario).as_deref(), Some(LARGE_FILE));
    let list = scenario.surface().list_area().unwrap();
    assert!(screen.text_in(list, list.y).contains(LARGE_FILE));
    // Scrolling the document moves the tree highlight with the cursor's file.
    for _ in 0..200 {
        scenario.mouse(MouseEventKind::ScrollDown, list.x + 5, list.y + 5);
        stable(&mut scenario);
        if cursor_file(&scenario).as_deref() != Some(LARGE_FILE) {
            break;
        }
    }
    assert_ne!(cursor_file(&scenario).as_deref(), Some(LARGE_FILE));
    assert_eq!(selected_file(&scenario), cursor_file(&scenario));
    // Clicking a directory row collapses it; while the cursor is inside it, the directory
    // stands for the cursor's file in the tree.
    let screen = stable(&mut scenario);
    let (x, y) = find_in(&screen, tree, "large/").unwrap();
    scenario.click(x, y);
    let screen = stable(&mut scenario);
    assert!(
        !tree_rows(&scenario, &screen)
            .iter()
            .any(|row| row.contains("huge.rs"))
    );
    scenario.key("esc");
    assert_eq!(scenario.surface().focus(), Focus::Review);
    scenario.key("g");
    stable(&mut scenario);
    while cursor_file(&scenario).as_deref() != Some(LARGE_FILE) {
        scenario.key("n");
        stable(&mut scenario);
    }
    let selection = scenario.surface().tree_selection().unwrap();
    let tree = scenario.surface().file_tree().unwrap();
    assert!(tree.is_expandable(&selection), "{selection}");
    assert_eq!(tree.first_file_under(&selection).unwrap().path, LARGE_FILE);
}

#[test]
fn panels_degrade_explicitly_at_narrow_widths() {
    let fixture = fixture();
    let (mut scenario, screen) = open(&fixture, fixture_branch_compare(), 200, 40);
    assert!(area_of(&scenario, FILES_PANE_ID).is_some());
    assert!(screen.row_text(0).contains("Publish"), "{}", screen.text());
    // The file panel collapses automatically below its width.
    scenario.resize(FILES_AUTO_BODY - 1, 40);
    let screen = stable(&mut scenario);
    assert!(
        area_of(&scenario, FILES_PANE_ID).is_none(),
        "{}",
        screen.text()
    );
    let list = scenario.surface().list_area().unwrap();
    assert_eq!(list.x, 1, "the document takes the width inside its border");
    // The user can open it again down to its minimum body width.
    scenario.key("t");
    stable(&mut scenario);
    assert!(area_of(&scenario, FILES_PANE_ID).is_some());
    scenario.resize(FILES_MIN_BODY - 1, 40);
    stable(&mut scenario);
    assert!(area_of(&scenario, FILES_PANE_ID).is_none());
    scenario.resize(FILES_AUTO_BODY - 10, 40);
    stable(&mut scenario);
    assert!(
        area_of(&scenario, FILES_PANE_ID).is_some(),
        "the choice is kept"
    );
    // The header drops parts in its elision order: never more parts at a narrower width,
    // the publish button first, the viewed count last.
    let mut previous = usize::MAX;
    for width in [200_u16, 120, 90, 70, 50, 36, 24] {
        scenario.resize(width, 30);
        let screen = stable(&mut scenario);
        let header = screen.row_text(0);
        let parts = ["Publish", "drafts", "+175", "▱", "viewed"]
            .iter()
            .filter(|part| header.contains(*part))
            .count()
            + usize::from(header.contains("unified") || header.contains("split"));
        assert!(parts <= previous, "{width}: {header}");
        previous = parts;
        if width <= 90 {
            assert!(!header.contains("Publish"), "{width}: {header}");
        }
        if width >= 50 {
            assert!(header.contains("viewed"), "{width}: {header}");
        }
    }
    // Focus leaves a panel that a resize hides.
    scenario.resize(200, 30);
    scenario.key("f");
    stable(&mut scenario);
    assert_eq!(scenario.surface().focus(), Focus::Filter);
    scenario.resize(FILES_MIN_BODY - 1, 30);
    stable(&mut scenario);
    assert_eq!(scenario.surface().focus(), Focus::Review);
    // Tiny bodies drop all pane chrome: the document takes the whole body.
    scenario.resize(20, 10);
    let screen = stable(&mut scenario);
    assert!(area_of(&scenario, REVIEW_PANE_ID).is_none());
    let list = scenario.surface().list_area().unwrap();
    assert_eq!((list.x, list.width), (0, 20), "{}", screen.text());
}

#[test]
fn filter_narrows_the_file_tree_and_opens_matches() {
    let fixture = fixture();
    let (mut scenario, _) = open(&fixture, fixture_branch_compare(), 160, 40);
    scenario.key("f");
    assert_eq!(scenario.surface().focus(), Focus::Filter);
    for character in "unicode".chars() {
        scenario.key(&character.to_string());
    }
    let screen = stable(&mut scenario);
    // Typing never triggers review actions (`n`, `e`, and `d` are bound).
    assert_eq!(scenario.surface().focus(), Focus::Filter);
    let rows = tree_rows(&scenario, &screen);
    let shown: Vec<&String> = rows.iter().filter(|row| !row.trim().is_empty()).collect();
    assert_eq!(shown.len(), 2, "{rows:#?}");
    assert!(
        shown[0].contains("docs/") && shown[1].contains("unicode.txt"),
        "{rows:#?}"
    );
    // The title row names the panel and, at its right, how many files match.
    let files = area_of(&scenario, FILES_PANE_ID).unwrap();
    let title = screen.text_in(files, files.y);
    assert!(
        title.contains(" Files ") && title.contains(" 1/15 "),
        "{}",
        screen.text()
    );
    // Enter opens the first match and moves to the tree.
    scenario.key("enter");
    stable(&mut scenario);
    assert_eq!(scenario.surface().focus(), Focus::Files);
    assert_eq!(cursor_file(&scenario).as_deref(), Some(UNICODE_FILE));
    // Escape in the filter clears it.
    scenario.key("f");
    scenario.key("esc");
    let screen = stable(&mut scenario);
    assert_eq!(scenario.surface().file_tree().unwrap().matches(), 15);
    assert!(
        tree_rows(&scenario, &screen)
            .iter()
            .any(|row| row.contains("huge.rs"))
    );
}

#[test]
fn hit_regions_agree_with_painting_after_resize() {
    let fixture = fixture();
    let (mut scenario, _) = open(&fixture, fixture_branch_compare(), 160, 40);
    for (width, height) in [
        (160_u16, 40_u16),
        (110, 30),
        (130, 24),
        (200, 50),
        (100, 12),
        (70, 20),
    ] {
        scenario.resize(width, height);
        let screen = stable(&mut scenario);
        let frame = Rect::new(0, 0, width, height);
        let tree = scenario.surface().file_tree().unwrap();
        for hit in screen.hits() {
            assert!(hit.area.width > 0 && hit.area.height > 0, "{hit:?}");
            assert_eq!(
                frame.intersection(hit.area),
                hit.area,
                "{width}x{height} {hit:?}"
            );
            let id = hit.id.as_str();
            // Every tree row region covers that row's painted name.
            if let Some(item) = id.strip_prefix(&format!("{TREE_ID}."))
                && let Some(file) = tree.file_of(item)
            {
                let name = file.path.rsplit('/').next().unwrap();
                let text = screen.text_in(hit.area, hit.area.y);
                let prefix: String = name.chars().take(6).collect();
                assert!(text.contains(&prefix), "{width}x{height} {name}: {text:?}");
            }
            // The divider handle sits on the review pane's left border.
            if id.ends_with(".divider.0") {
                let symbol = screen
                    .cell(hit.area.x, hit.area.y + 1)
                    .unwrap()
                    .symbol
                    .clone();
                assert_eq!(symbol, "│", "{width}x{height} {hit:?}");
            }
            if id.ends_with(".action.collapse") {
                assert!(
                    screen.text_in(hit.area, hit.area.y).contains('«'),
                    "{hit:?}"
                );
            }
            if id.ends_with(".publish.primary") {
                assert_eq!(screen.text_in(hit.area, hit.area.y), " Publish");
            }
        }
        // No region outlives what is painted: a hidden file panel leaves no tree regions.
        let files_shown = area_of(&scenario, FILES_PANE_ID).is_some();
        assert_eq!(
            screen
                .hits()
                .iter()
                .any(|hit| hit.id.as_str().starts_with(TREE_ID)),
            files_shown,
            "{width}x{height}"
        );
    }
    // The hide action really hides the panel.
    scenario.resize(160, 40);
    let screen = stable(&mut scenario);
    let action = screen
        .hits()
        .iter()
        .find(|hit| hit.id.as_str().ends_with(".action.collapse"))
        .unwrap()
        .area;
    scenario.click(action.x, action.y);
    stable(&mut scenario);
    assert!(area_of(&scenario, FILES_PANE_ID).is_none());
}

fn saved_workspace(
    fixture: &GitFixture,
    presentation: Option<bcode_code_review_models::ReviewPresentationState>,
) -> bcode_code_review_models::ReviewWorkspace {
    bcode_code_review_models::ReviewWorkspace {
        id: "saved-review".to_owned(),
        presentation,
        ..bcode_code_review_models::ReviewWorkspace::from_target(
            fixture.root().to_path_buf(),
            fixture_branch_compare(),
        )
    }
}

#[test]
fn panel_sizes_persist_to_presentation_state() {
    let fixture = fixture();
    let mut scenario = Scenario::new(
        surface(
            &fixture,
            fixture_branch_compare(),
            ReviewViewConfig::default(),
        )
        .with_workspace(saved_workspace(&fixture, None)),
        160,
        40,
    );
    stable(&mut scenario);
    let files = area_of(&scenario, FILES_PANE_ID).unwrap();
    // Drag the divider (the review pane's left border) four columns right.
    let divider = area_of(&scenario, REVIEW_PANE_ID).unwrap().x;
    scenario.mouse(MouseEventKind::Down(MouseButton::Left), divider, 10);
    scenario.mouse(MouseEventKind::Drag(MouseButton::Left), divider + 4, 10);
    scenario.mouse(MouseEventKind::Up(MouseButton::Left), divider + 4, 10);
    stable(&mut scenario);
    let widened = area_of(&scenario, FILES_PANE_ID).unwrap();
    assert_eq!(widened.width, files.width + 4);
    let panels = scenario.surface().presentation().panels;
    assert_eq!(panels.file_panel_width, Some(widened.width));
    // Keyboard resizing and hiding persist too.
    scenario.key(">");
    stable(&mut scenario);
    assert_eq!(
        scenario.surface().presentation().panels.file_panel_width,
        Some(widened.width + 4)
    );
    scenario.key("t");
    stable(&mut scenario);
    assert_eq!(
        scenario.surface().presentation().panels.file_panel_visible,
        Some(false)
    );
    // The in-process backend persists to the real workspace store: the save succeeds.
    stable(&mut scenario);
    assert!(
        !scenario
            .surface()
            .toasts()
            .iter()
            .any(|toast| toast.starts_with("Could not save")),
        "{:?}",
        scenario.surface().toasts()
    );
    // A new surface over the saved state restores the panels.
    let restored = scenario.surface().presentation();
    let mut scenario = Scenario::new(
        surface(
            &fixture,
            fixture_branch_compare(),
            ReviewViewConfig::default(),
        )
        .with_workspace(saved_workspace(&fixture, Some(restored))),
        160,
        40,
    );
    stable(&mut scenario);
    assert!(area_of(&scenario, FILES_PANE_ID).is_none());
    scenario.key("t");
    stable(&mut scenario);
    assert_eq!(
        area_of(&scenario, FILES_PANE_ID).unwrap().width,
        widened.width + 4
    );
    // State from a newer schema is dropped, not read as this version's.
    let mut newer = scenario.surface().presentation();
    newer.schema_version += 1;
    newer.panels.file_panel_visible = Some(false);
    let surface = surface(
        &fixture,
        fixture_branch_compare(),
        ReviewViewConfig::default(),
    )
    .with_workspace(saved_workspace(&fixture, Some(newer)));
    assert_eq!(
        surface.presentation(),
        bcode_code_review_models::ReviewPresentationState::default()
    );
}

#[test]
fn footer_hints_follow_focus_and_the_cursor_item() {
    let fixture = fixture();
    let (mut scenario, screen) = open(&fixture, fixture_branch_compare(), 200, 40);
    let footer = |screen: &Screen| screen.row_text(screen.height() - 1);
    assert!(footer(&screen).contains("n file"), "{}", footer(&screen));
    assert!(!footer(&screen).contains("expand"), "{}", footer(&screen));
    // On a gap the expansion hints come first.
    let doc = scenario.surface().document().unwrap();
    let gap = (0..doc.len())
        .find(|index| matches!(doc.item(*index), Some(ReviewItem::Gap(_))))
        .unwrap();
    let stops = super::stops_between(doc, 0, gap);
    for digit in stops.to_string().chars() {
        scenario.key(&digit.to_string());
    }
    scenario.key("j");
    let screen = stable(&mut scenario);
    assert!(
        matches!(scenario.surface().cursor(), Some(ItemKey::Gap { .. })),
        "{:?}",
        scenario.surface().cursor()
    );
    assert!(
        footer(&screen).starts_with(" e expand"),
        "{}",
        footer(&screen)
    );
    scenario.key("tab");
    let screen = stable(&mut scenario);
    assert!(
        footer(&screen).contains("enter open"),
        "{}",
        footer(&screen)
    );
    scenario.key("f");
    let screen = stable(&mut scenario);
    assert!(footer(&screen).contains("esc clear"), "{}", footer(&screen));
    // Narrow footers drop whole hints, never cutting one.
    scenario.key("esc");
    scenario.key("esc");
    scenario.resize(40, 20);
    let screen = stable(&mut scenario);
    let text = footer(&screen);
    assert!(!text.contains('…'), "{text}");
    assert!(
        ["expand ↓", "expand ↑", "all", "file", "hunk", "change"]
            .iter()
            .any(|label| text.ends_with(label)),
        "{text}"
    );
}

#[test]
fn ascii_icons_replace_unicode_glyphs() {
    let fixture = fixture();
    let config = ReviewViewConfig {
        icons: Some(IconStyle::Ascii),
        ..ReviewViewConfig::default()
    };
    let mut scenario = Scenario::new(surface(&fixture, fixture_branch_compare(), config), 160, 40);
    let screen = stable(&mut scenario);
    let rows = tree_rows(&scenario, &screen);
    let row = tree_row(&rows, "A new_000.rs");
    assert!(
        row.contains("+40 -0") && row.contains("###..") && row.ends_with('o'),
        "{row}"
    );
    assert!(
        screen.row_text(0).contains("viewed 0/15  ----------"),
        "{}",
        screen.row_text(0)
    );
    let text = screen.text();
    for glyph in ["✓", "○", "−", "■", "▱", "▍", "┄", "↓", "⌕", "«", "·"] {
        assert!(!text.contains(glyph), "{glyph}\n{text}");
    }
}
