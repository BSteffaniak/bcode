//! Contrast pairings shared by theme resolution and `bcode theme check`.
//!
//! [`PAIRS`] is the single table of foreground/background role pairings that
//! carry text and state. `bcode theme check` measures it, and resolution uses
//! it to raise derived roles (and authored roles of themes that opt in with
//! `contrast = "enforce"`) to their thresholds against the background each
//! role is actually painted on.
//!
//! Adjustments use [`bmux_color::ensure_contrast`], which changes only OKLCH
//! lightness, by the minimum needed, so hue is preserved.

use std::collections::BTreeMap;

use bmux_color::Rgb;
use bmux_tui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};

use crate::check::{ContrastGroup, SECONDARY_CONTRAST, TEXT_CONTRAST};

/// Which background a contrast pair measures against.
#[derive(Debug, Clone, Copy)]
pub enum PairBackground {
    /// The foreground role's own background when set, else this surface role,
    /// else the canvas.
    Surface(&'static str),
    /// The role paints its own background; its foreground falls back to this
    /// text role when unset.
    OwnWithText(&'static str),
}

/// One measured role pairing.
#[derive(Debug, Clone, Copy)]
pub struct PairSpec {
    /// Pair family.
    pub group: ContrastGroup,
    /// Role whose foreground is measured.
    pub role: &'static str,
    /// Background the foreground is painted on.
    pub background: PairBackground,
    /// Minimum WCAG contrast ratio.
    pub minimum: f64,
}

const fn on(
    group: ContrastGroup,
    role: &'static str,
    surface: &'static str,
    minimum: f64,
) -> PairSpec {
    PairSpec {
        group,
        role,
        background: PairBackground::Surface(surface),
        minimum,
    }
}

const fn own(
    group: ContrastGroup,
    role: &'static str,
    text: &'static str,
    minimum: f64,
) -> PairSpec {
    PairSpec {
        group,
        role,
        background: PairBackground::OwnWithText(text),
        minimum,
    }
}

/// The contrast pairs `bcode theme check` measures and resolution enforces.
///
/// Text and states use the WCAG AA text threshold; secondary text, focus
/// borders, diff signs, tool titles, and inactive selection use the
/// secondary threshold. Decorative borders are not measured.
///
/// Order matters for enforcement: a pair whose text comes from another role
/// is listed after that role's own pairs, so it measures the final color.
pub const PAIRS: &[PairSpec] = {
    use ContrastGroup::{Accent, Diff, Selection, Status, Text};
    const T: f64 = TEXT_CONTRAST;
    const S: f64 = SECONDARY_CONTRAST;
    &[
        on(Text, "text.primary", "canvas", T),
        on(Text, "text.primary", "surface.raised", T),
        on(Text, "text.primary", "surface.overlay", T),
        on(Text, "text.muted", "canvas", S),
        on(Text, "markdown.text", "canvas", T),
        on(Text, "markdown.inline_code", "canvas", T),
        on(Text, "markdown.code_block_text", "canvas", T),
        on(Text, "syntax.text", "canvas", T),
        on(Text, "syntax.comment", "canvas", S),
        on(Text, "source.text", "canvas", T),
        on(Text, "tool.output", "canvas", T),
        on(Text, "transcript.meta", "canvas", S),
        on(Accent, "accent", "canvas", T),
        on(Accent, "link", "canvas", T),
        on(Accent, "title", "canvas", T),
        on(Accent, "markdown.heading", "canvas", T),
        on(Accent, "markdown.link", "canvas", T),
        on(Accent, "transcript.user.label", "canvas", T),
        on(Accent, "transcript.assistant.label", "canvas", T),
        on(Accent, "border.focused", "canvas", S),
        on(Accent, "control.focused", "canvas", S),
        on(Status, "state.info", "canvas", T),
        on(Status, "state.success", "canvas", T),
        on(Status, "state.warning", "canvas", T),
        on(Status, "state.error", "canvas", T),
        own(Status, "state.info.surface", "state.info", T),
        own(Status, "state.success.surface", "state.success", T),
        own(Status, "state.warning.surface", "state.warning", T),
        own(Status, "state.error.surface", "state.error", T),
        on(Status, "tool.running.title", "canvas", S),
        on(Status, "tool.succeeded.title", "canvas", S),
        on(Status, "tool.failed.title", "canvas", S),
        on(Diff, "diff.text", "canvas", T),
        on(Diff, "diff.added", "canvas", S),
        on(Diff, "diff.removed", "canvas", S),
        on(Diff, "diff.hunk", "canvas", S),
        own(Diff, "diff.added_row", "diff.text", T),
        own(Diff, "diff.removed_row", "diff.text", T),
        own(Diff, "diff.added_emphasis", "diff.text", T),
        own(Diff, "diff.removed_emphasis", "diff.text", T),
        own(Selection, "selection.active", "text.primary", T),
        own(Selection, "selection.inactive", "text.primary", S),
    ]
};

/// A color whose displayed RGB value is known: explicit RGB, or a fixed
/// 256-color cube/grayscale index. The terminal default and the 16 ANSI
/// colors are user-configurable and therefore unknown.
pub(crate) fn measurable(color: Color) -> Result<Rgb, &'static str> {
    match color {
        Color::Rgb(..) => color.rgb().ok_or("unknown color"),
        Color::Indexed(index) if index >= 16 => {
            color.approximate_rgb().ok_or("unknown palette color")
        }
        Color::Default => Err("terminal default color"),
        _ => Err("terminal palette color"),
    }
}

/// Colors painted where a role sets none: the canvas text and fill.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CanvasColors {
    pub(crate) text: Option<Color>,
    pub(crate) fill: Option<Color>,
}

impl CanvasColors {
    pub(crate) fn of(styles: &BTreeMap<String, Style>) -> Self {
        let canvas = styles.get("canvas").copied().unwrap_or_default();
        Self {
            text: canvas
                .fg
                .or_else(|| styles.get("text.primary").and_then(|style| style.fg)),
            fill: canvas.bg,
        }
    }
}

/// Where a pair's painted colors come from, before `reversed` is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PairColors {
    pub(crate) fg: Option<Color>,
    pub(crate) bg: Option<Color>,
    /// Why a color came from a fallback.
    pub(crate) notes: Vec<String>,
}

/// Foreground and background `pair` paints with in `styles`, honoring the
/// role's `reversed` modifier. `None` when the role is unset.
pub(crate) fn pair_colors(
    styles: &BTreeMap<String, Style>,
    pair: &PairSpec,
    canvas: CanvasColors,
) -> Option<(Style, PairColors)> {
    let style = *styles.get(pair.role)?;
    let color_of = |role: &str, bg: bool| {
        styles
            .get(role)
            .and_then(|style| if bg { style.bg } else { style.fg })
    };
    let mut notes = Vec::new();
    let (fg, bg) = match pair.background {
        PairBackground::Surface(surface) => {
            let surface_bg = color_of(surface, true);
            if style.bg.is_none() && surface_bg.is_none() && surface != "canvas" {
                notes.push(format!("{surface} falls back to canvas"));
            }
            (
                style.fg.or(canvas.text),
                style.bg.or(surface_bg).or(canvas.fill),
            )
        }
        PairBackground::OwnWithText(text) => {
            if style.fg.is_none() {
                notes.push(format!("text from {text}"));
            }
            if style.bg.is_none() {
                notes.push("background from canvas".to_owned());
            }
            (
                style.fg.or_else(|| color_of(text, false)).or(canvas.text),
                style.bg.or(canvas.fill),
            )
        }
    };
    let (fg, bg) = if style.modifiers.contains(Modifier::REVERSED) {
        (bg, fg)
    } else {
        (fg, bg)
    };
    Some((style, PairColors { fg, bg, notes }))
}

/// Slack for 8-bit rounding of colors produced by `ensure_contrast`.
pub(crate) const RATIO_TOLERANCE: f64 = 0.005;

/// Which color of a role resolution changed to meet a contrast pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdjustedChannel {
    /// The role's foreground.
    Foreground,
    /// The role's own background.
    Background,
}

/// One color raised to meet its contrast pair during resolution.
///
/// Role names deserialize only when they name a role of a [`PAIRS`] entry,
/// since every adjustment originates from that table.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ContrastAdjustment {
    /// Adjusted role.
    pub role: &'static str,
    /// Adjusted color.
    pub channel: AdjustedChannel,
    /// Background role of the pair that required the adjustment: the surface
    /// the role is painted on, or the role itself when it paints its own
    /// background.
    pub pair_background: &'static str,
    /// Color before adjustment.
    pub before: Rgb,
    /// Color after adjustment.
    pub after: Rgb,
    /// The pair's other, unchanged color the adjustment was measured against.
    pub against: Rgb,
    /// Minimum ratio of the pair.
    pub minimum: f64,
}

/// The [`PAIRS`] role name equal to `name`: a measured role or a surface it is measured on.
fn pair_role(name: &str) -> Option<&'static str> {
    PAIRS.iter().find_map(|pair| {
        if pair.role == name {
            return Some(pair.role);
        }
        match pair.background {
            PairBackground::Surface(surface) | PairBackground::OwnWithText(surface)
                if surface == name =>
            {
                Some(surface)
            }
            _ => None,
        }
    })
}

/// Wire form of [`ContrastAdjustment`] before its role names are matched to [`PAIRS`].
#[derive(Deserialize)]
struct OwnedContrastAdjustment {
    role: String,
    channel: AdjustedChannel,
    pair_background: String,
    before: Rgb,
    after: Rgb,
    against: Rgb,
    minimum: f64,
}

// Hand-written: a derive would borrow the `&'static str` fields from the input.
impl<'de> Deserialize<'de> for ContrastAdjustment {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        OwnedContrastAdjustment::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

impl TryFrom<OwnedContrastAdjustment> for ContrastAdjustment {
    type Error = String;

    fn try_from(owned: OwnedContrastAdjustment) -> Result<Self, Self::Error> {
        let known = |name: &str| {
            pair_role(name).ok_or_else(|| format!("`{name}` is not a contrast pair role"))
        };
        Ok(Self {
            role: known(&owned.role)?,
            channel: owned.channel,
            pair_background: known(&owned.pair_background)?,
            before: owned.before,
            after: owned.after,
            against: owned.against,
            minimum: owned.minimum,
        })
    }
}

impl ContrastAdjustment {
    /// Contrast ratio of the pair before the adjustment.
    #[must_use]
    pub fn ratio_before(&self) -> f64 {
        self.before.contrast_ratio(self.against)
    }

    /// Contrast ratio of the pair after the adjustment.
    #[must_use]
    pub fn ratio_after(&self) -> f64 {
        self.after.contrast_ratio(self.against)
    }
}

/// Raise `adjustable` roles in `styles` to their [`PAIRS`] thresholds.
///
/// Pairs are visited in table order. A failing pair adjusts the role's own
/// foreground when it sets one; a role that paints its own background but
/// takes its text from another role adjusts that background instead, so the
/// shared text color stays consistent. Pairs with an unmeasurable color, a
/// `reversed` role, or a role that is not adjustable are left unchanged.
pub(crate) fn enforce(
    styles: &mut BTreeMap<String, Style>,
    adjustable: impl Fn(&str) -> bool,
) -> Vec<ContrastAdjustment> {
    let mut adjustments = Vec::new();
    for pair in PAIRS.iter().filter(|pair| adjustable(pair.role)) {
        let canvas = CanvasColors::of(styles);
        let Some((style, colors)) = pair_colors(styles, pair, canvas) else {
            continue;
        };
        if style.modifiers.contains(Modifier::REVERSED) {
            continue;
        }
        let (Some(Ok(fg)), Some(Ok(bg))) = (colors.fg.map(measurable), colors.bg.map(measurable))
        else {
            continue;
        };
        if fg.contrast_ratio(bg) + RATIO_TOLERANCE >= pair.minimum {
            continue;
        }
        let pair_background = match pair.background {
            PairBackground::Surface(surface) => surface,
            PairBackground::OwnWithText(_) => pair.role,
        };
        let adjustment = if style.fg.is_some() {
            let after = bmux_color::ensure_contrast(fg, bg, pair.minimum);
            ContrastAdjustment {
                role: pair.role,
                channel: AdjustedChannel::Foreground,
                pair_background,
                before: fg,
                after,
                against: bg,
                minimum: pair.minimum,
            }
        } else if matches!(pair.background, PairBackground::OwnWithText(_)) && style.bg.is_some() {
            let after = bmux_color::ensure_contrast(bg, fg, pair.minimum);
            ContrastAdjustment {
                role: pair.role,
                channel: AdjustedChannel::Background,
                pair_background,
                before: bg,
                after,
                against: fg,
                minimum: pair.minimum,
            }
        } else {
            continue;
        };
        if let Some(style) = styles.get_mut(pair.role) {
            let color = Color::from(adjustment.after);
            match adjustment.channel {
                AdjustedChannel::Foreground => style.fg = Some(color),
                AdjustedChannel::Background => style.bg = Some(color),
            }
        }
        adjustments.push(adjustment);
    }
    adjustments
}

#[cfg(test)]
mod tests {
    use bmux_color::Oklch;

    use super::*;
    use crate::check::{CheckVariant, ContrastOutcome, check_resolved};
    use crate::definition::{
        ResolvedThemeDefinition, ThemeCatalog, ThemeKind, ThemeSelection, parse_theme_definition,
    };

    /// Largest hue change, in degrees, a contrast adjustment may introduce for
    /// a color with visible chroma. `ensure_contrast` keeps hue in OKLCH; the
    /// bound absorbs 8-bit rounding and gamut clipping.
    const MAX_HUE_DRIFT_DEGREES: f64 = 5.0;
    /// Chroma below which hue is not perceptible enough to bound.
    const MIN_CHROMA_FOR_HUE: f64 = 0.03;

    fn hue_drift(before: Rgb, after: Rgb) -> f64 {
        let delta = (Oklch::from_rgb(before).h - Oklch::from_rgb(after).h).abs();
        if delta > 180.0 { 360.0 - delta } else { delta }
    }

    fn resolve(source: &str, variant: CheckVariant) -> ResolvedThemeDefinition {
        let mut catalog = ThemeCatalog::bundled().expect("bundled");
        let definition = parse_theme_definition("test.toml", source).expect("parses");
        let id = definition.id().to_owned();
        catalog.insert(definition);
        catalog
            .resolve(&ThemeSelection::new(id).variant(variant.resolved()))
            .expect("resolves")
    }

    fn row_outcome(resolved: &ResolvedThemeDefinition, subject: &str) -> ContrastOutcome {
        check_resolved(resolved, &[])
            .contrast
            .iter()
            .find(|row| {
                let name = if row.foreground == row.background {
                    row.foreground.to_owned()
                } else {
                    format!("{}/{}", row.foreground, row.background)
                };
                name == subject
            })
            .unwrap_or_else(|| panic!("no pair {subject}"))
            .outcome
    }

    /// Seeds whose derived state surfaces, inline code, and selection fall
    /// below their thresholds before contrast enforcement.
    const LOW_CONTRAST_SEEDS: &str = r##"
schema_version = 2
id = "low-contrast"
extends = ["seeded-base"]

[palette]
accent = "#3760bf"
background = "#e1e2e7"
blue = "#2e7de9"
green = "#587539"
orange = "#b15c00"
red = "#f52a65"
text = "#3760bf"
yellow = "#8c6c3e"
"##;

    #[test]
    fn derived_roles_meet_the_pair_they_are_painted_on() {
        let resolved = resolve(LOW_CONTRAST_SEEDS, CheckVariant::Light);
        assert!(
            !resolved.contrast_adjustments.is_empty(),
            "fixture must exercise enforcement"
        );
        for pair in PAIRS {
            let subject = match pair.background {
                PairBackground::Surface(surface) => format!("{}/{surface}", pair.role),
                PairBackground::OwnWithText(_) => pair.role.to_owned(),
            };
            assert_ne!(
                row_outcome(&resolved, &subject),
                ContrastOutcome::Fail,
                "{subject}"
            );
        }
        // Text keeps its color where it already passes; the tinted surface
        // moves instead of the shared state text.
        let surface = resolved
            .contrast_adjustments
            .iter()
            .find(|adjustment| {
                adjustment.role.starts_with("state.") && adjustment.role.ends_with(".surface")
            })
            .expect("a state surface is adjusted");
        assert_eq!(surface.channel, AdjustedChannel::Background);
    }

    #[test]
    fn authored_roles_are_preserved_unless_the_theme_opts_in() {
        const AUTHORED: &str = r##"
schema_version = 2
id = "authored"
extends = ["seeded-base"]

[palette]
accent = "#7aa2f7"
background = "#2e3440"
text = "#eceff4"

[styles."state.error"]
fg = "#bf616a"
"##;
        let preserved = resolve(AUTHORED, CheckVariant::Dark);
        assert_eq!(
            preserved.style("state.error").and_then(|style| style.fg),
            Some(Color::Rgb(0xbf, 0x61, 0x6a))
        );
        assert_eq!(
            row_outcome(&preserved, "state.error/canvas"),
            ContrastOutcome::Fail
        );

        let enforced = resolve(
            &AUTHORED.replace("extends", "contrast = \"enforce\"\nextends"),
            CheckVariant::Dark,
        );
        assert_eq!(
            row_outcome(&enforced, "state.error/canvas"),
            ContrastOutcome::Pass
        );
        let adjustment = enforced
            .contrast_adjustments
            .iter()
            .find(|adjustment| adjustment.role == "state.error")
            .expect("authored role adjusted");
        assert_eq!(adjustment.before, Rgb::new(0xbf, 0x61, 0x6a));
        assert!(hue_drift(adjustment.before, adjustment.after) <= MAX_HUE_DRIFT_DEGREES);
    }

    #[test]
    fn terminal_owned_colors_are_never_adjusted() {
        let catalog = ThemeCatalog::bundled().expect("bundled");
        let resolved = catalog
            .resolve(&ThemeSelection::new("terminal-native"))
            .expect("resolves");
        assert_eq!(resolved.contrast_adjustments, Vec::new());
    }

    /// Every selectable bundled theme, in both variants: each adjustment
    /// changes its color and keeps hue within [`MAX_HUE_DRIFT_DEGREES`].
    #[test]
    fn bundled_adjustments_keep_hue_within_bound() {
        let catalog = ThemeCatalog::bundled().expect("bundled");
        let mut adjusted = 0;
        for definition in catalog
            .definitions()
            .filter(|definition| definition.kind() == ThemeKind::Theme)
        {
            for variant in CheckVariant::ALL {
                let resolved = catalog
                    .resolve(&ThemeSelection::new(definition.id()).variant(variant.resolved()))
                    .expect("resolves");
                for adjustment in &resolved.contrast_adjustments {
                    adjusted += 1;
                    let label = format!(
                        "{} {} {}: {} -> {}",
                        definition.id(),
                        variant.name(),
                        adjustment.role,
                        adjustment.before.to_hex(),
                        adjustment.after.to_hex()
                    );
                    if Oklch::from_rgb(adjustment.before).c >= MIN_CHROMA_FOR_HUE {
                        assert!(
                            hue_drift(adjustment.before, adjustment.after) <= MAX_HUE_DRIFT_DEGREES,
                            "{label}"
                        );
                    }
                    assert_ne!(adjustment.before, adjustment.after, "{label}");
                }
            }
        }
        assert!(adjusted > 0, "bundled themes exercise enforcement");
    }
}
