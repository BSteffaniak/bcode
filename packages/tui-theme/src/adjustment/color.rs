//! Vibrance, richness and background tint, applied to a resolved theme before contrast.
//!
//! Everything is computed in OKLCH against the sRGB gamut. For a color with lightness `L`,
//! chroma `C` and hue `H`, `Cmax(L, H)` is the largest chroma sRGB can show at that lightness
//! and hue, and `r = C / Cmax(L, H)` is the color's relative chroma. Hue is always kept, and a
//! result outside the gamut loses chroma (never lightness or hue).
//!
//! The steps run in this order, each on the previous step's result:
//!
//! 1. Background tint: the canvas and every neutral surface (role backgrounds other than the
//!    tinted fills below, and the theme's terminal background) scale their chroma by the tint,
//!    `C' = C × tint`, at the same lightness and hue. `0` is neutral grey.
//! 2. Richness: each colorful color moves its lightness toward the lightness where its hue
//!    reaches its largest chroma (the hue's cusp), by `(L_cusp − L) × richness` but never more
//!    than [`RICHNESS_MAX_LIGHTNESS_SHIFT`] × richness, and keeps its relative chroma at the new
//!    lightness, so a pastel color at the gamut edge gains chroma.
//! 3. Vibrance: each colorful color's relative chroma becomes `r + (1 − r) × v` for a positive
//!    vibrance `v` and `r × (1 + v)` for a negative one, at the same lightness and hue.
//!
//! Colorful colors are decided by role, never by how colorful a color happens to be:
//!
//! * syntax: `syntax.*` and `markdown.code.*` foregrounds except plain code, variables,
//!   punctuation and comments;
//! * accents: accent, title, link, focused and hovered controls, states, activity, colored
//!   transcript labels, tool status titles, Markdown headings, links, inline code, list markers,
//!   alerts and checked tasks, the truncation notice, and diff titles, added, removed and hunk
//!   foregrounds;
//! * fills: the backgrounds of the active selection, diff rows and intraline emphasis, state
//!   surfaces, Markdown inline code and highlights, and review file headers, changed with
//!   [`FILL_STRENGTH`] of the vibrance and richness;
//! * ansi: the theme's ANSI colors except black, white and bright black.
//!
//! Text, muted text, comments, punctuation, borders, the canvas and neutral surfaces never
//! change with vibrance or richness. A colorful role painted with the theme's text, muted,
//! comment or border color is treated as that neutral color and stays too, and a color without
//! a perceptible hue (chroma below [`ACHROMATIC_CHROMA`]) has no hue to strengthen and stays.

use std::collections::BTreeMap;

use bmux_color::{Oklab, Oklch, Rgb};
use bmux_tui::style::{Color, Style};

use super::{ContrastSettings, painted_reversed, rgb_of};
use crate::definition::ResolvedThemeDefinition;
use crate::roles::ANSI_SLOTS;

/// The largest lightness change richness `1.0` makes; smaller richness scales it.
pub const RICHNESS_MAX_LIGHTNESS_SHIFT: f64 = 0.15;
/// Share of vibrance and richness tinted fills receive.
pub const FILL_STRENGTH: f64 = 0.5;
/// Chroma below which a color has no perceptible hue.
pub const ACHROMATIC_CHROMA: f64 = 0.004;

/// Bisection steps for the largest in-gamut chroma.
const CHROMA_STEPS: u32 = 32;
/// Golden-section steps for the cusp lightness.
const CUSP_STEPS: u32 = 40;
/// Chroma no sRGB color reaches.
const CHROMA_CEILING: f64 = 0.5;

/// Syntax foregrounds that are neutral by role.
const NEUTRAL_SYNTAX: [&str; 4] = ["text", "comment", "punctuation", "variable"];

/// Accent foregrounds vibrance and richness change.
const ACCENT_ROLES: &[&str] = &[
    "accent",
    "title",
    "link",
    "control.focused",
    "control.hover",
    "state.info",
    "state.success",
    "state.warning",
    "state.error",
    "activity.spinner",
    "activity.text",
    "activity.shimmer",
    "transcript.user.label",
    "transcript.assistant.label",
    "transcript.skill.label",
    "transcript.skill_error.label",
    "transcript.pending.label",
    "transcript.stream_status.label",
    "tool.running.title",
    "tool.waiting.title",
    "tool.succeeded.title",
    "tool.failed.title",
    "tool.timed_out.title",
    "markdown.heading",
    "markdown.link",
    "markdown.inline_code",
    "markdown.list_marker",
    "markdown.task_checked",
    "markdown.alert_note",
    "markdown.alert_tip",
    "markdown.alert_important",
    "markdown.alert_warning",
    "markdown.alert_caution",
    "source.truncated",
    "diff.title",
    "diff.added",
    "diff.removed",
    "diff.hunk",
];

/// Roles whose background is a tinted fill rather than a neutral surface.
const FILL_ROLES: &[&str] = &[
    "selection.active",
    "diff.added_row",
    "diff.removed_row",
    "diff.added_emphasis",
    "diff.removed_emphasis",
    "state.info.surface",
    "state.success.surface",
    "state.warning.surface",
    "state.error.surface",
    "markdown.inline_code",
    "markdown.highlight",
    "source.file.header",
];

/// Neutral roles whose foreground marks a color as neutral wherever another role uses it.
const NEUTRAL_ANCHORS: &[&str] = &[
    "canvas",
    "text.primary",
    "text.muted",
    "syntax.text",
    "syntax.comment",
    "border.default",
];

/// ANSI slots that are neutral.
const NEUTRAL_ANSI: [&str; 3] = ["ansi_black", "ansi_white", "ansi_bright_black"];

/// A colorful foreground's part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForegroundPart {
    Syntax,
    Accents,
}

fn foreground_part(role: &str) -> Option<ForegroundPart> {
    let syntax = role
        .strip_prefix("syntax.")
        .or_else(|| role.strip_prefix("markdown.code."));
    if let Some(token) = syntax {
        return (!NEUTRAL_SYNTAX.contains(&token)).then_some(ForegroundPart::Syntax);
    }
    ACCENT_ROLES
        .contains(&role)
        .then_some(ForegroundPart::Accents)
}

/// The largest chroma sRGB shows at lightness `l` and hue `h` (degrees).
#[must_use]
pub fn max_chroma(l: f64, h: f64) -> f64 {
    if l <= 0.0 || l >= 1.0 {
        return 0.0;
    }
    let (mut low, mut high) = (0.0, CHROMA_CEILING);
    for _ in 0..CHROMA_STEPS {
        let mid = f64::midpoint(low, high);
        if Oklab::from(Oklch::new(l, mid, h)).in_srgb_gamut() {
            low = mid;
        } else {
            high = mid;
        }
    }
    low
}

/// The lightness where hue `h` (degrees) reaches its largest sRGB chroma.
#[must_use]
pub fn cusp_lightness(h: f64) -> f64 {
    // The largest chroma rises to the cusp and falls after it, so a golden-section search
    // finds it.
    let ratio = (5.0_f64.sqrt() - 1.0) / 2.0;
    let (mut low, mut high) = (0.0_f64, 1.0_f64);
    for _ in 0..CUSP_STEPS {
        let left = (high - low).mul_add(-ratio, high);
        let right = (high - low).mul_add(ratio, low);
        if max_chroma(left, h) < max_chroma(right, h) {
            low = left;
        } else {
            high = right;
        }
    }
    f64::midpoint(low, high)
}

/// `color` with its chroma scaled by `tint`, at the same lightness and hue.
#[must_use]
pub fn tinted(color: Rgb, tint: f64) -> Rgb {
    let lch = Oklch::from_rgb(color);
    let c = (lch.c * tint).min(max_chroma(lch.l, lch.h));
    Oklch::new(lch.l, c, lch.h).to_rgb()
}

/// `color` moved toward its hue's cusp by `richness` (see the module documentation).
#[must_use]
pub fn enriched(color: Rgb, richness: f64) -> Rgb {
    let lch = Oklch::from_rgb(color);
    if richness <= 0.0 || lch.c < ACHROMATIC_CHROMA {
        return color;
    }
    let limit = max_chroma(lch.l, lch.h);
    let relative = if limit > 0.0 {
        (lch.c / limit).min(1.0)
    } else {
        0.0
    };
    let bound = RICHNESS_MAX_LIGHTNESS_SHIFT * richness;
    let shift = ((cusp_lightness(lch.h) - lch.l) * richness).clamp(-bound, bound);
    let l = (lch.l + shift).clamp(0.0, 1.0);
    Oklch::new(l, relative * max_chroma(l, lch.h), lch.h).to_rgb()
}

/// `color` with its relative chroma changed by `vibrance` (see the module documentation).
#[must_use]
pub fn vivid(color: Rgb, vibrance: f64) -> Rgb {
    let lch = Oklch::from_rgb(color);
    if vibrance == 0.0 || lch.c < ACHROMATIC_CHROMA {
        return color;
    }
    let limit = max_chroma(lch.l, lch.h);
    if limit <= 0.0 {
        return color;
    }
    let relative = (lch.c / limit).min(1.0);
    let changed = if vibrance > 0.0 {
        (1.0 - relative).mul_add(vibrance, relative)
    } else {
        relative * (1.0 + vibrance)
    };
    Oklch::new(lch.l, changed.clamp(0.0, 1.0) * limit, lch.h).to_rgb()
}

/// Richness then vibrance at `strength` of the settings.
fn colorful(color: Rgb, settings: &ContrastSettings, strength: f64) -> Rgb {
    let enriched = enriched(color, settings.richness.get() * strength);
    vivid(enriched, settings.vibrance.get() * strength)
}

/// Apply background tint, richness and vibrance to `theme` (see the module documentation).
/// Settings that leave colors exact change nothing.
pub fn apply_color(theme: &mut ResolvedThemeDefinition, settings: &ContrastSettings) {
    if settings.color_is_exact() {
        return;
    }
    let tint = settings.background_tint;
    if !tint.is_exact() {
        tint_backgrounds(theme, tint.get());
    }
    if settings.vibrance.is_exact() && settings.richness.is_exact() {
        return;
    }
    let parts = settings.color_parts;
    let neutral = neutral_colors(&theme.styles);
    for (role, style) in &mut theme.styles {
        if painted_reversed(style) {
            continue;
        }
        let enabled = match foreground_part(role) {
            Some(ForegroundPart::Syntax) => parts.syntax,
            Some(ForegroundPart::Accents) => parts.accents,
            None => false,
        };
        if enabled {
            change(&mut style.fg, &neutral, |rgb| colorful(rgb, settings, 1.0));
        }
        if parts.fills && FILL_ROLES.contains(&role.as_str()) {
            change(&mut style.bg, &[], |rgb| {
                colorful(rgb, settings, FILL_STRENGTH)
            });
        }
    }
    if parts.ansi {
        for slot in ANSI_SLOTS
            .iter()
            .filter(|slot| !NEUTRAL_ANSI.contains(slot))
        {
            if let Some(color) = theme.palette.get_mut(*slot) {
                let mut value = Some(*color);
                change(&mut value, &[], |rgb| colorful(rgb, settings, 1.0));
                if let Some(value) = value {
                    *color = value;
                }
            }
        }
    }
}

/// Replace `color` with `map` of it unless it is unmeasurable (terminal-owned) or one of
/// `neutral`.
fn change(color: &mut Option<Color>, neutral: &[Rgb], map: impl Fn(Rgb) -> Rgb) {
    if let Some(rgb) = rgb_of(*color)
        && !neutral.contains(&rgb)
    {
        *color = Some(Color::from(map(rgb)));
    }
}

/// The foregrounds of the neutral anchor roles.
fn neutral_colors(styles: &BTreeMap<String, Style>) -> Vec<Rgb> {
    NEUTRAL_ANCHORS
        .iter()
        .filter_map(|role| rgb_of(styles.get(*role).and_then(|style| style.fg)))
        .collect()
}

fn tint_backgrounds(theme: &mut ResolvedThemeDefinition, tint: f64) {
    for (role, style) in &mut theme.styles {
        if FILL_ROLES.contains(&role.as_str()) || painted_reversed(style) {
            continue;
        }
        change(&mut style.bg, &[], |rgb| tinted(rgb, tint));
    }
    if let Some(color) = theme.palette.get_mut("terminal_bg") {
        let mut value = Some(*color);
        change(&mut value, &[], |rgb| tinted(rgb, tint));
        if let Some(value) = value {
            *color = value;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adjustment::{
        BackgroundTint, ColorParts, ContrastFactor, MinimumContrast, Richness, Vibrance,
    };
    use crate::definition::{ResolvedThemeVariant, ThemeCatalog, ThemeSelection};

    /// Largest hue change, in degrees, for a color with visible chroma; absorbs 8-bit
    /// rounding.
    const MAX_HUE_DRIFT_DEGREES: f64 = 4.0;
    /// Lightness tolerance for 8-bit rounding.
    const LIGHTNESS_SLACK: f64 = 0.006;
    /// Relative chroma tolerance for 8-bit rounding.
    const RELATIVE_SLACK: f64 = 0.03;

    fn catalog() -> ThemeCatalog {
        ThemeCatalog::bundled().expect("bundled")
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

    fn resolve(id: &str, variant: ResolvedThemeVariant, settings: ContrastSettings) -> Resolved {
        Resolved(
            catalog()
                .resolve(&ThemeSelection::new(id).variant(variant).contrast(settings))
                .expect("resolves"),
        )
    }

    fn vibrance(value: f64) -> ContrastSettings {
        ContrastSettings {
            vibrance: Vibrance::new(value).expect("vibrance"),
            ..ContrastSettings::EXACT
        }
    }

    fn richness(value: f64) -> ContrastSettings {
        ContrastSettings {
            richness: Richness::new(value).expect("richness"),
            ..ContrastSettings::EXACT
        }
    }

    fn tint(value: f64) -> ContrastSettings {
        ContrastSettings {
            background_tint: BackgroundTint::new(value).expect("tint"),
            ..ContrastSettings::EXACT
        }
    }

    fn relative(color: Rgb) -> f64 {
        let lch = Oklch::from_rgb(color);
        lch.c / max_chroma(lch.l, lch.h)
    }

    fn hue_drift(before: Rgb, after: Rgb) -> f64 {
        let delta = (Oklch::from_rgb(before).h - Oklch::from_rgb(after).h).abs();
        if delta > 180.0 { 360.0 - delta } else { delta }
    }

    const COLORFUL: &[&str] = &[
        "accent",
        "state.error",
        "state.success",
        "state.warning",
        "link",
        "markdown.heading",
        "syntax.keyword",
        "syntax.string",
        "syntax.function",
        "diff.added",
    ];
    const NEUTRAL_FG: &[&str] = &[
        "text.primary",
        "text.muted",
        "syntax.comment",
        "syntax.punctuation",
        "syntax.text",
        "border.default",
        "markdown.text",
        "canvas",
    ];
    const NEUTRAL_BG: &[&str] = &["canvas", "surface.raised", "selection.inactive"];

    const THEMES: &[(&str, ResolvedThemeVariant)] = &[
        ("everforest", ResolvedThemeVariant::Dark),
        ("everforest", ResolvedThemeVariant::Light),
        ("catppuccin-frappe", ResolvedThemeVariant::Dark),
        ("solarized", ResolvedThemeVariant::Light),
        ("zenburn", ResolvedThemeVariant::Dark),
        ("tokyonight-storm", ResolvedThemeVariant::Dark),
    ];

    #[test]
    fn vibrance_raises_relative_chroma_keeping_lightness_and_hue() {
        for &(id, variant) in THEMES {
            let exact = resolve(id, variant, ContrastSettings::EXACT);
            let vivid = resolve(id, variant, vibrance(0.5));
            let dull = resolve(id, variant, vibrance(-0.5));
            for role in COLORFUL {
                let (before, after, lowered) = (exact.fg(role), vivid.fg(role), dull.fg(role));
                if Oklch::from_rgb(before).c < ACHROMATIC_CHROMA
                    || neutral_colors(&exact.0.styles).contains(&before)
                {
                    assert_eq!(after, before, "{id} {role}");
                    continue;
                }
                let (r, r_after) = (relative(before), relative(after));
                let expected = (1.0 - r).mul_add(0.5, r);
                assert!(
                    (r_after - expected).abs() < RELATIVE_SLACK,
                    "{id} {role}: {r} -> {r_after}, expected {expected}"
                );
                assert!(relative(lowered) < r, "{id} {role}");
                for changed in [after, lowered] {
                    let (l0, l1) = (Oklch::from_rgb(before).l, Oklch::from_rgb(changed).l);
                    assert!((l0 - l1).abs() < LIGHTNESS_SLACK, "{id} {role}: {l0} {l1}");
                    assert!(
                        hue_drift(before, changed) < MAX_HUE_DRIFT_DEGREES,
                        "{id} {role}"
                    );
                }
            }
        }
    }

    #[test]
    fn vibrance_and_richness_leave_neutral_roles_and_surfaces_exact() {
        for &(id, variant) in THEMES {
            let exact = resolve(id, variant, ContrastSettings::EXACT);
            let changed = resolve(
                id,
                variant,
                ContrastSettings {
                    vibrance: Vibrance::new(1.0).expect("vibrance"),
                    richness: Richness::new(1.0).expect("richness"),
                    ..ContrastSettings::EXACT
                },
            );
            for role in NEUTRAL_FG {
                assert_eq!(changed.fg(role), exact.fg(role), "{id} {role}");
            }
            for role in NEUTRAL_BG {
                assert_eq!(changed.bg(role), exact.bg(role), "{id} {role}");
            }
            for slot in NEUTRAL_ANSI {
                if exact.0.color(slot).is_some() {
                    assert_eq!(changed.slot(slot), exact.slot(slot), "{id} {slot}");
                }
            }
        }
    }

    #[test]
    fn richness_moves_lightness_toward_the_cusp_within_its_bound() {
        // Tokyo Night Moon's accents sit at the gamut edge, so only richness adds chroma.
        let id = "tokyonight-moon";
        let variant = ResolvedThemeVariant::Dark;
        let exact = resolve(id, variant, ContrastSettings::EXACT);
        let rich = resolve(id, variant, richness(0.6));
        let mut gained = 0;
        for role in COLORFUL {
            let (before, after) = (exact.fg(role), rich.fg(role));
            let (b, a) = (Oklch::from_rgb(before), Oklch::from_rgb(after));
            let cusp = cusp_lightness(b.h);
            let shift = a.l - b.l;
            assert!(
                shift.abs() <= RICHNESS_MAX_LIGHTNESS_SHIFT.mul_add(0.6, LIGHTNESS_SLACK),
                "{role}: {shift}"
            );
            assert!(
                (cusp - a.l).abs() <= (cusp - b.l).abs() + LIGHTNESS_SLACK,
                "{role} moves toward the cusp {cusp}: {} -> {}",
                b.l,
                a.l
            );
            assert!(hue_drift(before, after) < MAX_HUE_DRIFT_DEGREES, "{role}");
            assert!(
                (relative(after) - relative(before)).abs() < RELATIVE_SLACK,
                "{role} keeps its relative chroma"
            );
            if a.c > b.c + 0.01 {
                gained += 1;
            }
        }
        assert!(gained > 0, "pastel accents gain chroma");
    }

    #[test]
    fn background_tint_zero_greys_the_canvas_at_the_same_lightness() {
        for &(id, variant) in THEMES {
            let exact = resolve(id, variant, ContrastSettings::EXACT);
            let grey = resolve(id, variant, tint(0.0));
            for role in NEUTRAL_BG {
                let (before, after) = (exact.bg(role), grey.bg(role));
                assert!(
                    Oklch::from_rgb(after).c < 0.002,
                    "{id} {role}: {after:?} is neutral"
                );
                let (l0, l1) = (Oklch::from_rgb(before).l, Oklch::from_rgb(after).l);
                assert!((l0 - l1).abs() < LIGHTNESS_SLACK, "{id} {role}");
            }
            // Foregrounds stay.
            for role in COLORFUL.iter().chain(NEUTRAL_FG) {
                assert_eq!(grey.fg(role), exact.fg(role), "{id} {role}");
            }
        }
        // Solarized's dark canvas already sits at the gamut edge for its lightness; Tokyo
        // Night Storm's has room to tint more.
        let storm = resolve("tokyonight-storm", ResolvedThemeVariant::Dark, tint(2.0));
        let exact = resolve(
            "tokyonight-storm",
            ResolvedThemeVariant::Dark,
            ContrastSettings::EXACT,
        );
        assert!(
            Oklch::from_rgb(storm.bg("canvas")).c > Oklch::from_rgb(exact.bg("canvas")).c + 0.02,
            "a tint above 1 tints more"
        );
        assert!(hue_drift(storm.bg("canvas"), exact.bg("canvas")) < MAX_HUE_DRIFT_DEGREES);
    }

    #[test]
    fn color_parts_and_fills_select_what_changes() {
        let id = "everforest";
        let variant = ResolvedThemeVariant::Dark;
        let exact = resolve(id, variant, ContrastSettings::EXACT);
        let only = |parts: ColorParts| {
            resolve(
                id,
                variant,
                ContrastSettings {
                    color_parts: parts,
                    ..vibrance(0.6)
                },
            )
        };
        let none = ColorParts {
            syntax: false,
            accents: false,
            fills: false,
            ansi: false,
        };
        let syntax = only(ColorParts {
            syntax: true,
            ..none
        });
        assert_ne!(syntax.fg("syntax.keyword"), exact.fg("syntax.keyword"));
        assert_eq!(syntax.fg("state.error"), exact.fg("state.error"));
        assert_eq!(syntax.slot("ansi_red"), exact.slot("ansi_red"));
        let accents = only(ColorParts {
            accents: true,
            ..none
        });
        assert_ne!(accents.fg("state.error"), exact.fg("state.error"));
        assert_eq!(accents.fg("syntax.keyword"), exact.fg("syntax.keyword"));
        let ansi = only(ColorParts { ansi: true, ..none });
        assert_ne!(ansi.slot("ansi_red"), exact.slot("ansi_red"));
        assert_eq!(ansi.fg("state.error"), exact.fg("state.error"));

        // Fills change at reduced strength: less than a foreground of the same relative chroma
        // would.
        let fills = only(ColorParts {
            fills: true,
            ..none
        });
        let before = exact.bg("diff.added_row");
        let after = fills.bg("diff.added_row");
        let full = vivid(before, 0.6);
        assert_ne!(after, before);
        assert!(relative(after) < relative(full), "{after:?} {full:?}");
        assert_eq!(fills.fg("state.error"), exact.fg("state.error"));
        assert_eq!(fills.bg("canvas"), exact.bg("canvas"));
    }

    #[test]
    fn the_contrast_floor_runs_after_the_color_steps() {
        let settings = ContrastSettings {
            richness: Richness::new(1.0).expect("richness"),
            vibrance: Vibrance::new(1.0).expect("vibrance"),
            background_tint: BackgroundTint::new(2.0).expect("tint"),
            minimum: MinimumContrast::new(4.5).expect("floor"),
            factor: ContrastFactor::EXACT,
            ..ContrastSettings::EXACT
        };
        for &(id, variant) in THEMES {
            let adjusted = resolve(id, variant, settings);
            let canvas = adjusted.bg("canvas");
            for role in COLORFUL {
                let ratio = adjusted.fg(role).contrast_ratio(canvas);
                assert!(ratio + 0.02 >= 4.5, "{id} {role}: {ratio}");
            }
        }
    }

    #[test]
    fn helpers_stay_in_gamut_and_keep_hue() {
        for hex in [
            "#88c0d0", "#e9c268", "#bd93f9", "#ff5555", "#002b36", "#d3c6aa",
        ] {
            let color = Rgb::from_hex(hex).expect("hex");
            for value in [vivid(color, 1.0), enriched(color, 1.0), tinted(color, 2.0)] {
                let lch = Oklch::from_rgb(value);
                assert!(
                    lch.c <= max_chroma(lch.l, lch.h) + 0.01,
                    "{hex} -> {value:?}"
                );
                if Oklch::from_rgb(color).c > 0.03 {
                    assert!(hue_drift(color, value) < MAX_HUE_DRIFT_DEGREES, "{hex}");
                }
            }
            assert_eq!(vivid(color, 0.0), color);
            assert_eq!(enriched(color, 0.0), color);
        }
        // Full vibrance reaches the gamut edge.
        let nord = Rgb::from_hex("#88c0d0").expect("hex");
        assert!(relative(vivid(nord, 1.0)) > 0.95);
    }
}
