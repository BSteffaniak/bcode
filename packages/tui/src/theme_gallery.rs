//! Generated theme gallery: bundled themes and profiles rendered through the
//! real transcript sample path into a headless BMUX buffer.
//!
//! `cargo run -p xtask -- theme-gallery` writes the result to
//! `docs/theme-gallery.md`; `--check` fails when the committed page is stale.
//!
//! # Selection
//!
//! The page stays bounded and deterministic:
//!
//! * every selectable bundled theme (palettes, combos and root themes) is
//!   rendered alone in its dark variant;
//! * every bundled profile is rendered over [`PROFILE_BASES`] (the configured
//!   default theme and the `bcode` house theme);
//! * renders with identical text are printed once and list every combination
//!   that produces them, because colors do not survive an ANSI-free render and
//!   most palettes share their structure;
//! * at most [`MAX_RENDERS`] distinct renders are printed;
//! * every selectable theme also gets one swatch row per variant with the
//!   resolved colors of its core roles.
//!
//! Each render is exactly [`GALLERY_WIDTH`] × [`GALLERY_HEIGHT`] cells, with
//! trailing blank cells trimmed per row.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use bcode_tui_theme::definition::{
    ResolvedThemeDefinition, ResolvedThemeVariant, ThemeCatalog, ThemeError, ThemeKind,
    ThemeSelection,
};
use bmux_tui::buffer::Buffer;
use bmux_tui::geometry::Rect;
use bmux_tui::style::{Color, Style};

use super::structure::StructureState;
use super::theme::{PresentedTheme, resolved_accent, resolved_definition_theme};

/// Render width in terminal cells.
pub const GALLERY_WIDTH: u16 = 80;
/// Render height in terminal rows; taller samples are clipped.
pub const GALLERY_HEIGHT: u16 = 96;
/// Upper bound on distinct renders printed on the page.
pub const MAX_RENDERS: usize = 64;
/// Bases every profile is rendered over, after the configured default theme.
pub const PROFILE_BASES: [&str; 1] = ["bcode"];

/// One theme plus ordered overlays, rendered in one variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GalleryCombo {
    /// Base theme id.
    pub base: String,
    /// Ordered overlay ids (profiles).
    pub overlays: Vec<String>,
}

impl GalleryCombo {
    /// Human-readable `base + overlay` label.
    #[must_use]
    pub fn label(&self) -> String {
        std::iter::once(self.base.as_str())
            .chain(self.overlays.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" + ")
    }
}

/// Render the complete gallery page from the bundled catalog.
///
/// # Errors
///
/// Returns an error when the bundled catalog fails to parse or a selected
/// combination fails to resolve.
pub fn gallery_markdown() -> Result<String, ThemeError> {
    let catalog = ThemeCatalog::bundled()?;
    let themes = catalog
        .definitions()
        .filter(|definition| definition.kind() == ThemeKind::Theme)
        .map(|definition| definition.id().to_owned())
        .collect::<Vec<_>>();
    let profiles = catalog
        .definitions()
        .filter(|definition| definition.kind() == ThemeKind::Profile)
        .map(|definition| definition.id().to_owned())
        .collect::<Vec<_>>();
    let combos = gallery_combos(&themes, &profiles);

    let mut renders = Vec::<(String, Vec<String>)>::new();
    let mut by_text = BTreeMap::<String, usize>::new();
    let mut omitted = Vec::new();
    for combo in &combos {
        let resolved = resolve(&catalog, combo, ResolvedThemeVariant::Dark)?;
        let text = gallery_text(render_sample(&resolved, GALLERY_WIDTH, GALLERY_HEIGHT));
        if let Some(index) = by_text.get(&text) {
            renders[*index].1.push(combo.label());
        } else if renders.len() < MAX_RENDERS {
            by_text.insert(text.clone(), renders.len());
            renders.push((text, vec![combo.label()]));
        } else {
            omitted.push(combo.label());
        }
    }

    let mut page = String::new();
    write_header(&mut page, combos.len(), renders.len());
    if !omitted.is_empty() {
        let _ = writeln!(
            page,
            "Omitted after the render limit: {}.\n",
            omitted.join(", ")
        );
    }
    page.push_str("## Renders\n\n");
    for (index, (text, labels)) in renders.iter().enumerate() {
        let _ = writeln!(page, "### {}. {}\n", index + 1, labels[0]);
        if labels.len() > 1 {
            let shared = labels[1..]
                .iter()
                .map(|label| format!("`{label}`"))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(page, "Also rendered identically by: {shared}.\n");
        }
        let _ = writeln!(page, "```text\n{text}\n```\n");
    }
    for (variant, name) in [
        (ResolvedThemeVariant::Dark, "Dark"),
        (ResolvedThemeVariant::Light, "Light"),
    ] {
        let _ = writeln!(page, "## {name} variant swatches\n");
        page.push_str(
            "| Theme | Canvas | Text | Muted | Accent | Border | Raised | Selection | Info | Success | Warning | Error |\n",
        );
        page.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|\n");
        for theme in &themes {
            let combo = GalleryCombo {
                base: theme.clone(),
                overlays: Vec::new(),
            };
            let resolved = resolve(&catalog, &combo, variant)?;
            let _ = writeln!(page, "{}", swatch_row(theme, &resolved));
        }
        page.push('\n');
    }
    // Exactly one trailing newline keeps the committed file stable.
    while page.ends_with("\n\n") {
        page.pop();
    }
    Ok(page)
}

/// Combinations rendered on the page, in deterministic order: each
/// selectable theme alone (the default theme first), then each profile over the default theme and
/// [`PROFILE_BASES`].
#[must_use]
pub fn gallery_combos(themes: &[String], profiles: &[String]) -> Vec<GalleryCombo> {
    let default_theme = bcode_config::TuiConfig::default().theme.name;
    let mut bases = vec![default_theme];
    for base in PROFILE_BASES {
        if !bases.iter().any(|existing| existing == base) {
            bases.push(base.to_owned());
        }
    }
    bases.retain(|base| themes.contains(base));
    // The default theme leads, so the render most palettes share is labeled
    // with the theme users start from.
    let lead = bases.first();
    let alone = lead
        .into_iter()
        .chain(themes.iter().filter(|theme| Some(*theme) != lead))
        .map(|theme| GalleryCombo {
            base: theme.clone(),
            overlays: Vec::new(),
        });
    let overlaid = bases.iter().flat_map(|base| {
        profiles.iter().map(|profile| GalleryCombo {
            base: base.clone(),
            overlays: vec![profile.clone()],
        })
    });
    alone.chain(overlaid).collect()
}

/// Printed form of a fixed-size render: trailing blank rows are trimmed, and
/// a sample that fills the last row is marked as clipped.
fn gallery_text(mut rows: Vec<String>) -> String {
    let clipped = rows.last().is_some_and(|row| !row.is_empty());
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    if clipped {
        rows.push(format!("… (clipped at {GALLERY_HEIGHT} rows)"));
    }
    rows.join("\n")
}

fn resolve(
    catalog: &ThemeCatalog,
    combo: &GalleryCombo,
    variant: ResolvedThemeVariant,
) -> Result<ResolvedThemeDefinition, ThemeError> {
    catalog.resolve(
        &ThemeSelection::new(combo.base.clone())
            .overlays(combo.overlays.clone())
            .variant(variant),
    )
}

/// Render the appearance picker's sample transcript under `resolved` into a
/// `width` × `height` headless buffer and return its rows as plain text with
/// trailing blank cells trimmed.
///
/// The theme and structural policy are installed the way the TUI installs
/// them (theme chrome plus the theme's rule layers); the previously presented
/// transcript theme is restored afterwards.
#[must_use]
pub fn render_sample(resolved: &ResolvedThemeDefinition, width: u16, height: u16) -> Vec<String> {
    let theme = presented(resolved);
    let previous = super::render::semantic_state_theme();
    super::render::set_plugin_visual_theme(&theme);
    let mut structure = StructureState::default();
    structure.set_layers(&super::theme::structure_layers_for(Some(resolved)));
    let sample = super::appearance_picker_sample::sample_transcript();
    let rows = super::render::sample_transcript_rows(&sample, width, structure.snapshot());
    super::render::set_plugin_visual_theme(&previous);

    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    for (y, row) in (0..height).zip(rows.iter()) {
        buffer.write_line_with_fallback_style(Rect::new(0, y, width, 1), row, theme.canvas);
    }
    (0..height)
        .filter_map(|y| buffer.row_symbols(y))
        .map(|row| row.trim_end().to_owned())
        .collect()
}

fn presented(resolved: &ResolvedThemeDefinition) -> PresentedTheme {
    let accent = resolved_accent(Some(resolved));
    resolved_definition_theme(Some(resolved), accent).presented(accent)
}

fn swatch_row(id: &str, resolved: &ResolvedThemeDefinition) -> String {
    let theme = presented(resolved);
    let cells = [
        theme.canvas.bg,
        theme.text.fg,
        theme.muted.fg,
        Some(theme.accent),
        theme.border.fg,
        theme.surfaces.raised.bg,
        selection_color(theme.selection),
        theme.info.fg,
        theme.success.fg,
        theme.warning.fg,
        theme.error.fg,
    ];
    let mut row = format!("| `{id}` |");
    for color in cells {
        let _ = write!(row, " {} |", color_name(color));
    }
    row
}

/// The selection's visible color: its background, else its foreground.
const fn selection_color(style: Style) -> Option<Color> {
    match style.bg {
        Some(color) => Some(color),
        None => style.fg,
    }
}

/// Stable text for a resolved color; terminal-controlled colors are named,
/// never guessed.
#[must_use]
pub fn color_name(color: Option<Color>) -> String {
    let Some(color) = color else {
        return "—".to_owned();
    };
    let name = match color {
        Color::Rgb(r, g, b) => return format!("`#{r:02x}{g:02x}{b:02x}`"),
        Color::Indexed(index) => return format!("`idx {index}`"),
        Color::Default => "default",
        Color::Black => "black",
        Color::Red => "red",
        Color::Green => "green",
        Color::Yellow => "yellow",
        Color::Blue => "blue",
        Color::Magenta => "magenta",
        Color::Cyan => "cyan",
        Color::White => "white",
        Color::BrightBlack => "bright black",
        Color::BrightRed => "bright red",
        Color::BrightGreen => "bright green",
        Color::BrightYellow => "bright yellow",
        Color::BrightBlue => "bright blue",
        Color::BrightMagenta => "bright magenta",
        Color::BrightCyan => "bright cyan",
        Color::BrightWhite => "bright white",
    };
    format!("*{name}*")
}

fn write_header(page: &mut String, combos: usize, renders: usize) {
    let _ = write!(
        page,
        "# Theme gallery\n\n\
         <!-- Generated by `cargo run -p xtask -- theme-gallery`. Do not edit by hand; \
         `cargo run -p xtask -- theme-gallery --check` fails when this page is stale. -->\n\n\
         Bundled themes and profiles rendered through the TUI's sample transcript \
         (the appearance picker preview) into a headless {GALLERY_WIDTH}×{GALLERY_HEIGHT} \
         BMUX buffer (trailing blank rows trimmed). Renders are plain text, so they show \
         structure, chrome glyphs and disclosure; the swatch tables show color.\n\n\
         Selection: every selectable bundled theme alone (dark variant), plus every \
         bundled profile over the default theme and `bcode`. Identical renders are \
         printed once and list every combination that produces them. This page shows \
         {renders} distinct renders of {combos} combinations (at most {MAX_RENDERS}).\n\n\
         Swatches name terminal-controlled colors (`default` and the 16 ANSI colors) \
         instead of guessing their values; `—` means the role is unset. *Selection* is \
         the selection background, or its foreground when it has none. Nerd Font icons \
         need a Nerd Font to display, and viewers that measure them differently from \
         the terminal show the rows they appear in misaligned.\n\n"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::text_width::display_width;

    fn bundled(id: &str, overlays: &[&str]) -> ResolvedThemeDefinition {
        let catalog = ThemeCatalog::bundled().unwrap();
        resolve(
            &catalog,
            &GalleryCombo {
                base: id.to_owned(),
                overlays: overlays.iter().map(|&o| o.to_owned()).collect(),
            },
            ResolvedThemeVariant::Dark,
        )
        .unwrap()
    }

    #[test]
    fn renders_are_exactly_bounded_by_the_requested_size() {
        for (width, height) in [(0, 0), (1, 1), (3, 2), (12, 5), (80, 56)] {
            let rows = render_sample(&bundled("bcode", &[]), width, height);
            assert_eq!(rows.len(), usize::from(height));
            for row in &rows {
                assert!(display_width(row) <= usize::from(width), "{width}: {row:?}");
            }
        }
    }

    #[test]
    fn profiles_change_the_rendered_structure() {
        let plain = render_sample(&bundled("bcode", &[]), 80, 56);
        let bare = render_sample(&bundled("bcode", &["compact"]), 80, 56);
        assert_ne!(plain, bare);
        assert!(plain.iter().any(|row| row.contains("let row = 1")));
    }

    #[test]
    fn rendering_is_deterministic_and_restores_the_presented_theme() {
        let before = crate::render::semantic_state_theme();
        let first = render_sample(&bundled("crt", &[]), 80, 56);
        let second = render_sample(&bundled("crt", &[]), 80, 56);
        assert_eq!(first, second);
        assert_eq!(crate::render::semantic_state_theme(), before);
    }

    #[test]
    fn gallery_covers_every_theme_and_profile_within_bounds() {
        let page = gallery_markdown().unwrap();
        let catalog = ThemeCatalog::bundled().unwrap();
        for definition in catalog.definitions() {
            let id = definition.id();
            match definition.kind() {
                ThemeKind::Theme => assert!(page.contains(&format!("| `{id}` |")), "{id}"),
                ThemeKind::Profile => assert!(page.contains(&format!("+ {id}")), "{id}"),
                ThemeKind::Base => {}
            }
        }
        assert!(page.matches("```text").count() <= MAX_RENDERS);
        assert!(!page.contains("(clipped at"), "raise GALLERY_HEIGHT");
        assert!(page.ends_with("|\n"));
        assert_eq!(page, gallery_markdown().unwrap());
    }

    #[test]
    fn terminal_colors_are_named_not_guessed() {
        assert_eq!(color_name(Some(Color::Default)), "*default*");
        assert_eq!(color_name(Some(Color::Rgb(1, 2, 255))), "`#0102ff`");
        assert_eq!(color_name(Some(Color::Indexed(7))), "`idx 7`");
        assert_eq!(color_name(None), "—");
    }
}
