//! Behavior of decorative chrome in the terminal renderer: status separators,
//! surface layering, and motion.

use std::time::{Duration, Instant};

use bcode_tui_theme::chrome::{
    FocusEffect, IconSet, Motion, SeparatorStyle, SurfaceStyle, TuiChrome,
};
use bmux_tui::style::Color;
use bmux_tui::text_width::display_width;

use super::*;
use crate::app::TemporalDamage;
use crate::invalidation::InvalidationKey;
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
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
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

fn activity_deadline(app: &BmuxApp, now: Instant) -> Option<Duration> {
    app.invalidation_requests(now)
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
    set_chrome_theme(&app.presented_theme());
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
    set_chrome_theme(&previous);
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
    set_chrome_theme(&app.presented_theme());

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
    set_chrome_theme(&previous);
    drop(app);
}
