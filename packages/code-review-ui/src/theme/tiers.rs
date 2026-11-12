//! Semantic tiers: the few surfaces, text levels and accents every review screen is painted
//! from.
//!
//! * **Surfaces.** `canvas` is the screen and the panes' body; `surface_alt` sets rows apart
//!   (the header and footer bars, file headers, cards such as the comment composer);
//!   `overlay` carries modals, the palette, pickers and toasts.
//! * **Text.** `text`, `strong` (bold text), `muted` (secondary metadata) and `dim` (tertiary:
//!   separators, hints' punctuation, unused progress).
//! * **Accents.** `accent` marks focus and the current item; `accent_alt` is a second hue for
//!   categories that must differ from the accent; `on_accent` is text on an accent fill
//!   (buttons, the active pill).
//!
//! Every tier comes from the bcode theme's roles with fallbacks, so a theme that defines few
//! roles still yields a complete set:
//!
//! * `surface_alt` and `overlay` use `surface.raised` and `surface.overlay` when they differ
//!   from the canvas, else the text color mixed into the canvas (6% and 10%), else (terminal
//!   colors, no RGB canvas) no fill at all.
//! * `accent` is the focused-control color (`control.focused`), else the focused border, else
//!   the theme's accent role.
//! * `accent_alt` is the first of the given alternates (syntax keyword and type colors, info,
//!   link) that reads as a different hue: for an RGB accent, an RGB color at least 40° of
//!   OKLCH hue from the accent and 20° from the success, error and info colors; for terminal
//!   colors, a different color. Without one, an RGB accent is turned 70° in hue (readable on
//!   the canvas), and a terminal-color accent gets magenta (blue when it is magenta itself).
//! * `dim` is the muted color mixed toward the canvas while it keeps 2:1 contrast, else muted
//!   text with the dim modifier.
//! * `on_accent` is the canvas color on the accent when that reads (4.5:1), else black or
//!   white, whichever reads better; with terminal colors it is the accent reversed.

use bmux_color::{Oklch, Rgb, ensure_contrast, mix};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui_components::theme::ComponentTheme;

/// Share of the text color mixed into the canvas for `surface_alt` without a raised surface.
const SURFACE_ALT_TINT: f64 = 0.06;
/// Share of the text color mixed into the canvas for `overlay` without an overlay surface.
const OVERLAY_TINT: f64 = 0.10;
/// Share of the muted color kept for `dim`.
const DIM_SHARE: f64 = 0.65;
/// Least contrast `dim` keeps against the canvas.
const DIM_CONTRAST: f64 = 2.0;
/// Contrast `on_accent` text needs on the accent.
const ON_ACCENT_CONTRAST: f64 = 4.5;
/// Least OKLCH hue difference between the accent and the second accent.
const ALT_HUE_FROM_ACCENT: f64 = 40.0;
/// Least OKLCH hue difference between the second accent and the success, error and info
/// colors.
const ALT_HUE_FROM_STATES: f64 = 20.0;
/// Least OKLCH chroma for a color to have a hue at all.
const MIN_CHROMA: f64 = 0.03;
/// Hue turn of a derived second accent.
const ALT_HUE_TURN: f64 = 70.0;
/// Contrast text colors need on the canvas.
const TEXT_CONTRAST: f64 = 4.5;

/// The review's semantic tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tiers {
    /// The screen and the panes' body: canvas background with primary text.
    pub canvas: Style,
    /// Rows set apart from the canvas: header and footer bars, file headers, cards.
    pub surface_alt: Style,
    /// Modals, the palette, pickers and toasts.
    pub overlay: Style,
    /// Primary text.
    pub text: Style,
    /// Bold primary text.
    pub strong: Style,
    /// Secondary metadata.
    pub muted: Style,
    /// Tertiary text: separators, punctuation, unused cells.
    pub dim: Style,
    /// Focus and the current item.
    pub accent: Style,
    /// A second hue for categories that must differ from the accent.
    pub accent_alt: Style,
    /// Text on an accent fill.
    pub on_accent: Style,
}

impl Tiers {
    /// Tiers of `component`, with `focus` (the focused-control color, when the theme has one)
    /// and `alternates` (candidate second hues, most preferred first).
    #[must_use]
    pub fn derive(component: &ComponentTheme, focus: Option<Color>, alternates: &[Color]) -> Self {
        let canvas_bg = component.canvas.bg;
        let text_fg = component.text.fg.or(component.canvas.fg);
        let base = canvas_bg.and_then(rgb);
        let ink = text_fg.and_then(rgb);
        let tint = |share: f64| match (base, ink) {
            (Some(base), Some(ink)) => Some(Color::from(mix(ink, base, share))),
            _ => None,
        };
        let surface_alt_bg = component
            .surfaces
            .raised
            .bg
            .filter(|bg| Some(*bg) != canvas_bg)
            .or_else(|| tint(SURFACE_ALT_TINT));
        let overlay_bg = component
            .surfaces
            .overlay
            .bg
            .filter(|bg| Some(*bg) != canvas_bg && Some(*bg) != surface_alt_bg)
            .or_else(|| tint(OVERLAY_TINT))
            .or(surface_alt_bg);
        let fill = |bg: Option<Color>| {
            let style = text_fg.map_or_else(Style::new, |fg| Style::new().fg(fg));
            bg.map_or(style, |bg| style.bg(bg))
        };

        let text = text_fg.map_or_else(Style::new, |fg| Style::new().fg(fg));
        let muted = component.muted;
        let dim = match (muted.fg.and_then(rgb), base) {
            (Some(muted), Some(base)) => {
                let dimmed = mix(muted, base, DIM_SHARE);
                let color = if dimmed.contrast_ratio(base) >= DIM_CONTRAST {
                    dimmed
                } else {
                    ensure_contrast(dimmed, base, DIM_CONTRAST)
                };
                Style::new().fg(Color::from(color))
            }
            _ => muted.add_modifier(Modifier::DIM),
        };

        let accent_fg = focus
            .or(component.border_focused.fg)
            .or(component.accent.fg);
        let accent = accent_fg.map_or_else(
            || Style::new().add_modifier(Modifier::BOLD),
            |fg| Style::new().fg(fg),
        );
        let accent_alt = second_accent(
            accent_fg,
            [component.success.fg, component.error.fg, component.info.fg],
            alternates,
            base,
        )
        .map_or(accent, |fg| Style::new().fg(fg));
        let on_accent = accent_fg
            .map_or_else(
                || Style::new().add_modifier(Modifier::REVERSED),
                |fg| on_accent(fg, base),
            )
            .add_modifier(Modifier::BOLD);

        Self {
            canvas: fill(canvas_bg),
            surface_alt: fill(surface_alt_bg),
            overlay: fill(overlay_bg),
            text,
            strong: text.add_modifier(Modifier::BOLD),
            muted,
            dim,
            accent,
            accent_alt,
            on_accent,
        }
    }
}

/// Text on an `accent` fill: the `canvas` color when it reads on the accent, else black or
/// white; a terminal-color accent is reversed instead.
fn on_accent(accent: Color, canvas: Option<Rgb>) -> Style {
    rgb(accent).map_or_else(
        || Style::new().fg(accent).add_modifier(Modifier::REVERSED),
        |accent_rgb| {
            let ink = canvas
                .filter(|canvas| canvas.contrast_ratio(accent_rgb) >= ON_ACCENT_CONTRAST)
                .unwrap_or_else(|| readable_extreme(accent_rgb));
            Style::new().fg(Color::from(ink)).bg(accent)
        },
    )
}

/// Black or white, whichever reads better on `fill`.
pub fn readable_extreme(fill: Rgb) -> Rgb {
    if Rgb::BLACK.contrast_ratio(fill) >= Rgb::WHITE.contrast_ratio(fill) {
        Rgb::BLACK
    } else {
        Rgb::WHITE
    }
}

/// The second accent: the first alternate that reads as a different hue from `accent` and the
/// `states`, else (for an RGB accent) the accent turned in hue, else `None`.
fn second_accent(
    accent: Option<Color>,
    states: [Option<Color>; 3],
    alternates: &[Color],
    canvas: Option<Rgb>,
) -> Option<Color> {
    let Some(accent_rgb) = accent.and_then(rgb) else {
        // Terminal colors: another color than the accent and the states, else magenta (blue
        // when the accent is magenta), the terminal palette's usual second hue.
        let fallback = if matches!(accent, Some(Color::Magenta | Color::BrightMagenta)) {
            Color::Blue
        } else {
            Color::Magenta
        };
        return alternates
            .iter()
            .copied()
            .find(|color| Some(*color) != accent && !states.contains(&Some(*color)))
            .or_else(|| accent.map(|_| fallback));
    };
    let accent_hue = hue(accent_rgb)?;
    let state_hues: Vec<f64> = states
        .iter()
        .flatten()
        .filter_map(|c| rgb(*c))
        .filter_map(hue)
        .collect();
    let found = alternates.iter().copied().find(|color| {
        rgb(*color).and_then(hue).is_some_and(|candidate| {
            hue_distance(candidate, accent_hue) >= ALT_HUE_FROM_ACCENT
                && state_hues
                    .iter()
                    .all(|state| hue_distance(candidate, *state) >= ALT_HUE_FROM_STATES)
        })
    });
    found.or_else(|| {
        let mut turned = Oklch::from_rgb(accent_rgb);
        turned.h = (turned.h + ALT_HUE_TURN).rem_euclid(360.0);
        let turned = turned.to_rgb();
        let turned = canvas.map_or(turned, |canvas| {
            ensure_contrast(turned, canvas, TEXT_CONTRAST)
        });
        Some(Color::from(turned))
    })
}

/// OKLCH hue of `color`, when it has enough chroma to have one.
fn hue(color: Rgb) -> Option<f64> {
    let lch = Oklch::from_rgb(color);
    (lch.c >= MIN_CHROMA).then_some(lch.h)
}

/// Angular distance between two hues, in degrees.
fn hue_distance(a: f64, b: f64) -> f64 {
    let difference = (a - b).rem_euclid(360.0);
    difference.min(360.0 - difference)
}

/// The RGB value of `color`, when it is one.
const fn rgb(color: Color) -> Option<Rgb> {
    match color {
        Color::Rgb(r, g, b) => Some(Rgb::new(r, g, b)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui_components::theme::{ComponentSurfaces, ComponentThemeSeed};

    fn seed(background: Rgb, foreground: Rgb) -> ComponentThemeSeed {
        ComponentThemeSeed {
            background,
            foreground,
            accent: Rgb::new(122, 162, 247),
            info: Rgb::new(125, 207, 255),
            success: Rgb::new(158, 206, 106),
            warning: Rgb::new(224, 175, 104),
            error: Rgb::new(247, 118, 142),
        }
    }

    #[test]
    fn rgb_themes_get_distinct_layered_surfaces_and_readable_accent_text() {
        for (background, foreground) in [
            (Rgb::new(24, 26, 33), Rgb::new(214, 218, 228)),
            (Rgb::new(250, 250, 250), Rgb::new(40, 44, 52)),
        ] {
            let component = ComponentTheme::derive(seed(background, foreground));
            let tiers = Tiers::derive(&component, None, &[Color::Rgb(187, 154, 247)]);
            let backgrounds = [tiers.canvas.bg, tiers.surface_alt.bg, tiers.overlay.bg];
            assert!(backgrounds.iter().all(Option::is_some), "{tiers:?}");
            assert_ne!(backgrounds[0], backgrounds[1]);
            assert_ne!(backgrounds[1], backgrounds[2]);
            assert_ne!(tiers.muted, tiers.dim);
            assert_ne!(tiers.accent, tiers.accent_alt);
            let (Some(Color::Rgb(r, g, b)), Some(Color::Rgb(br, bg, bb))) =
                (tiers.on_accent.fg, tiers.on_accent.bg)
            else {
                panic!("{:?}", tiers.on_accent);
            };
            assert!(Rgb::new(r, g, b).contrast_ratio(Rgb::new(br, bg, bb)) >= 4.5);
        }
    }

    #[test]
    fn themes_with_few_roles_fall_back_without_inventing_colors() {
        // Terminal colors only: no fills, the accent reversed for text on it, dim by modifier.
        let terminal = ComponentTheme::terminal_default();
        let tiers = Tiers::derive(&terminal, None, &[]);
        assert!(
            tiers
                .surface_alt
                .bg
                .is_none_or(|bg| Some(bg) == terminal.canvas.bg),
            "{tiers:?}"
        );
        assert!(tiers.on_accent.modifiers.contains(Modifier::REVERSED));
        assert!(tiers.dim.modifiers.contains(Modifier::DIM) || tiers.dim.fg.is_some());

        // An RGB canvas without raised or overlay surfaces gets tinted ones.
        let mut flat = ComponentTheme::derive(seed(Rgb::new(0, 0, 0), Rgb::new(255, 255, 255)));
        flat.surfaces = ComponentSurfaces {
            normal: flat.canvas,
            raised: flat.canvas,
            overlay: flat.canvas,
            scrim: None,
        };
        let tiers = Tiers::derive(&flat, None, &[]);
        assert!(tiers.surface_alt.bg.is_some());
        assert_ne!(tiers.surface_alt.bg, flat.canvas.bg);
        assert_ne!(tiers.overlay.bg, tiers.surface_alt.bg);
        // Without a distinct alternate the second hue is the accent turned in hue.
        assert_ne!(tiers.accent_alt, tiers.accent);
        let (Some(alt), Some(accent)) = (
            tiers.accent_alt.fg.and_then(rgb),
            tiers.accent.fg.and_then(rgb),
        ) else {
            panic!("{tiers:?}");
        };
        assert!(hue_distance(hue(alt).unwrap(), hue(accent).unwrap()) >= ALT_HUE_FROM_ACCENT);
    }

    #[test]
    fn the_focused_control_color_is_the_accent() {
        let component = ComponentTheme::derive(seed(Rgb::new(0, 0, 0), Rgb::new(255, 255, 255)));
        let focus = Color::Rgb(1, 2, 3);
        assert_eq!(
            Tiers::derive(&component, Some(focus), &[]).accent.fg,
            Some(focus)
        );
    }
}
