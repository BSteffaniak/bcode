//! Terminal adaptation of resolved theme chrome.

use bcode_presentation_policy::models::{
    FactMatcher, LayerSource, OneOrMany, PresentationRule, PropertyPatch, RuleLayer, Spacing,
    UnitKind,
};
use bcode_tui_theme::chrome::{
    BorderStyle, Density, FocusEffect, IconSet, SpeakerStyle, TuiChrome,
};
use bcode_tui_theme::definition::{
    ContainerBorder, ContainerLayout, ContainerRecipe, ContainerWidth, ResolvedThemeDefinition,
};
use bmux_tui::chrome::BorderSet;
use bmux_tui::gradient::ColorGradient;
use bmux_tui::text_width::display_width;
use bmux_tui_components::theme::ComponentChrome;

/// BMUX glyph set for a theme border style; `None` for no border or a
/// leading-bar-only style that has no panel glyph set.
#[must_use]
pub const fn border_set(style: BorderStyle) -> Option<BorderSet> {
    match style {
        BorderStyle::None | BorderStyle::Left => None,
        BorderStyle::Ascii => Some(BorderSet::ASCII),
        BorderStyle::Single => Some(BorderSet::SINGLE),
        BorderStyle::Rounded => Some(BorderSet::ROUNDED),
        BorderStyle::Thick => Some(BorderSet::THICK),
        BorderStyle::Double => Some(BorderSet::DOUBLE),
    }
}

/// Focus gradient for the chrome, when its effect and colors allow one.
///
/// Gradients need two explicit RGB endpoints; terminal-owned colors keep the
/// focused border style instead of guessing RGB values.
#[must_use]
pub fn focus_gradient(chrome: &TuiChrome) -> Option<ColorGradient> {
    if chrome.focus == FocusEffect::None {
        return None;
    }
    let mut colors = chrome.focus_colors.iter().filter_map(|color| color.rgb());
    let from = colors.next()?;
    let to = colors.next()?;
    Some(ColorGradient::new(from, to))
}

/// Chrome for one surface: an unset style keeps the component's default glyphs.
fn surface_chrome(style: Option<BorderStyle>, chrome: &TuiChrome) -> ComponentChrome {
    ComponentChrome {
        border_set: style.and_then(border_set),
        focus_gradient: focus_gradient(chrome),
    }
}

/// Chrome for panels, panes, pickers, and inputs.
#[must_use]
pub fn component_chrome(theme: &ResolvedThemeDefinition) -> ComponentChrome {
    surface_chrome(theme.chrome.borders.panel, &theme.chrome)
}

/// Chrome for dialogs and modals.
#[must_use]
pub fn dialog_chrome(theme: &ResolvedThemeDefinition) -> ComponentChrome {
    surface_chrome(theme.chrome.borders.dialog, &theme.chrome)
}

/// Terminal chrome consumed by transcript units and the composer: icons,
/// disclosure markers, density, the tool and composer border styles, and the
/// speaker treatment of messages.
///
/// The default reproduces the transcript of a theme without `[chrome.tui]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct UnitChrome {
    /// Icon set for the header `icon` field and the disclosure marker glyphs.
    pub icons: IconSet,
    /// Spacing density; feeds the host default `spacing` of tools and groups.
    pub density: Density,
    /// Border style around tool units; `None` keeps the theme's container recipes.
    pub tool_border: Option<BorderStyle>,
    /// Border style of the input composer; `None` keeps its single-line frame.
    pub composer_border: Option<BorderStyle>,
    /// Speaker treatment of user and assistant message headers.
    pub speaker: SpeakerStyle,
}

/// Header icon candidates for one tool category, most specific first.
struct IconCandidates {
    nerd: &'static str,
    unicode: &'static str,
    ascii: &'static str,
}

/// Disclosure marker glyphs per icon set, as (expanded, collapsed) candidates.
const NERD_MARKERS: (&str, &str) = ("\u{f107}", "\u{f105}");
const UNICODE_MARKERS: (&str, &str) = ("▾", "▸");
const ASCII_MARKERS: (&str, &str) = ("v", ">");

/// Widest glyph a header icon or marker may occupy, in terminal cells.
const MAX_GLYPH_CELLS: usize = 2;

/// First candidate whose measured width is one or two cells.
///
/// Glyphs the width tables report as zero-width (unassigned or combining) or
/// wider than two cells fall back to the next, more portable candidate.
fn measured<'a>(candidates: &[&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .copied()
        .find(|glyph| (1..=MAX_GLYPH_CELLS).contains(&display_width(glyph)))
}

impl UnitChrome {
    /// Unit chrome of a resolved theme definition; `None` yields the default.
    #[must_use]
    pub fn from_theme(theme: Option<&ResolvedThemeDefinition>) -> Self {
        theme.map_or_else(Self::default, |theme| Self {
            icons: theme.chrome.icons,
            density: theme.chrome.density,
            tool_border: theme.chrome.borders.tool,
            composer_border: theme.chrome.borders.composer,
            speaker: theme.chrome.speaker,
        })
    }

    /// Disclosure marker for the expanded or collapsed state.
    ///
    /// `none` icon sets use ASCII markers, `unicode` the triangles every
    /// theme used before, and `nerd` Nerd Font chevrons. A glyph whose
    /// measured width differs from the Unicode marker falls back to it, so
    /// the marker column never shifts between sets.
    #[must_use]
    pub fn marker(self, expanded: bool) -> &'static str {
        let pick = |pair: (&'static str, &'static str)| if expanded { pair.0 } else { pair.1 };
        let unicode = pick(UNICODE_MARKERS);
        match self.icons {
            IconSet::None => pick(ASCII_MARKERS),
            IconSet::Unicode => unicode,
            IconSet::Nerd => {
                let nerd = pick(NERD_MARKERS);
                if display_width(nerd) == display_width(unicode) {
                    nerd
                } else {
                    unicode
                }
            }
        }
    }

    /// Header icon for a tool name, or `None` when the icon set is `none`.
    ///
    /// The tool is categorized by its name (`shell.run`, `filesystem.read`,
    /// `grep`, ...). Nerd glyphs fall back to Unicode, and Unicode to ASCII,
    /// when the measured width is not one or two cells.
    #[must_use]
    pub fn tool_icon(self, tool: &str) -> Option<&'static str> {
        let icon = tool_icon_candidates(tool);
        match self.icons {
            IconSet::None => None,
            IconSet::Unicode => measured(&[icon.unicode, icon.ascii]),
            IconSet::Nerd => measured(&[icon.nerd, icon.unicode, icon.ascii]),
        }
    }

    /// Border glyphs drawn around tool units, when the tool border style
    /// draws a full frame.
    #[must_use]
    pub fn tool_border_set(self) -> Option<BorderSet> {
        self.tool_border.and_then(border_set)
    }

    /// Container recipe for a tool unit under the tool border style.
    ///
    /// * unset: the theme's recipe, unchanged;
    /// * `none`: no container;
    /// * `left`: a leading bar, keeping the recipe's padding;
    /// * framed styles: a full-width panel with a border on all sides, whose
    ///   glyphs come from [`Self::tool_border_set`].
    #[must_use]
    pub const fn tool_container(self, recipe: ContainerRecipe) -> ContainerRecipe {
        match self.tool_border {
            None => recipe,
            Some(BorderStyle::None) => ContainerRecipe {
                layout: ContainerLayout::Plain,
                border: ContainerBorder::None,
                ..recipe
            },
            Some(BorderStyle::Left) => ContainerRecipe {
                layout: ContainerLayout::LeftBar,
                border: ContainerBorder::Left,
                ..recipe
            },
            Some(
                BorderStyle::Ascii
                | BorderStyle::Single
                | BorderStyle::Rounded
                | BorderStyle::Thick
                | BorderStyle::Double,
            ) => ContainerRecipe {
                layout: ContainerLayout::Panel,
                width: ContainerWidth::Full,
                border: ContainerBorder::All,
                ..recipe
            },
        }
    }

    /// Default `spacing` for tool and group units under this density, or
    /// `None` for `cozy`, the built-in default.
    #[must_use]
    pub const fn density_spacing(self) -> Option<Spacing> {
        match self.density {
            Density::Compact => Some(Spacing {
                before: 0,
                after: 0,
            }),
            Density::Cozy => None,
            Density::Comfortable => Some(Spacing {
                before: 1,
                after: 1,
            }),
        }
    }

    /// Host-default rule layer carrying the density spacing, lowest in
    /// precedence so plugin, theme, configuration, and user rules override it.
    ///
    /// Only tools and groups are spaced: message frames keep their natural
    /// rows, which the shared Markdown projection relies on.
    #[must_use]
    pub fn density_layer(self) -> Option<RuleLayer> {
        let spacing = self.density_spacing()?;
        Some(RuleLayer {
            source: LayerSource::Host,
            rules: vec![PresentationRule {
                matcher: FactMatcher {
                    kind: OneOrMany(vec![UnitKind::Tool, UnitKind::Group]),
                    ..FactMatcher::default()
                },
                set: PropertyPatch {
                    spacing: Some(spacing),
                    ..PropertyPatch::default()
                },
            }],
        })
    }
}

fn tool_icon_candidates(tool: &str) -> IconCandidates {
    let name = tool.to_ascii_lowercase();
    let leaf = name.rsplit('.').next().unwrap_or(&name);
    let is = |words: &[&str]| words.contains(&leaf);
    let (nerd, unicode, ascii) = if name.starts_with("git") {
        ("\u{e702}", "±", "+")
    } else if is(&["run", "bash", "shell", "terminal", "exec", "command"]) {
        ("\u{f489}", "$", "$")
    } else if is(&["read", "view", "cat", "body", "open"]) {
        ("\u{f15c}", "≡", "=")
    } else if is(&[
        "write",
        "edit",
        "multi_edit",
        "patch",
        "apply_patch",
        "create",
    ]) {
        ("\u{f044}", "✎", "*")
    } else if is(&["grep", "search", "find", "glob", "query"]) {
        ("\u{f002}", "⌕", "?")
    } else if is(&["list", "ls", "stat", "exists", "tree"]) {
        ("\u{f07b}", "▤", "#")
    } else if is(&["fetch", "web", "web_search", "url", "http"]) || name.starts_with("web") {
        ("\u{f0ac}", "◎", "@")
    } else {
        ("\u{f0ad}", "•", "-")
    };
    IconCandidates {
        nerd,
        unicode,
        ascii,
    }
}

#[cfg(test)]
mod tests {
    use bcode_tui_theme::chrome::TuiChrome;
    use bmux_tui::prelude::Line;
    use bmux_tui::style::Color;

    use super::*;

    #[test]
    fn gradients_need_two_rgb_colors_and_an_effect() {
        let mut chrome = TuiChrome {
            focus: FocusEffect::Gradient,
            focus_colors: vec![
                Color::Rgb(255, 0, 170),
                Color::Cyan,
                Color::Rgb(0, 255, 255),
            ],
            ..TuiChrome::default()
        };
        let gradient = focus_gradient(&chrome).expect("two rgb colors");
        assert_eq!(gradient.from, bmux_color::Rgb::new(255, 0, 170));
        assert_eq!(gradient.to, bmux_color::Rgb::new(0, 255, 255));
        chrome.focus_colors = vec![Color::Cyan, Color::Magenta];
        assert_eq!(focus_gradient(&chrome), None);
        chrome.focus = FocusEffect::None;
        chrome.focus_colors = vec![Color::Rgb(1, 1, 1), Color::Rgb(2, 2, 2)];
        assert_eq!(focus_gradient(&chrome), None);
    }

    #[test]
    fn themes_without_chrome_keep_component_defaults() {
        let theme = ResolvedThemeDefinition::empty("plain", "Plain");
        assert_eq!(component_chrome(&theme), ComponentChrome::DEFAULT);
        assert_eq!(dialog_chrome(&theme), ComponentChrome::DEFAULT);
    }

    #[test]
    fn border_styles_map_to_bmux_glyph_sets() {
        assert_eq!(border_set(BorderStyle::Rounded), Some(BorderSet::ROUNDED));
        assert_eq!(border_set(BorderStyle::None), None);
        assert_eq!(border_set(BorderStyle::Left), None);
    }

    fn chrome(icons: IconSet) -> UnitChrome {
        UnitChrome {
            icons,
            ..UnitChrome::default()
        }
    }

    #[test]
    fn default_unit_chrome_matches_themes_without_chrome() {
        let theme = ResolvedThemeDefinition::empty("plain", "Plain");
        let defaults = UnitChrome::from_theme(Some(&theme));
        assert_eq!(defaults, UnitChrome::default());
        assert_eq!(UnitChrome::from_theme(None), UnitChrome::default());
        assert_eq!(defaults.marker(true), "▾");
        assert_eq!(defaults.marker(false), "▸");
        assert_eq!(defaults.density_layer(), None);
        let recipe = ContainerRecipe {
            layout: ContainerLayout::LeftBar,
            width: ContainerWidth::Content,
            border: ContainerBorder::Left,
            padding_x: 1,
            padding_y: 0,
        };
        assert_eq!(defaults.tool_container(recipe), recipe);
        assert_eq!(defaults.tool_border_set(), None);
    }

    #[test]
    fn marker_glyph_sets_follow_icons_with_equal_measured_widths() {
        assert_eq!(chrome(IconSet::None).marker(false), ">");
        assert_eq!(chrome(IconSet::None).marker(true), "v");
        for expanded in [true, false] {
            let unicode = chrome(IconSet::Unicode).marker(expanded);
            let nerd = chrome(IconSet::Nerd).marker(expanded);
            assert_eq!(display_width(nerd), display_width(unicode));
            assert_eq!(display_width(chrome(IconSet::None).marker(expanded)), 1);
        }
    }

    #[test]
    fn tool_icons_are_measured_and_absent_without_an_icon_set() {
        let tools = [
            "shell.run",
            "filesystem.read",
            "filesystem.multi_edit",
            "grep",
            "filesystem.list",
            "git.clone",
            "web.fetch",
            "mystery",
            "",
            "読み込み",
        ];
        for tool in tools {
            assert_eq!(chrome(IconSet::None).tool_icon(tool), None);
            for icons in [IconSet::Unicode, IconSet::Nerd] {
                let icon = chrome(icons).tool_icon(tool).expect("icon");
                assert!(
                    (1..=MAX_GLYPH_CELLS).contains(&display_width(icon)),
                    "{tool}: {icon:?}"
                );
            }
        }
        assert_ne!(
            chrome(IconSet::Unicode).tool_icon("filesystem.read"),
            chrome(IconSet::Unicode).tool_icon("shell.run")
        );
    }

    #[test]
    fn unmeasurable_glyphs_fall_back_to_portable_candidates() {
        // A lone combining mark measures zero cells; a wide pair measures four.
        assert_eq!(measured(&["\u{301}", "≡", "="]), Some("≡"));
        assert_eq!(measured(&["漢字", "="]), Some("="));
        assert_eq!(measured(&["漢"]), Some("漢"));
        assert_eq!(measured(&["\u{301}"]), None);
    }

    #[test]
    fn tool_border_styles_map_to_container_recipes() {
        let recipe = ContainerRecipe {
            layout: ContainerLayout::Plain,
            width: ContainerWidth::Content,
            border: ContainerBorder::None,
            padding_x: 1,
            padding_y: 0,
        };
        let with = |style| UnitChrome {
            tool_border: Some(style),
            ..UnitChrome::default()
        };
        let none = with(BorderStyle::None).tool_container(ContainerRecipe {
            layout: ContainerLayout::Panel,
            border: ContainerBorder::All,
            ..recipe
        });
        assert_eq!(none.layout, ContainerLayout::Plain);
        assert_eq!(none.border, ContainerBorder::None);
        let left = with(BorderStyle::Left).tool_container(recipe);
        assert_eq!(
            (left.layout, left.border, left.padding_x),
            (ContainerLayout::LeftBar, ContainerBorder::Left, 1)
        );
        let rounded = with(BorderStyle::Rounded);
        let framed = rounded.tool_container(recipe);
        assert_eq!(
            (framed.layout, framed.border, framed.width),
            (
                ContainerLayout::Panel,
                ContainerBorder::All,
                ContainerWidth::Full
            )
        );
        assert_eq!(rounded.tool_border_set(), Some(BorderSet::ROUNDED));
        assert_eq!(with(BorderStyle::Left).tool_border_set(), None);
    }

    #[test]
    fn density_spaces_tools_and_groups_from_the_host_layer() {
        let with = |density| UnitChrome {
            density,
            ..UnitChrome::default()
        };
        let compact = with(Density::Compact).density_layer().expect("layer");
        assert_eq!(compact.source, LayerSource::Host);
        assert_eq!(
            compact.rules[0].matcher.kind.0,
            vec![UnitKind::Tool, UnitKind::Group]
        );
        assert_eq!(
            compact.rules[0].set.spacing,
            Some(Spacing {
                before: 0,
                after: 0
            })
        );
        assert_eq!(
            with(Density::Comfortable).density_spacing(),
            Some(Spacing {
                before: 1,
                after: 1
            })
        );
        assert_eq!(with(Density::Cozy).density_layer(), None);
    }

    fn framed_rows(content: &str, set: BorderSet, width: u16, padding_x: u16) -> Vec<Line> {
        let mut rows = vec![Line::from(content)];
        bcode_tui_components::transcript::apply_transcript_container(
            &mut rows,
            0,
            bcode_tui_components::transcript::TranscriptContainerStyle {
                recipe: bcode_tui_components::transcript::TranscriptContainerRecipe {
                    layout: bcode_tui_components::transcript::TranscriptContainerLayout::Panel,
                    width: bcode_tui_components::transcript::TranscriptContainerWidth::Full,
                    border: bcode_tui_components::transcript::TranscriptContainerBorder::All,
                    padding_x,
                    padding_y: 0,
                },
                style: bmux_tui::style::Style::default(),
                border_set: set,
            },
            width,
        );
        rows
    }

    #[test]
    fn tool_frames_paint_the_chrome_border_set_without_touching_content() {
        let text = framed_rows("a│─┌b", BorderSet::ROUNDED, 12, 1)
            .iter()
            .map(Line::plain_text)
            .collect::<Vec<_>>();
        assert_eq!(text[0], "╭──────────╮");
        assert_eq!(text[1], "│ a│─┌b    │");
        assert_eq!(text[2], "╰──────────╯");
        let thick = framed_rows("x", BorderSet::THICK, 12, 1);
        assert!(thick[1].plain_text().starts_with(BorderSet::THICK.vertical));
    }

    #[test]
    fn framed_rows_stay_within_the_available_width_at_tiny_widths() {
        for width in [0_u16, 1, 2, 3, 6] {
            for set in [BorderSet::SINGLE, BorderSet::DOUBLE, BorderSet::ROUNDED] {
                let rows = framed_rows("漢字👍e\u{301}", set, width, 0);
                assert_eq!(rows.len(), 3, "width {width}");
                let widths = rows
                    .iter()
                    .map(|row| display_width(&row.plain_text()))
                    .collect::<Vec<_>>();
                let expected = usize::from(width.max(1));
                assert!(
                    widths.iter().all(|row| *row == expected),
                    "width {width}: {widths:?}"
                );
            }
        }
    }
}
