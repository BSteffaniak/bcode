//! Screen behavior of the V2 review surface: loading, scrolling with sticky headers, context
//! expansion, layout modes, resize anchoring, tiny widths, and wide-grapheme alignment.

use super::{open, stable, surface};
use crate::v2::document::{FileKey, ItemKey, LayoutMode, ReviewItem};
use crate::v2::testing::fixture::{FixtureSpec, GitFixture, LARGE_FILE, UNICODE_FILE};
use crate::v2::testing::fixture_branch_compare;
use crate::v2::testing::harness::{Scenario, Screen};
use crate::v2::tui::ReviewSurfaceV2;
use crate::v2::tui::config::ReviewViewConfig;
use bmux_tui::event::{Event, FocusEvent, MouseEventKind};
use bmux_tui::geometry::Rect;

fn fixture() -> GitFixture {
    GitFixture::generate(FixtureSpec::small())
}

fn cursor(scenario: &Scenario<ReviewSurfaceV2>) -> ItemKey {
    scenario.surface().cursor().cloned().expect("cursor")
}

fn cursor_index(scenario: &Scenario<ReviewSurfaceV2>) -> usize {
    let surface = scenario.surface();
    surface
        .document()
        .and_then(|doc| doc.index_of(surface.cursor()?))
        .expect("cursor index")
}

/// Document list area of the last frame.
fn list(scenario: &Scenario<ReviewSurfaceV2>) -> Rect {
    scenario.surface().list_area().expect("list area")
}

/// Text of the document list's `row`-th row (zero is its top row).
fn list_row(scenario: &Scenario<ReviewSurfaceV2>, screen: &Screen, row: u16) -> String {
    let area = list(scenario);
    screen.text_in(area, area.y + row)
}

fn file_of(key: &ItemKey) -> Option<String> {
    key.file().map(|file| file.path.clone())
}

/// Move the cursor to the item at `index` with a count prefix.
fn move_to(scenario: &mut Scenario<ReviewSurfaceV2>, index: usize) -> Screen {
    let current = cursor_index(scenario);
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

/// Index of the first item of `path` matching `predicate`.
fn find_item(
    scenario: &Scenario<ReviewSurfaceV2>,
    path: &str,
    predicate: impl Fn(&ReviewItem) -> bool,
) -> Option<usize> {
    let doc = scenario.surface().document()?;
    (0..doc.len()).find(|index| {
        doc.item(*index).is_some_and(|item| {
            item.file().is_some_and(|file| file.path == path) && predicate(item)
        })
    })
}

/// Press `n` until the cursor is on the header of `path`.
fn goto_file(scenario: &mut Scenario<ReviewSurfaceV2>, path: &str) -> Screen {
    scenario.key("g");
    let mut screen = stable(scenario);
    for _ in 0..64 {
        if matches!(cursor(scenario), ItemKey::FileHeader(file) if file.path == path) {
            return screen;
        }
        scenario.key("n");
        screen = stable(scenario);
    }
    panic!("no file {path}");
}

fn assert_hits_inside(screen: &Screen, context: &str) {
    let frame = Rect::new(0, 0, screen.width(), screen.height());
    for hit in screen.hits() {
        assert!(
            hit.area.width > 0 && hit.area.height > 0,
            "{context}: empty hit {hit:?}"
        );
        assert_eq!(
            frame.intersection(hit.area),
            hit.area,
            "{context}: hit outside the frame {hit:?}"
        );
    }
}

#[test]
fn loading_state_shows_until_the_index_arrives_without_blocking_input() {
    let fixture = fixture();
    let mut scenario = Scenario::new(
        surface(
            &fixture,
            fixture_branch_compare(),
            ReviewViewConfig::default(),
        ),
        100,
        20,
    );
    assert!(scenario.render().contains("Opening review"));
    // Polling spawns the open task but runs nothing until the harness executes it.
    scenario.surface_mut();
    let action = scenario.key("j");
    assert_eq!(action.action, bcode_plugin_sdk::tui::PluginTuiAction::None);
    let screen = stable(&mut scenario);
    assert!(!screen.contains("Opening review"), "{}", screen.text());
    assert!(screen.contains("15 files"), "{}", screen.text());
    // Frames issue no service calls: rendering again without settling spawns nothing.
    let spawned = scenario.host().spawned_tasks();
    scenario.render();
    scenario.render();
    assert_eq!(scenario.host().spawned_tasks(), spawned);
}

#[test]
fn next_file_walks_every_file_to_the_end_with_the_right_sticky_header() {
    let fixture = fixture();
    let (mut scenario, _) = open(&fixture, fixture_branch_compare(), 100, 30);
    let total = scenario.surface().document().unwrap().file_keys().count();
    let mut visited = Vec::new();
    for _ in 0..total + 2 {
        scenario.key("n");
        let screen = stable(&mut scenario);
        let key = cursor(&scenario);
        let ItemKey::FileHeader(file) = &key else {
            panic!("cursor not on a file header: {key:?}");
        };
        if visited.last() != Some(&file.path) {
            visited.push(file.path.clone());
        }
        // The file header is on screen: at the first list row, or lower at the end of the
        // review where the content cannot scroll further.
        assert!(
            screen.contains(&file.path),
            "{}\n{}",
            file.path,
            screen.text()
        );
        let at_end = scenario.surface().top_key().is_some()
            && !list_row(&scenario, &screen, 0).contains(&file.path);
        if at_end {
            let rest = total - visited.len();
            assert!(rest <= 3, "{}\n{}", file.path, screen.text());
        }
    }
    // The first file is where the review starts; `n` visits every other one in order.
    assert_eq!(visited.len(), total - 1, "{visited:?}");
    // Inside a file, scrolling down past its header keeps the header pinned.
    scenario.key("g");
    stable(&mut scenario);
    let large = find_item(&scenario, LARGE_FILE, |item| {
        matches!(item, ReviewItem::FileHeader(_))
    })
    .unwrap();
    move_to(&mut scenario, large);
    scenario.key("6");
    scenario.key("0");
    scenario.key("j");
    let screen = stable(&mut scenario);
    assert_eq!(file_of(&cursor(&scenario)).as_deref(), Some(LARGE_FILE));
    let pinned = scenario.surface().pinned_keys();
    assert!(
        matches!(pinned.first(), Some(ItemKey::FileHeader(file)) if file.path == LARGE_FILE),
        "{pinned:?}\n{}",
        screen.text()
    );
    assert!(
        list_row(&scenario, &screen, 0).contains(LARGE_FILE),
        "{}",
        screen.text()
    );
    // Jumping to a hunk places its header right below the pinned file header; scrolling the
    // viewport a little then pins the hunk header below the file header.
    scenario.key("}");
    let screen = stable(&mut scenario);
    assert!(matches!(cursor(&scenario), ItemKey::HunkHeader { .. }));
    assert!(
        list_row(&scenario, &screen, 0).contains(LARGE_FILE),
        "{}",
        screen.text()
    );
    assert!(
        list_row(&scenario, &screen, 1).contains("@@"),
        "{}",
        screen.text()
    );
    let area = list(&scenario);
    scenario.mouse(MouseEventKind::ScrollDown, area.x + 10, area.y + 10);
    let screen = stable(&mut scenario);
    let pinned = scenario.surface().pinned_keys();
    assert!(
        matches!(pinned.get(1), Some(ItemKey::HunkHeader { .. })),
        "{pinned:?}\n{}",
        screen.text()
    );
    assert!(
        list_row(&scenario, &screen, 0).contains(LARGE_FILE),
        "{}",
        screen.text()
    );
    assert!(
        list_row(&scenario, &screen, 1).contains("@@"),
        "{}",
        screen.text()
    );
}

#[test]
fn paging_reaches_the_end_of_the_review() {
    let fixture = fixture();
    let (mut scenario, _) = open(&fixture, fixture_branch_compare(), 100, 30);
    let mut previous = String::new();
    for _ in 0..400 {
        scenario.key("pagedown");
        let screen = stable(&mut scenario).text();
        if screen == previous {
            break;
        }
        previous = screen;
    }
    let screen = stable(&mut scenario);
    let doc = scenario.surface().document().unwrap();
    let last_file = doc.file_keys().last().unwrap().path.clone();
    assert!(screen.contains(&last_file), "{}", screen.text());
    // Every file is resident or was loaded on the way; nothing is still loading.
    assert!(!screen.contains("loading"), "{}", screen.text());
    // `G` lands on the last item.
    scenario.key("shift+g");
    stable(&mut scenario);
    let doc = scenario.surface().document().unwrap();
    assert_eq!(cursor_index(&scenario), doc.len() - 1);
}

fn expand_in_order(fixture: &GitFixture, order: &[&str]) -> (Screen, Vec<(u32, u32)>) {
    let (mut scenario, _) = open(fixture, fixture_branch_compare(), 100, 40);
    let gap = find_item(
        &scenario,
        LARGE_FILE,
        |item| matches!(item, ReviewItem::Gap(gap) if gap.end.is_some() && gap.start > 1),
    )
    .expect("gap between hunks");
    move_to(&mut scenario, gap);
    assert!(matches!(cursor(&scenario), ItemKey::Gap { .. }));
    for key in order {
        if let Some(count) = key
            .strip_suffix('e')
            .filter(|count| !count.is_empty() && count.chars().all(|c| c.is_ascii_digit()))
        {
            for digit in count.chars() {
                scenario.key(&digit.to_string());
            }
            scenario.key("e");
        } else {
            scenario.key(key);
        }
        stable(&mut scenario);
    }
    let file = scenario
        .surface()
        .document()
        .unwrap()
        .file_keys()
        .find(|file| file.path == LARGE_FILE)
        .unwrap()
        .clone();
    let revealed = scenario
        .surface()
        .document()
        .unwrap()
        .revealed(&file)
        .map(|set| set.iter().collect())
        .unwrap_or_default();
    // Compare the same region of the review: from the top, jump to the large file.
    scenario.key("g");
    stable(&mut scenario);
    let header = find_item(&scenario, LARGE_FILE, |item| {
        matches!(item, ReviewItem::FileHeader(_))
    })
    .unwrap();
    move_to(&mut scenario, header);
    scenario.key("shift+e");
    (stable(&mut scenario), revealed)
}

#[test]
fn expanding_a_gap_in_either_order_gives_the_same_screen() {
    let fixture = fixture();
    let (down_then_up, down_up) = expand_in_order(&fixture, &["e", "shift+e"]);
    let (up_then_down, up_down) = expand_in_order(&fixture, &["shift+e", "e"]);
    assert_eq!(down_up, up_down);
    assert_ne!(down_up, Vec::new());
    assert_eq!(down_then_up.text(), up_then_down.text());
    // Count prefixes reveal that many lines.
    let (_, counted) = expand_in_order(&fixture, &["5e", "3e"]);
    let total: u32 = counted.iter().map(|(start, end)| end - start + 1).sum();
    assert_eq!(total, 8, "{counted:?}");
}

/// Text of the new (else old) side of a diff line item.
fn item_text(item: &ReviewItem) -> Option<String> {
    match item {
        ReviewItem::Line(line) => line.content.text().map(str::to_owned),
        ReviewItem::LinePair(pair) => pair
            .new
            .as_ref()
            .or(pair.old.as_ref())
            .and_then(|side| side.content.text().map(str::to_owned)),
        _ => None,
    }
}

#[test]
fn revealed_lines_painted_while_loading_repaint_with_their_text() {
    let fixture = fixture();
    let (mut scenario, _) = open(&fixture, fixture_branch_compare(), 100, 40);
    let doc = scenario.surface().document().unwrap();
    let (gap, before) = (0..doc.len())
        .find_map(|index| match doc.item(index) {
            Some(ReviewItem::Gap(gap)) if gap.hidden_lines().is_some_and(|hidden| hidden > 20) => {
                Some((index, gap.clone()))
            }
            _ => None,
        })
        .expect("a bounded gap wider than one step");
    let hidden_before = before.hidden_lines().unwrap();
    // Reach the gap moving up so it sits at the top of the viewport, the lines it reveals
    // below it.
    scenario.key("shift+g");
    stable(&mut scenario);
    move_to(&mut scenario, gap);
    // Reveal context above the next hunk and paint before the blob lines arrive, as a real host
    // does: the revealed rows paint as loading first, and the gap keeps its key (its first line).
    scenario.key("shift+e");
    let loading = scenario.render();
    assert!(loading.contains("⋯"), "{}", loading.text());
    let screen = stable(&mut scenario);
    let doc = scenario.surface().document().unwrap();
    let Some(ReviewItem::Gap(after)) = doc.item(gap) else {
        panic!("gap stays in place");
    };
    let remaining = after.hidden_lines().unwrap();
    assert_eq!(remaining, hidden_before - 20);
    let first = item_text(doc.item(gap + 1).unwrap()).expect("revealed line loaded");
    // Long lines wrap; their start is on one row.
    let start: String = first.trim_start().chars().take(16).collect();
    assert!(screen.contains(&start), "{first}\n{}", screen.text());
    assert!(!screen.contains("⋯"), "{}", screen.text());
    assert!(
        screen.contains(&format!("{remaining} hidden lines")),
        "{}",
        screen.text()
    );
}

#[test]
fn gap_segments_reveal_on_click_and_hits_match_their_painted_cells() {
    let fixture = fixture();
    let (mut scenario, _) = open(&fixture, fixture_branch_compare(), 100, 40);
    let doc = scenario.surface().document().unwrap();
    let gap = (0..doc.len())
        .find(|index| {
            matches!(doc.item(*index), Some(ReviewItem::Gap(gap))
                if gap.hidden_lines().is_none_or(|hidden| hidden > 20))
        })
        .expect("a gap wider than one step");
    let screen = move_to(&mut scenario, gap);
    let label = screen.find("↓ 20").expect("gap row on screen");
    let hit = screen
        .hits()
        .iter()
        .find(|hit| hit.id.as_str().ends_with(".action.1") && hit.area.y == label.y)
        .expect("down segment hit");
    assert_eq!(hit.area.x, label.x, "{hit:?}");
    assert_eq!(hit.area.width, 4);
    let before = scenario.surface().document().unwrap().len();
    scenario.click(label.x + 1, label.y);
    let after = stable(&mut scenario);
    let grown = scenario.surface().document().unwrap().len() - before;
    assert!((1..=20).contains(&grown), "{grown}\n{}", after.text());
}

#[test]
fn layout_follows_width_and_the_toggle_keeps_the_cursor_line() {
    let fixture = fixture();
    // The automatic layout follows the document's width beside the file panel.
    let (mut scenario, wide) = open(&fixture, fixture_branch_compare(), 200, 30);
    assert_eq!(
        scenario.surface().document().unwrap().layout(),
        LayoutMode::SideBySide
    );
    assert!(wide.row_text(0).contains("split"), "{}", wide.text());
    scenario.resize(80, 30);
    let narrow = stable(&mut scenario);
    assert_eq!(
        scenario.surface().document().unwrap().layout(),
        LayoutMode::Unified
    );
    assert!(narrow.row_text(0).contains("unified"), "{}", narrow.text());

    // Put the cursor on a changed line, toggle, and stay on the same source line.
    let line = find_item(&scenario, LARGE_FILE, ReviewItem::is_change).unwrap();
    move_to(&mut scenario, line);
    let before = scenario
        .surface()
        .document()
        .unwrap()
        .item(cursor_index(&scenario))
        .unwrap()
        .line_numbers()
        .unwrap();
    scenario.key("shift+s");
    stable(&mut scenario);
    let doc = scenario.surface().document().unwrap();
    assert_eq!(doc.layout(), LayoutMode::SideBySide);
    let (old, new) = doc
        .item(cursor_index(&scenario))
        .unwrap()
        .line_numbers()
        .unwrap();
    assert!(
        (before.0.is_some() && before.0 == old) || (before.1.is_some() && before.1 == new),
        "{before:?} -> {old:?}/{new:?}"
    );
}

#[test]
fn resize_keeps_the_same_top_item() {
    let fixture = fixture();
    let (mut scenario, _) = open(&fixture, fixture_branch_compare(), 110, 30);
    for _ in 0..3 {
        scenario.key("pagedown");
        stable(&mut scenario);
    }
    let top = scenario.surface().top_key().unwrap();
    for (width, height) in [(90, 30), (100, 20), (70, 45), (110, 30)] {
        scenario.resize(width, height);
        stable(&mut scenario);
        assert_eq!(
            scenario.surface().top_key().as_ref(),
            Some(&top),
            "{width}x{height}"
        );
    }
}

#[test]
fn tiny_sizes_render_without_panics_or_stale_hits() {
    let fixture = fixture();
    let (mut scenario, _) = open(&fixture, fixture_branch_compare(), 100, 30);
    let gap = find_item(&scenario, LARGE_FILE, |item| {
        matches!(item, ReviewItem::Gap(_))
    })
    .unwrap();
    move_to(&mut scenario, gap);
    for width in 0..=10 {
        for height in [0, 1, 2, 3, 4, 10] {
            scenario.resize(width, height);
            let screen = stable(&mut scenario);
            assert_hits_inside(&screen, &format!("{width}x{height}"));
            scenario.key("j");
            scenario.key("e");
            scenario.mouse(MouseEventKind::ScrollDown, 0, 0);
            scenario.click(width.saturating_sub(1), height.saturating_sub(1));
            let screen = stable(&mut scenario);
            assert_hits_inside(&screen, &format!("{width}x{height} after input"));
        }
    }
    scenario.resize(100, 30);
    let screen = stable(&mut scenario);
    assert_hits_inside(&screen, "restored");
    assert!(screen.contains("files"));
}

#[test]
fn wide_graphemes_keep_columns_aligned() {
    let fixture = fixture();
    for width in [72_u16, 150] {
        let (mut scenario, _) = open(&fixture, fixture_branch_compare(), width, 40);
        goto_file(&mut scenario, UNICODE_FILE);
        scenario.key("shift+x");
        let screen = stable(&mut scenario);
        assert!(screen.contains("漢字"), "{}", screen.text());
        let layout = scenario.surface().document().unwrap().layout();
        // Every content row of the file keeps the gutter (unified) or divider (split) column.
        let area = list(&scenario);
        let rows: Vec<u16> = (area.y..area.bottom())
            .filter(|y| {
                let text = screen.text_in(area, *y);
                text.contains("漢字") || text.contains("🦀") || text.contains("emoji")
            })
            .collect();
        assert!(!rows.is_empty(), "{}", screen.text());
        let column = |y: u16, symbol: &str| {
            (area.x..area.right())
                .find(|x| screen.cell(*x, y).is_some_and(|cell| cell.symbol == symbol))
        };
        match layout {
            LayoutMode::SideBySide => {
                let dividers: Vec<_> = rows.iter().map(|y| column(*y, "│")).collect();
                assert!(
                    dividers.iter().all(|x| x.is_some() && *x == dividers[0]),
                    "{dividers:?}\n{}",
                    screen.text()
                );
            }
            LayoutMode::Unified => {
                // The marker column sits at the same cell on every row, right before the
                // content: the gutter is shared by the whole document.
                let gutter = bmux_tui_components::diff_viewer::DiffRowGutter::new(
                    4,
                    bmux_tui_components::diff_viewer::DiffGutterShape::LineNumbersThenMarker,
                )
                .unified_width();
                let marker = area.x + u16::try_from(gutter - 2).unwrap();
                for y in &rows {
                    let symbol = screen.cell(marker, *y).map(|cell| cell.symbol.clone());
                    assert!(
                        matches!(symbol.as_deref(), Some(" " | "+" | "-")),
                        "{symbol:?} {:?}",
                        screen.row_text(*y)
                    );
                    let first = screen.cell(marker + 2, *y).map(|cell| cell.symbol.clone());
                    assert_ne!(first.as_deref(), Some(""), "{:?}", screen.row_text(*y));
                }
            }
        }
        assert_hits_inside(&screen, "unicode");
    }
}

#[test]
fn refresh_detects_working_tree_changes_and_keeps_position() {
    let fixture = fixture();
    let target = bcode_code_review_models::ReviewTarget::WorkingTreeUnstaged;
    let (mut scenario, first) = open(&fixture, target, 100, 30);
    let files = scenario.surface().document().unwrap().file_keys().count();
    scenario.key("r");
    let screen = stable(&mut scenario);
    assert!(
        screen.contains("up to date"),
        "{}\n{}",
        first.text(),
        screen.text()
    );
    // Put the cursor on a changed line of the second file; it survives the refresh.
    scenario.key("n");
    scenario.key("]");
    stable(&mut scenario);
    let before = cursor(&scenario);
    assert!(matches!(before, ItemKey::Line { .. }), "{before:?}");
    std::fs::write(fixture.root().join("refresh-new.txt"), "hello\n").unwrap();
    fixture.run(&["add", "-N", "refresh-new.txt"]);
    scenario.send(&Event::Focus(FocusEvent::Gained));
    let screen = stable(&mut scenario);
    let doc = scenario.surface().document().unwrap();
    assert_eq!(doc.file_keys().count(), files + 1, "{}", screen.text());
    let after = cursor(&scenario);
    assert!(
        after == before || after == ItemKey::FileHeader(before.file().unwrap().clone()),
        "{before:?} -> {after:?}"
    );
    assert!(doc.file_keys().any(
        |file| *file.as_ref() == FileKey::new(file.source_id.clone(), None, "refresh-new.txt")
    ));
}

#[test]
fn quit_closes_the_surface() {
    let fixture = fixture();
    let (mut scenario, _) = open(&fixture, fixture_branch_compare(), 80, 20);
    scenario.key("q");
    assert!(matches!(
        scenario.actions().last(),
        Some(bcode_plugin_sdk::tui::PluginTuiAction::Close { outcome: None })
    ));
}

#[test]
fn measurement_hints_equal_the_built_rows_of_every_item() {
    use crate::v2::tui::rows::{RowCache, RowContext, ViewParams};
    use crate::v2::tui::syntax::SyntaxStore;
    use crate::v2::tui::theme::{IconStyle, ReviewTheme};
    use bmux_tui_components::diff_viewer::{DiffGutterShape, DiffRowGutter, DiffRowOverflow};
    use std::cell::RefCell;

    let fixture = fixture();
    for layout in ["unified", "split"] {
        let (mut scenario, _) = open(&fixture, fixture_branch_compare(), 100, 200);
        if layout == "split" {
            scenario.key("shift+s");
        }
        // Reveal every file's hidden lines so expanded and loading lines are covered too.
        for path in [UNICODE_FILE, LARGE_FILE] {
            goto_file(&mut scenario, path);
            scenario.key("shift+x");
            stable(&mut scenario);
        }
        let doc = scenario.surface().document().unwrap();
        let syntax = SyntaxStore::default();
        for overflow in [DiffRowOverflow::Wrap, DiffRowOverflow::Truncate] {
            let params = ViewParams {
                theme: ReviewTheme::derive(None, IconStyle::Unicode),
                overflow,
                gutter: DiffRowGutter::new(4, DiffGutterShape::LineNumbersThenMarker),
                tab_width: 4,
                expand_step: 20,
                layout_revision: 0,
                paint_revision: 0,
                annotation_revision: 0,
            };
            let cache = RefCell::new(RowCache::default());
            let notes = crate::v2::tui::notes::ReviewNotes::default();
            let agents = crate::v2::tui::agent::AgentStreams::default();
            let context = RowContext {
                doc,
                params: &params,
                syntax: &syntax,
                cache: &cache,
                notes: &notes,
                agents: &agents,
                folds: &crate::v2::tui::rows::Folds::default(),
                composer: None,
                search: None,
            };
            for index in 0..doc.len() {
                for width in [0_u16, 1, 2, 5, 9, 14, 30, 61, 130] {
                    assert_eq!(
                        context.height(index, width),
                        Some(context.rows(index, width).height),
                        "{layout} {overflow:?} item {index} at {width}: {:?}",
                        doc.item(index)
                    );
                }
            }
        }
    }
}

#[test]
fn host_and_plugin_configuration_choose_the_layout() {
    use crate::v2::testing::harness::ScriptedHost;
    use crate::v2::tui::config::{ConfiguredLayout, ReviewViewConfig};
    use bcode_plugin_sdk::tui::PluginTuiDiffLayout;

    let fixture = fixture();
    let layout_of = |host: ScriptedHost, config: ReviewViewConfig, width: u16| {
        let mut scenario = Scenario::with_host(
            surface(&fixture, fixture_branch_compare(), config),
            host,
            width,
            30,
        );
        stable(&mut scenario);
        scenario.surface().document().unwrap().layout()
    };
    // The host's `[tui.diff_viewer]` preference applies at any width.
    assert_eq!(
        layout_of(
            ScriptedHost::new().with_diff_layout(PluginTuiDiffLayout::Unified),
            ReviewViewConfig::default(),
            200
        ),
        LayoutMode::Unified
    );
    assert_eq!(
        layout_of(
            ScriptedHost::new().with_diff_layout(PluginTuiDiffLayout::Auto { breakpoint: 90 }),
            ReviewViewConfig::default(),
            95
        ),
        LayoutMode::SideBySide
    );
    // The plugin's own configuration overrides it.
    assert_eq!(
        layout_of(
            ScriptedHost::new().with_diff_layout(PluginTuiDiffLayout::Unified),
            ReviewViewConfig {
                diff_layout: Some(ConfiguredLayout::SideBySide),
                side_by_side_breakpoint: None,
                icons: None,
                tab_width: None,
            },
            80
        ),
        LayoutMode::SideBySide
    );
}

#[test]
fn syntax_highlighting_arrives_after_content_without_moving_anything() {
    use bmux_tui::style::Color;
    let fixture = fixture();
    let (mut scenario, _) = open(&fixture, fixture_branch_compare(), 100, 40);
    let path = GitFixture::modified_path(0);
    goto_file(&mut scenario, &path);
    let keyword = Color::Rgb(86, 156, 214);
    let keyword_cell = |screen: &Screen| {
        (0..screen.height()).find_map(|y| {
            let x = screen.column_of(y, "let ")?;
            Some(screen.style_at(x, y).fg)
        })
    };
    let highlighted = stable(&mut scenario);
    assert_eq!(
        keyword_cell(&highlighted),
        Some(Some(keyword)),
        "{}",
        highlighted.text()
    );

    // A file reached for the first time renders unhighlighted, then highlighted in place.
    let other = GitFixture::modified_path(2);
    scenario.key("n");
    scenario.key("n");
    scenario.settle();
    let first = scenario.render();
    assert!(
        matches!(cursor(&scenario), ItemKey::FileHeader(file) if file.path == other),
        "{}",
        first.text()
    );
    assert!(first.contains("let "), "{}", first.text());
    assert_ne!(
        keyword_cell(&first),
        Some(Some(keyword)),
        "{}",
        first.text()
    );
    let later = stable(&mut scenario);
    assert_eq!(first.text(), later.text());
    assert_eq!(
        keyword_cell(&later),
        Some(Some(keyword)),
        "{}",
        later.text()
    );
}
