//! Behavior of screen chrome: the header row, status line placement, and composer chrome.

use bcode_tui_theme::chrome::{BorderStyle, ChromeText, StatusPlacement, TuiChrome, Visibility};
use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;

use super::*;
use crate::theme::definition::{ThemeCatalog, ThemeSelection};

fn app_with(edit: impl FnOnce(&mut TuiChrome)) -> BmuxApp {
    let catalog = ThemeCatalog::bundled().expect("bundled themes parse");
    let mut definition = catalog
        .resolve(&ThemeSelection::new("nord"))
        .expect("nord resolves");
    edit(&mut definition.chrome);
    let theme = crate::theme::resolved_definition_theme(
        Some(&definition),
        crate::theme::PENDING_AGENT_METADATA_ACCENT,
    );
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    app.preview_resolved_theme("screen-chrome-test", &theme);
    app.set_daemon_connection(crate::app::DaemonConnectionState::Connected);
    app
}

/// The prepared layout, transcript area, and painted rows of one frame of an app themed by
/// `edit`.
fn frame(
    edit: impl FnOnce(&mut TuiChrome),
    width: u16,
    height: u16,
) -> (FrameLayout, Rect, Vec<String>) {
    let mut app = app_with(edit);
    let area = Rect::new(0, 0, width, height);
    let layout = prepare_frame(&mut app, area).expect("layout");
    let transcript = transcript_area_for_frame(&app, area);
    let mut buffer = Buffer::empty(area);
    render(&mut app, &mut PaintCx::new(&mut Frame::new(&mut buffer)));
    drop(app);
    let rows = (0..height)
        .map(|y| buffer.row_symbols(y).expect("row"))
        .collect();
    (layout, transcript, rows)
}

#[test]
fn a_hidden_header_gives_its_row_to_the_transcript() {
    for (header, top) in [(Visibility::Shown, 1), (Visibility::Hidden, 0)] {
        let (layout, transcript, painted) = frame(|chrome| chrome.header = header, 60, 20);
        assert_eq!(layout.body().y, top, "{header:?}");
        assert_eq!(layout.composer().bottom(), 20);
        assert_eq!(
            transcript.y,
            layout.body().y,
            "the transcript area and the painted body agree"
        );
        assert_eq!(
            painted[0].contains("bcode"),
            header.is_shown(),
            "{painted:?}"
        );
    }
}

#[test]
fn the_status_line_sits_above_or_below_the_composer() {
    let (above, _, _) = frame(|_| {}, 60, 20);
    assert_eq!(above.status().bottom(), above.composer().y);
    assert_eq!(above.composer().bottom(), 20);

    let (below, _, _) = frame(
        |chrome| chrome.status.placement = StatusPlacement::BelowComposer,
        60,
        20,
    );
    assert_eq!(below.status().y, below.composer().bottom());
    assert_eq!(below.status().bottom(), 20);
    assert_eq!(below.body().bottom(), below.composer().y);
    assert_eq!(
        below.body().height,
        above.body().height,
        "placement moves rows, it does not change the transcript's share"
    );
}

#[test]
fn the_status_line_indent_leaves_leading_columns_blank() {
    for indent in [0_u8, 2, 8] {
        let (layout, _, painted) = frame(|chrome| chrome.status.indent = indent, 60, 20);
        let status = &painted[usize::from(layout.status().y)];
        let leading = status.chars().take_while(|cell| *cell == ' ').count();
        assert_eq!(leading, usize::from(indent), "{status:?}");
    }
    // An indent wider than a tiny terminal never paints outside it.
    let (_, _, painted) = frame(|chrome| chrome.status.indent = 8, 4, 6);
    assert!(painted.iter().all(|row| row.chars().count() == 4));
}

#[test]
fn composer_chrome_draws_rules_a_prompt_and_no_title_or_controls() {
    let (layout, _, painted) = frame(
        |chrome| {
            chrome.borders.composer = Some(BorderStyle::Rules);
            chrome.composer.prompt = ChromeText::new("❯ ");
            chrome.composer.title = Visibility::Hidden;
            chrome.composer.placeholder = ChromeText::new("");
            chrome.composer.controls = Visibility::Hidden;
        },
        40,
        12,
    );
    let composer =
        &painted[usize::from(layout.composer().y)..usize::from(layout.composer().bottom())];
    assert_eq!(composer[0], "─".repeat(40));
    assert!(composer[1].starts_with("❯ "), "{composer:?}");
    assert_eq!(composer[composer.len() - 1], "─".repeat(40));
    let text = composer.join("\n");
    for absent in ["Message", "Send", "Ask Bcode"] {
        assert!(!text.contains(absent), "{absent} in {composer:?}");
    }
}

#[test]
fn hidden_composer_controls_release_the_editor_reservation() {
    let size = bmux_tui::geometry::Size::new(40, 3);
    assert!(super::super::composer_submit::trailing_exclusion(size, Visibility::Shown) > 0);
    assert_eq!(
        super::super::composer_submit::trailing_exclusion(size, Visibility::Hidden),
        0
    );
}
