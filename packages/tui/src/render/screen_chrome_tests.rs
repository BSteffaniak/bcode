//! Behavior of screen chrome: the header row, status line placement, composer chrome, and the
//! activity line.

use bcode_tui_theme::chrome::{
    ActivityPlacement, BorderStyle, ChromeList, ChromeText, StatusPlacement, TuiChrome, Visibility,
};
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

/// A busy frame: its layout, painted rows, the activity text and shimmer styles, and the
/// activity row.
fn busy_frame(
    edit: impl FnOnce(&mut TuiChrome),
) -> (FrameLayout, Vec<String>, (Style, Style), Line) {
    let mut app = app_with(edit);
    app.set_activity_for_test(crate::activity::ActivityState::RunningTool {
        name: "shell.run".to_owned(),
    });
    let area = Rect::new(0, 0, 60, 20);
    let layout = prepare_frame(&mut app, area).expect("layout");
    let mut buffer = Buffer::empty(area);
    render(&mut app, &mut PaintCx::new(&mut Frame::new(&mut buffer)));
    let theme = TuiTheme::for_app(&app);
    let line = activity_line(&app, theme, std::time::Instant::now());
    let (_, text, shimmer) = theme.activity_styles(app.presented_theme().surfaces);
    drop(app);
    let rows = (0..20)
        .map(|y| buffer.row_symbols(y).expect("row"))
        .collect();
    (layout, rows, (text, shimmer), line)
}

#[test]
fn an_activity_line_appears_above_the_composer_only_while_busy() {
    let line_activity = |chrome: &mut TuiChrome| {
        chrome.activity.placement = ActivityPlacement::Line;
        chrome.activity.spinner = ChromeText::new("✢✳");
        chrome.activity.verbs = ChromeList::new(&["Mulling"]).expect("verbs");
    };
    let (idle, _, _) = frame(line_activity, 60, 20);
    assert!(idle.activity().is_empty(), "no activity row while idle");

    let (busy, rows, _, _) = busy_frame(line_activity);
    assert_eq!(busy.activity().height, 1);
    assert_eq!(
        busy.activity().bottom(),
        busy.status().y.min(busy.composer().y)
    );
    assert_eq!(busy.body().bottom(), busy.activity().y);
    let activity = &rows[usize::from(busy.activity().y)];
    assert!(activity.starts_with("✢ Mulling… ("), "{activity:?}");
    let status = &rows[usize::from(busy.status().y)];
    assert!(
        !status.contains("tool shell.run"),
        "the status line leaves busy activity: {status:?}"
    );

    // Without its own row, activity stays the status line's first segment.
    let (status_only, rows, _, _) = busy_frame(|_| {});
    assert!(status_only.activity().is_empty());
    assert!(rows[usize::from(status_only.status().y)].contains("tool shell.run"));
}

#[test]
fn shimmer_highlights_a_bounded_window_of_the_activity_text() {
    let (_, _, (text, shimmer), line) = busy_frame(|chrome| {
        chrome.activity.placement = ActivityPlacement::Line;
        chrome.activity.verbs = ChromeList::new(&["Mulling"]).expect("verbs");
        chrome.activity.shimmer = true;
    });
    let lit = line
        .spans
        .iter()
        .filter(|span| span.style == shimmer && span.style != text)
        .count();
    assert!(lit <= SHIMMER_CELLS, "{line:?}");
}

#[test]
fn status_segments_select_and_order_the_status_line() {
    use bcode_tui_theme::chrome::{StatusSegment, StatusSegments};
    let status_row = |segments: Option<&[StatusSegment]>| {
        let mut app = app_with(|chrome| {
            if let Some(segments) = segments {
                chrome.status.segments = StatusSegments::new(segments).expect("segments");
            }
        });
        app.set_status("saved".to_owned());
        let area = Rect::new(0, 0, 100, 20);
        let layout = prepare_frame(&mut app, area).expect("layout");
        let mut buffer = Buffer::empty(area);
        render(&mut app, &mut PaintCx::new(&mut Frame::new(&mut buffer)));
        drop(app);
        buffer
            .row_symbols(layout.status().y)
            .expect("row")
            .trim_end()
            .to_owned()
    };
    let default = status_row(None);
    assert!(
        default.starts_with("ready") && default.contains("saved"),
        "{default:?}"
    );
    let reordered = status_row(Some(&[StatusSegment::Message, StatusSegment::Activity]));
    assert!(
        reordered.starts_with("saved") && reordered.ends_with("ready"),
        "{reordered:?}"
    );
    let message_only = status_row(Some(&[StatusSegment::Message]));
    assert_eq!(message_only, "saved");
    assert_eq!(status_row(Some(&[])), "");
    assert!(StatusSegments::new(&[StatusSegment::Hints, StatusSegment::Hints]).is_none());
}
