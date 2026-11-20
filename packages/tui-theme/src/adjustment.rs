//! User contrast adjustments, applied on top of a resolved theme's exact colors.
//!
//! [`apply_contrast`] changes a [`ResolvedThemeDefinition`] after resolution, so theme files and
//! the resolution itself stay untouched; [`ContrastSettings::EXACT`] (the default) changes
//! nothing at all.
//!
//! Every adjusted color is measured against the background it is painted on, in OKLCH:
//!
//! * Factor: lightness moves away from (factor above 1) or toward (below 1) the background's
//!   lightness, `L' = L_bg + (L − L_bg) × factor`, clamped to `[0, 1]`. Hue is kept. Chroma
//!   follows the factor by a quarter, `C' = C × (1 + (factor − 1) / 4)`, so colors pushed to a
//!   lightness extreme keep some colorfulness and colors pulled toward the background fade a
//!   little with it; colors outside the sRGB gamut lose chroma (never lightness or hue).
//! * Floor: a color still below the minimum contrast ratio then changes lightness by the
//!   minimum needed to reach it ([`bmux_color::ensure_contrast`]); colors at or above it stay.
//!
//! Parts select what is adjusted:
//!
//! * fills: role backgrounds other than the canvas (surfaces, selection, line highlight, diff
//!   rows, state surfaces) move against the canvas background. The factor applies; the floor
//!   does not, since a fill carries no text of its own. Fills change first, so foregrounds are
//!   measured against the changed fills.
//! * text: every role foreground outside syntax, against the role's own background, else the
//!   surface its [`crate::contrast::PAIRS`] entry paints it on, else the canvas. A fill whose
//!   text comes from another role and fails the floor gets its own foreground.
//! * syntax: `syntax.*` against the `source.text` background and `markdown.code.*` against the
//!   `markdown.code_block_text` background (each falling back to the canvas).
//! * ansi: the sixteen `ansi_*` palette slots and `terminal_fg` against `terminal_bg` (else
//!   the canvas). ANSI black and white get the factor but not the floor, since programs use
//!   them as fills as often as text.
//!
//! Colors the terminal owns (the default color and the sixteen terminal palette indices) are
//! never adjusted, and nothing is adjusted against them, so terminal-native themes stay as
//! they are. Roles painted `reversed` are left as they are.

use std::collections::BTreeMap;

pub use bcode_tui_theme_models::{
    ContrastFactor, ContrastOverride, ContrastParts, ContrastPreferences, ContrastSettings,
    MinimumContrast,
};
use bmux_color::{Oklch, Rgb};
use bmux_tui::style::{Color, Modifier, Style};

use crate::contrast::{PAIRS, PairBackground, measurable};
use crate::definition::ResolvedThemeDefinition;
use crate::roles::ANSI_SLOTS;

/// Share of the factor's change from 1 that chroma follows.
const CHROMA_FOLLOW: f64 = 0.25;

/// ANSI slots that get the factor but not the floor.
const ANSI_FILL_SLOTS: [&str; 2] = ["ansi_black", "ansi_white"];

/// Which part a role foreground belongs to.
fn foreground_part(role: &str) -> Part {
    if role.starts_with("syntax.") || role.starts_with("markdown.code.") {
        Part::Syntax
    } else {
        Part::Text
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Text,
    Syntax,
}

/// `color` with its lightness distance from `background` scaled by `factor` (see the module
/// documentation). A factor of exactly 1 returns `color` unchanged.
#[must_use]
pub fn scale_from(color: Rgb, background: Rgb, factor: ContrastFactor) -> Rgb {
    if factor.is_exact() {
        return color;
    }
    let factor = factor.get();
    let lch = Oklch::from_rgb(color);
    let background_l = Oklch::from_rgb(background).l;
    let l = (lch.l - background_l)
        .mul_add(factor, background_l)
        .clamp(0.0, 1.0);
    let c = lch.c * (factor - 1.0).mul_add(CHROMA_FOLLOW, 1.0).max(0.0);
    Oklch { l, c, h: lch.h }.to_rgb()
}

/// `color` against `background` with `settings`' factor, then lifted to its floor when it falls
/// below one (`floor` false skips the floor).
fn adjusted(color: Rgb, background: Rgb, settings: &ContrastSettings, floor: bool) -> Rgb {
    let scaled = scale_from(color, background, settings.factor);
    match settings.minimum.ratio() {
        Some(minimum) if floor && scaled.contrast_ratio(background) < minimum => {
            bmux_color::ensure_contrast(scaled, background, minimum)
        }
        _ => scaled,
    }
}

fn rgb_of(color: Option<Color>) -> Option<Rgb> {
    color.and_then(|color| measurable(color).ok())
}

const fn painted_reversed(style: &Style) -> bool {
    style.modifiers.contains(Modifier::REVERSED)
}

/// The background `role`'s foreground is painted on when the role sets none of its own.
fn surface_of(role: &str) -> Option<&'static str> {
    match foreground_part(role) {
        Part::Syntax if role.starts_with("markdown.code.") => Some("markdown.code_block_text"),
        Part::Syntax => Some("source.text"),
        Part::Text => PAIRS
            .iter()
            .filter(|pair| pair.role == role)
            .find_map(|pair| match pair.background {
                PairBackground::Surface(surface) => Some(surface),
                PairBackground::OwnWithText(_) => None,
            }),
    }
}

/// Apply `settings` to `theme`'s colors (see the module documentation). Exact settings leave
/// `theme` untouched.
pub fn apply_contrast(theme: &mut ResolvedThemeDefinition, settings: &ContrastSettings) {
    if settings.is_exact() {
        return;
    }
    let parts = settings.parts;
    let canvas_fill = theme.styles.get("canvas").and_then(|style| style.bg);
    let canvas = rgb_of(canvas_fill);
    if parts.fills
        && let Some(canvas) = canvas
    {
        adjust_fills(&mut theme.styles, canvas, settings);
    }
    if parts.text || parts.syntax {
        adjust_foregrounds(&mut theme.styles, canvas, settings);
    }
    if parts.ansi {
        adjust_ansi(&mut theme.palette, canvas, settings);
    }
    theme.fingerprint = crate::definition::adjusted_fingerprint(theme);
}

fn adjust_fills(styles: &mut BTreeMap<String, Style>, canvas: Rgb, settings: &ContrastSettings) {
    for (role, style) in styles.iter_mut() {
        if role == "canvas" || painted_reversed(style) {
            continue;
        }
        let Some(fill) = rgb_of(style.bg) else {
            continue;
        };
        if fill != canvas {
            style.bg = Some(Color::from(adjusted(fill, canvas, settings, false)));
        }
    }
}

fn adjust_foregrounds(
    styles: &mut BTreeMap<String, Style>,
    canvas: Option<Rgb>,
    settings: &ContrastSettings,
) {
    let background_of = |styles: &BTreeMap<String, Style>, role: &str, own: Option<Color>| {
        rgb_of(own).or_else(|| {
            surface_of(role)
                .and_then(|surface| rgb_of(styles.get(surface).and_then(|style| style.bg)))
                .or(canvas)
        })
    };
    // Surfaces are measured first, so roles painted on them see their final background.
    let roles = styles.keys().cloned().collect::<Vec<_>>();
    let mut changes = Vec::new();
    for role in &roles {
        let style = styles[role];
        let enabled = match foreground_part(role) {
            Part::Text => settings.parts.text,
            Part::Syntax => settings.parts.syntax,
        };
        if !enabled || painted_reversed(&style) {
            continue;
        }
        let (Some(foreground), Some(background)) =
            (rgb_of(style.fg), background_of(styles, role, style.bg))
        else {
            continue;
        };
        changes.push((
            role.clone(),
            adjusted(foreground, background, settings, true),
        ));
    }
    for (role, foreground) in changes {
        if let Some(style) = styles.get_mut(&role) {
            style.fg = Some(Color::from(foreground));
        }
    }
    if settings.parts.text
        && let Some(minimum) = settings.minimum.ratio()
    {
        lift_inherited_text(styles, minimum);
    }
}

/// Give a fill that takes its text from another role its own foreground when that text fails
/// the floor on it.
fn lift_inherited_text(styles: &mut BTreeMap<String, Style>, minimum: f64) {
    for pair in PAIRS {
        let PairBackground::OwnWithText(text) = pair.background else {
            continue;
        };
        let Some(style) = styles.get(pair.role).copied() else {
            continue;
        };
        if style.fg.is_some() || painted_reversed(&style) {
            continue;
        }
        let (Some(fill), Some(inherited)) = (
            rgb_of(style.bg),
            rgb_of(styles.get(text).and_then(|text| text.fg)),
        ) else {
            continue;
        };
        if inherited.contrast_ratio(fill) < minimum
            && let Some(style) = styles.get_mut(pair.role)
        {
            style.fg = Some(Color::from(bmux_color::ensure_contrast(
                inherited, fill, minimum,
            )));
        }
    }
}

fn adjust_ansi(
    palette: &mut BTreeMap<String, Color>,
    canvas: Option<Rgb>,
    settings: &ContrastSettings,
) {
    let Some(background) = rgb_of(palette.get("terminal_bg").copied()).or(canvas) else {
        return;
    };
    for slot in ANSI_SLOTS.iter().copied().chain(["terminal_fg"]) {
        let Some(color) = palette.get_mut(slot) else {
            continue;
        };
        let Some(rgb) = rgb_of(Some(*color)) else {
            continue;
        };
        *color = Color::from(adjusted(
            rgb,
            background,
            settings,
            !ANSI_FILL_SLOTS.contains(&slot),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{ResolvedThemeVariant, ThemeCatalog, ThemeSelection};

    /// Largest hue change, in degrees, the factor may introduce for a color with visible
    /// chroma; the bound absorbs 8-bit rounding.
    const MAX_HUE_DRIFT_DEGREES: f64 = 6.0;
    /// Chroma below which hue is not perceptible enough to bound.
    const MIN_CHROMA_FOR_HUE: f64 = 0.04;
    /// Slack for 8-bit rounding of contrast ratios.
    const RATIO_SLACK: f64 = 0.02;

    fn catalog() -> ThemeCatalog {
        ThemeCatalog::bundled().expect("bundled")
    }

    fn resolve(id: &str, variant: ResolvedThemeVariant, settings: ContrastSettings) -> Resolved {
        Resolved(
            catalog()
                .resolve(&ThemeSelection::new(id).variant(variant).contrast(settings))
                .expect("resolves"),
        )
    }

    struct Resolved(ResolvedThemeDefinition);

    impl Resolved {
        fn fg(&self, role: &str) -> Rgb {
            rgb_of(self.0.style(role).and_then(|style| style.fg)).expect(role)
        }

        fn bg(&self, role: &str) -> Rgb {
            rgb_of(self.0.style(role).and_then(|style| style.bg)).expect(role)
        }

        fn slot(&self, slot: &str) -> Rgb {
            rgb_of(self.0.color(slot)).expect(slot)
        }
    }

    fn factor(value: f64) -> ContrastSettings {
        ContrastSettings {
            factor: ContrastFactor::new(value).expect("factor"),
            ..ContrastSettings::EXACT
        }
    }

    fn only(parts: ContrastParts, settings: ContrastSettings) -> ContrastSettings {
        ContrastSettings { parts, ..settings }
    }

    const NO_PARTS: ContrastParts = ContrastParts {
        text: false,
        syntax: false,
        fills: false,
        ansi: false,
    };

    fn hue_drift(before: Rgb, after: Rgb) -> f64 {
        let delta = (Oklch::from_rgb(before).h - Oklch::from_rgb(after).h).abs();
        if delta > 180.0 { 360.0 - delta } else { delta }
    }

    /// Text and syntax roles measured against their painted background, as the adjustment
    /// pairs them.
    const MEASURED: &[(&str, &str)] = &[
        ("text.primary", "canvas"),
        ("text.muted", "canvas"),
        ("state.error", "canvas"),
        ("state.success", "canvas"),
        ("link", "canvas"),
        ("markdown.heading", "canvas"),
        ("syntax.keyword", "canvas"),
        ("syntax.string", "canvas"),
        ("syntax.comment", "canvas"),
    ];

    #[test]
    fn exact_settings_resolve_every_bundled_theme_identically() {
        let catalog = catalog();
        let ids = catalog
            .definitions()
            .filter(|definition| definition.kind() == crate::definition::ThemeKind::Theme)
            .map(|definition| definition.id().to_owned())
            .collect::<Vec<_>>();
        assert!(ids.len() > 20, "{ids:?}");
        let exact_but_configured = [
            ContrastSettings::EXACT,
            // A factor of 1 with no floor is exact whatever the parts.
            only(NO_PARTS, factor(1.0)),
            // No part adjusted is exact whatever the factor.
            only(NO_PARTS, factor(1.6)),
        ];
        for id in &ids {
            for variant in [
                ResolvedThemeVariant::Unspecified,
                ResolvedThemeVariant::Dark,
                ResolvedThemeVariant::Light,
            ] {
                let authored = catalog
                    .resolve(&ThemeSelection::new(id.clone()).variant(variant))
                    .expect("resolves");
                for settings in exact_but_configured {
                    let adjusted = catalog
                        .resolve(
                            &ThemeSelection::new(id.clone())
                                .variant(variant)
                                .contrast(settings),
                        )
                        .expect("resolves");
                    assert_eq!(adjusted, authored, "{id} {variant:?} {settings:?}");
                }
            }
        }
    }

    #[test]
    fn a_factor_above_one_raises_and_below_one_lowers_each_pairs_contrast_keeping_hue() {
        for (id, variant) in [
            ("solarized", ResolvedThemeVariant::Light),
            ("solarized", ResolvedThemeVariant::Dark),
            ("catppuccin-mocha", ResolvedThemeVariant::Dark),
            ("tokyonight", ResolvedThemeVariant::Dark),
        ] {
            let exact = resolve(id, variant, ContrastSettings::EXACT);
            let higher = resolve(id, variant, factor(1.4));
            let lower = resolve(id, variant, factor(0.7));
            for (role, surface) in MEASURED {
                let before = exact.fg(role);
                let background = exact.bg(surface);
                let ratio = before.contrast_ratio(background);
                let raised = higher.fg(role).contrast_ratio(higher.bg(surface));
                let lowered = lower.fg(role).contrast_ratio(lower.bg(surface));
                assert!(
                    raised > ratio - RATIO_SLACK,
                    "{id} {role}: {ratio} -> {raised}"
                );
                assert!(
                    lowered < ratio + RATIO_SLACK,
                    "{id} {role}: {ratio} -> {lowered}"
                );
                for after in [higher.fg(role), lower.fg(role)] {
                    if Oklch::from_rgb(before).c >= MIN_CHROMA_FOR_HUE
                        && Oklch::from_rgb(after).c >= MIN_CHROMA_FOR_HUE
                    {
                        let drift = hue_drift(before, after);
                        assert!(drift <= MAX_HUE_DRIFT_DEGREES, "{id} {role}: {drift}");
                    }
                }
            }
            // The canvas itself is the reference and never moves.
            assert_eq!(higher.bg("canvas"), exact.bg("canvas"));
            // Low-contrast text clearly gains.
            let muted =
                |theme: &Resolved| theme.fg("text.muted").contrast_ratio(theme.bg("canvas"));
            assert!(muted(&higher) > muted(&exact) + 0.2, "{id}");
            assert!(muted(&lower) < muted(&exact) - 0.2, "{id}");
        }
    }

    #[test]
    fn the_floor_lifts_only_failing_pairs_and_leaves_passing_pairs_exact() {
        let floor = ContrastSettings {
            minimum: MinimumContrast::new(4.5).expect("floor"),
            ..ContrastSettings::EXACT
        };
        let exact = resolve(
            "solarized",
            ResolvedThemeVariant::Light,
            ContrastSettings::EXACT,
        );
        let lifted = resolve("solarized", ResolvedThemeVariant::Light, floor);
        let mut lifted_pairs = 0;
        for (role, surface) in MEASURED {
            let background = exact.bg(surface);
            let before = exact.fg(role).contrast_ratio(background);
            let after = lifted.fg(role).contrast_ratio(lifted.bg(surface));
            if before >= 4.5 {
                assert_eq!(lifted.fg(role), exact.fg(role), "{role} passes and stays");
            } else {
                assert!(after + 0.01 >= 4.5, "{role}: {before} -> {after}");
                lifted_pairs += 1;
            }
        }
        assert!(lifted_pairs > 0, "solarized light has failing pairs");
        // The floor alone moves no fill.
        assert_eq!(lifted.bg("selection.active"), exact.bg("selection.active"));
    }

    #[test]
    fn parts_isolate_what_changes() {
        let id = "tokyonight";
        let variant = ResolvedThemeVariant::Dark;
        let exact = resolve(id, variant, ContrastSettings::EXACT);
        let part = |parts| resolve(id, variant, only(parts, factor(1.5)));
        let text = part(ContrastParts {
            text: true,
            ..NO_PARTS
        });
        assert_ne!(text.fg("text.muted"), exact.fg("text.muted"));
        assert_eq!(text.fg("syntax.keyword"), exact.fg("syntax.keyword"));
        assert_eq!(text.bg("surface.raised"), exact.bg("surface.raised"));
        assert_eq!(text.slot("ansi_red"), exact.slot("ansi_red"));

        let syntax = part(ContrastParts {
            syntax: true,
            ..NO_PARTS
        });
        assert_ne!(syntax.fg("syntax.comment"), exact.fg("syntax.comment"));
        assert_eq!(syntax.fg("text.muted"), exact.fg("text.muted"));
        assert_eq!(syntax.slot("ansi_red"), exact.slot("ansi_red"));

        let fills = part(ContrastParts {
            fills: true,
            ..NO_PARTS
        });
        let separation = |theme: &Resolved, role| {
            (Oklch::from_rgb(theme.bg(role)).l - Oklch::from_rgb(theme.bg("canvas")).l).abs()
        };
        assert!(separation(&fills, "selection.active") > separation(&exact, "selection.active"));
        assert_eq!(fills.fg("text.muted"), exact.fg("text.muted"));
        assert_eq!(fills.slot("ansi_red"), exact.slot("ansi_red"));

        let ansi = part(ContrastParts {
            ansi: true,
            ..NO_PARTS
        });
        assert_ne!(
            ansi.slot("ansi_bright_black"),
            exact.slot("ansi_bright_black")
        );
        assert_eq!(ansi.fg("text.muted"), exact.fg("text.muted"));
        assert_eq!(ansi.bg("selection.active"), exact.bg("selection.active"));
        // Only the ansi part reaches the terminal palette.
        let everything_but_ansi = part(ContrastParts {
            ansi: false,
            ..ContrastParts::ALL
        });
        for slot in ANSI_SLOTS {
            assert_eq!(everything_but_ansi.slot(slot), exact.slot(slot), "{slot}");
        }
    }

    #[test]
    fn terminal_owned_colors_are_never_adjusted() {
        let settings = ContrastSettings {
            factor: ContrastFactor::new(1.8).expect("factor"),
            minimum: MinimumContrast::new(7.0).expect("floor"),
            parts: ContrastParts::ALL,
        };
        let exact = catalog()
            .resolve(&ThemeSelection::new("terminal-native"))
            .expect("resolves");
        let adjusted = catalog()
            .resolve(&ThemeSelection::new("terminal-native").contrast(settings))
            .expect("resolves");
        assert_eq!(adjusted.styles, exact.styles);
        assert_eq!(adjusted.palette, exact.palette);
    }

    #[test]
    fn adjusted_resolutions_have_their_own_fingerprint() {
        let exact = resolve("nord", ResolvedThemeVariant::Dark, ContrastSettings::EXACT);
        let higher = resolve("nord", ResolvedThemeVariant::Dark, factor(1.2));
        let higher_again = resolve("nord", ResolvedThemeVariant::Dark, factor(1.2));
        assert_ne!(higher.0.fingerprint, exact.0.fingerprint);
        assert_eq!(higher.0.fingerprint, higher_again.0.fingerprint);
    }
}
