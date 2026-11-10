//! Behavior of decorative chrome in the terminal renderer: status separators,
//! surface layering, and motion.

use std::time::{Duration, Instant, SystemTime};

use bcode_config::TuiDiffViewerConfig;
use bcode_presentation_policy::models::UnitFacts;
use bcode_tui_theme::chrome::{
    BorderStyle, FocusEffect, IconSet, Motion, SeparatorStyle, SurfaceStyle, TuiChrome,
};
use bmux_tui::style::Color;
use bmux_tui::text_width::display_width;

use super::*;
use crate::app::TemporalDamage;
use crate::invalidation::InvalidationKey;
use crate::structure::GroupView;
use crate::theme::definition::{ThemeCatalog, ThemeSelection};

const ACTIVITY_KEY: &str = "activity-animation";

/// The bundled `nord` theme with `edit` applied to its chrome.
fn themed(edit: impl FnOnce(&mut TuiChrome)) -> crate::theme::ResolvedTheme {
    let catalog = ThemeCatalog::bundled().expect("bundled themes parse");
    let mut definition = catalog
        .resolve(&ThemeSelection::new("nord"))
        .expect("nord resolves");
    // Tool containers without fills of their own, so layering is observable.
    for (role, style) in &mut definition.styles {
        if role.starts_with("tool.") && role.ends_with(".container") {
            style.bg = None;
        }
    }
    edit(&mut definition.chrome);
    crate::theme::resolved_definition_theme(
        Some(&definition),
        crate::theme::PENDING_AGENT_METADATA_ACCENT,
    )
}

fn presented(edit: impl FnOnce(&mut TuiChrome)) -> crate::theme::PresentedTheme {
    let theme = themed(edit);
    theme.presented(theme.accent)
}

fn app_with(edit: impl FnOnce(&mut TuiChrome), reduce_motion: bool) -> BmuxApp {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.apply_tui_config(bcode_config::TuiConfig {
        reduce_motion,
        ..bcode_config::TuiConfig::default()
    });
    app.preview_resolved_theme("chrome-test", &themed(edit));
    app.set_daemon_connection(crate::app::DaemonConnectionState::Connected);
    app
}

fn text(spans: &[Span]) -> String {
    spans.iter().map(|span| span.content.as_str()).collect()
}

fn trimmed(rows: &[Line]) -> Vec<String> {
    rows.iter()
        .map(|row| row.plain_text().trim_end().to_owned())
        .collect()
}

#[test]
fn status_line_joins_segments_with_the_theme_separator() {
    for (icons, separators, expected) in [
        (IconSet::Unicode, SeparatorStyle::Plain, " · "),
        (IconSet::Unicode, SeparatorStyle::Bar, " │ "),
        (IconSet::None, SeparatorStyle::Bar, " | "),
        (IconSet::Unicode, SeparatorStyle::Slant, " ╱ "),
        (IconSet::None, SeparatorStyle::Slant, " / "),
        (IconSet::None, SeparatorStyle::Powerline, " > "),
        (IconSet::Nerd, SeparatorStyle::Powerline, " \u{e0b1} "),
    ] {
        let app = app_with(
            |chrome| {
                chrome.icons = icons;
                chrome.separators = separators;
            },
            false,
        );
        let theme = TuiTheme::for_app(&app);
        let wide = text(&statusline_spans(
            &app,
            200,
            theme,
            TranscriptStatusPosition::default(),
        ));
        assert!(
            wide.contains(expected),
            "{icons:?}/{separators:?}: {wide:?}"
        );
        let plain = app_with(|chrome| chrome.icons = icons, false);
        let plain_theme = TuiTheme::for_app(&plain);
        for width in [0_usize, 1, 2, 5, 12, 40, 200] {
            let line = text(&statusline_spans(
                &app,
                width,
                theme,
                TranscriptStatusPosition::default(),
            ));
            let reference = text(&statusline_spans(
                &plain,
                width,
                plain_theme,
                TranscriptStatusPosition::default(),
            ));
            assert_eq!(
                display_width(&line),
                display_width(&reference),
                "{separators:?} at {width}: {line:?} vs {reference:?}"
            );
        }
        drop(plain);
        drop(app);
    }
}

#[test]
fn flat_surfaces_reproduce_themes_without_surface_chrome() {
    let default = presented(|_| {});
    let flat = presented(|chrome| chrome.surfaces = SurfaceStyle::Flat);
    assert_eq!(default.containers, flat.containers);
    let layered = presented(|chrome| chrome.surfaces = SurfaceStyle::Layered);
    assert!(
        layered
            .effects
            .raised_background(layered.surfaces.raised, layered.canvas)
            .is_some(),
        "nord paints a distinct raised surface"
    );
    assert_ne!(layered.containers, flat.containers);
    assert_eq!(layered.containers.user, flat.containers.user);
    assert_eq!(layered.containers.assistant, flat.containers.assistant);
}

#[test]
fn layered_surfaces_keep_tool_geometry_anchors_and_selection() {
    let previous = semantic_state_theme();
    let item = crate::transcript::tool_result_item(
        "call",
        Some("read"),
        None,
        "alpha\n漢字👍e\u{301}\nlonger line of tool output text",
        false,
    );
    for tool_border in [
        None,
        Some(BorderStyle::None),
        Some(BorderStyle::Left),
        Some(BorderStyle::Rounded),
    ] {
        let flat = presented(|chrome| chrome.borders.tool = tool_border);
        let layered = presented(|chrome| {
            chrome.borders.tool = tool_border;
            chrome.surfaces = SurfaceStyle::Layered;
        });
        let raised = layered
            .effects
            .raised_background(layered.surfaces.raised, layered.canvas)
            .expect("raised background");
        for width in [0_u16, 1, 2, 3, 4, 12, 40] {
            let flat_layout = TranscriptItemLayout::resolve(&flat, &item, width);
            let layered_layout = TranscriptItemLayout::resolve(&layered, &item, width);
            assert_eq!(
                (
                    flat_layout.outer_width,
                    flat_layout.content_width,
                    flat_layout.content_x,
                    flat_layout.bottom_rows,
                ),
                (
                    layered_layout.outer_width,
                    layered_layout.content_width,
                    layered_layout.content_x,
                    layered_layout.bottom_rows,
                ),
                "{tool_border:?} at {width}"
            );
            set_plugin_visual_theme(&flat);
            let (flat_rows, flat_anchors, flat_selection) = transcript_item_layout_from_item(
                &item,
                width,
                None,
                TuiDiffViewerConfig::default(),
                None,
            );
            set_plugin_visual_theme(&layered);
            let (rows, anchors, selection) = transcript_item_layout_from_item(
                &item,
                width,
                None,
                TuiDiffViewerConfig::default(),
                None,
            );
            let context = format!("{tool_border:?} at {width}");
            assert_eq!(rows.len(), flat_rows.len(), "{context}");
            assert_eq!(anchors, flat_anchors, "{context}");
            assert_eq!(selection, flat_selection, "{context}");
            assert_eq!(trimmed(&rows), trimmed(&flat_rows), "{context}");
            for row in &rows {
                assert!(
                    row.width() <= usize::from(width.max(1)),
                    "{context}: {row:?}"
                );
            }
            // Frames too narrow to draw degrade to no container, flat or not.
            assert!(
                layered_layout.container.is_none()
                    || rows
                        .iter()
                        .flat_map(|row| &row.spans)
                        .any(|span| span.style.bg == Some(raised)),
                "{context}: layered rows paint the raised surface"
            );
        }
    }
    set_plugin_visual_theme(&previous);
}

fn group_view() -> GroupView {
    GroupView {
        id: 7,
        facts: UnitFacts::default(),
        presentation: bcode_presentation_policy::host_defaults(),
        members: vec![7, 8],
        bridged: Vec::new(),
        attached: Vec::new(),
        tools: vec![("読み込み".to_owned(), 1), ("grep👍".to_owned(), 1)],
        duration_ms: Some(1_200),
    }
}

#[test]
fn layered_group_headers_fill_their_row_without_moving_content_or_anchors() {
    let previous = semantic_state_theme();
    let view = group_view();
    let presentation = bcode_presentation_policy::host_defaults();
    let flat = presented(|_| {});
    let layered = presented(|chrome| chrome.surfaces = SurfaceStyle::Layered);
    let raised = layered
        .effects
        .raised_background(layered.surfaces.raised, layered.canvas)
        .expect("raised background");
    for width in [0_u16, 1, 2, 5, 9, 30, 80] {
        let paint = |theme: &crate::theme::PresentedTheme| {
            set_plugin_visual_theme(theme);
            let (mut rows, mut anchors) = (Vec::new(), Vec::new());
            group_header::push_group_header(&mut rows, &mut anchors, &view, &presentation, width);
            (rows, anchors)
        };
        let (flat_rows, flat_anchors) = paint(&flat);
        let (rows, anchors) = paint(&layered);
        assert_eq!(anchors, flat_anchors, "width {width}");
        assert_eq!(trimmed(&rows), trimmed(&flat_rows), "width {width}");
        let header = rows.last().expect("header row");
        assert_eq!(
            header.width(),
            flat_rows
                .last()
                .expect("header")
                .width()
                .max(usize::from(width)),
            "width {width}"
        );
        assert!(header.width() <= usize::from(width).max(1), "width {width}");
        assert!(
            header
                .spans
                .iter()
                .filter(|span| !span.content.is_empty())
                .all(|span| span.style.bg == Some(raised)),
            "width {width}: {header:?}"
        );
    }
    set_plugin_visual_theme(&previous);
}

fn activity_deadline(app: &BmuxApp, now: Instant) -> Option<Duration> {
    app.invalidation_requests(now, SystemTime::now())
        .into_iter()
        .find(|request| request.key.as_str() == ACTIVITY_KEY)
        .map(|request| request.at.saturating_duration_since(now))
}

#[test]
fn idle_apps_schedule_no_activity_frames_at_any_motion_level() {
    for motion in [Motion::None, Motion::Subtle, Motion::Full] {
        for reduce_motion in [false, true] {
            let app = app_with(
                |chrome| {
                    chrome.motion = motion;
                    chrome.focus = FocusEffect::Sweep;
                    chrome.focus_colors = vec![Color::Rgb(255, 0, 170), Color::Rgb(0, 255, 255)];
                },
                reduce_motion,
            );
            assert_eq!(activity_deadline(&app, Instant::now()), None, "{motion:?}");
        }
    }
}

#[test]
fn motion_sets_the_activity_cadence_and_reduce_motion_forces_none() {
    for (motion, reduce_motion, interval, animates) in [
        (Motion::Full, false, Duration::from_millis(80), true),
        (Motion::Subtle, false, Duration::from_millis(100), true),
        (Motion::None, false, Duration::from_secs(1), false),
        (Motion::Full, true, Duration::from_secs(1), false),
    ] {
        let mut app = app_with(|chrome| chrome.motion = motion, reduce_motion);
        app.set_cancelling();
        let deadline = activity_deadline(&app, Instant::now()).expect("activity frame");
        assert!(
            deadline <= interval && deadline > interval / 2,
            "{motion:?}/{reduce_motion}: {deadline:?}"
        );
        let before = app.spinner_step();
        app.handle_invalidations_with_damage(&[InvalidationKey::new(ACTIVITY_KEY)], Instant::now());
        assert_eq!(app.spinner_step() != before, animates, "{motion:?}");
    }
}

#[test]
fn focus_sweeps_only_with_motion_and_repaint_only_while_active() {
    let sweep = |chrome: &mut TuiChrome| {
        chrome.motion = Motion::Full;
        chrome.focus = FocusEffect::Sweep;
        chrome.focus_colors = vec![Color::Rgb(255, 0, 170), Color::Rgb(0, 255, 255)];
    };
    let tick = |app: &mut BmuxApp| {
        let (_, damage) = app.handle_invalidations_with_damage(
            &[InvalidationKey::new(ACTIVITY_KEY)],
            Instant::now(),
        );
        damage
    };

    let mut app = app_with(sweep, false);
    app.set_cancelling();
    let previous = semantic_state_theme();
    set_plugin_visual_theme(&app.presented_theme());
    // With no gradient-bordered component on screen, only the status line
    // repaints even while the sweep advances.
    begin_focus_frame(0, true);
    let phase = app.focus_phase();
    assert_eq!(tick(&mut app), vec![TemporalDamage::Status]);
    assert_ne!(app.focus_phase(), phase);
    // A dialog or picker paints focused chrome: ticks repaint everything.
    let _chrome = component_chrome();
    let phase = app.focus_phase();
    let damage = tick(&mut app);
    assert!(damage.contains(&TemporalDamage::Full), "{damage:?}");
    assert_ne!(app.focus_phase(), phase);
    set_focus_phase(app.focus_phase());
    let gradient = component_chrome().focus_gradient.expect("focus gradient");
    assert_eq!(gradient.phase, app.focus_phase());
    set_focus_phase(0);
    assert_eq!(
        component_chrome().focus_gradient.expect("gradient").phase,
        0
    );
    set_plugin_visual_theme(&previous);
    app.set_idle();
    assert!(tick(&mut app).is_empty(), "idle ticks repaint nothing");
    drop(app);

    let mut reduced = app_with(sweep, true);
    reduced.set_cancelling();
    assert_eq!(tick(&mut reduced), vec![TemporalDamage::Status]);
    assert_eq!(reduced.focus_phase(), 0);
    drop(reduced);

    let mut gradient_only = app_with(
        |chrome| {
            sweep(chrome);
            chrome.focus = FocusEffect::Gradient;
        },
        false,
    );
    gradient_only.set_cancelling();
    assert_eq!(tick(&mut gradient_only), vec![TemporalDamage::Status]);
    assert_eq!(gradient_only.focus_phase(), 0);
    begin_focus_frame(0, true);
    drop(gradient_only);
}

#[test]
fn focus_frames_track_painted_gradients_and_sync_the_phase_for_every_painter() {
    let sweep = |chrome: &mut TuiChrome| {
        chrome.motion = Motion::Full;
        chrome.focus = FocusEffect::Sweep;
        chrome.focus_colors = vec![Color::Rgb(255, 0, 170), Color::Rgb(0, 255, 255)];
    };
    let app = app_with(sweep, false);
    let previous = semantic_state_theme();
    set_plugin_visual_theme(&app.presented_theme());

    // A full frame that paints no bordered component reports nothing.
    begin_focus_frame(7, true);
    assert!(!focus_gradient_painted());
    // A picker painting focused chrome (without the transcript renderer, as
    // canvas-owning foregrounds do) gets the frame's phase and is recorded.
    let chrome = component_chrome();
    assert_eq!(chrome.focus_gradient.expect("gradient").phase, 7);
    assert!(focus_gradient_painted());
    // Partial frames keep the overlay recorded: it stays on screen.
    begin_focus_frame(8, false);
    assert!(focus_gradient_painted());
    if let Some(gradient) = dialog_chrome().focus_gradient {
        assert_eq!(gradient.phase, 8);
    }
    // The next full frame without it forgets it.
    begin_focus_frame(9, true);
    assert!(!focus_gradient_painted());

    begin_focus_frame(0, true);
    set_plugin_visual_theme(&previous);
    drop(app);
}
