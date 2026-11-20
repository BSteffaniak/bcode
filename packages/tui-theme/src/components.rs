//! Component styles derived from a resolved theme definition.
//!
//! Maps semantic theme roles onto the BMUX component theme and the presentation styles every
//! terminal program shares: source and diff viewers, markdown, and syntax colors. A program that
//! resolves a theme with [`crate::definition::ThemeCatalog`] gets the same component colors and
//! chrome as the bcode chat interface from [`component_styles`].
//!
//! Every role is optional. Unset roles fall back to related roles and finally to terminal
//! defaults, so a theme without a role (or no theme at all) still yields readable styles.

use crate::chrome::{BorderStyle, FocusEffect, TuiChrome};
use crate::definition::ResolvedThemeDefinition;
use bcode_markdown_render::MarkdownTheme;
use bcode_syntax_render::{SyntaxColor, SyntaxPalette};
use bmux_tui::chrome::BorderSet;
use bmux_tui::gradient::ColorGradient;
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui_components::diff_viewer::DiffViewerStyle;
use bmux_tui_components::source_viewer::SourceViewerStyle;
use bmux_tui_components::theme::{
    ComponentChrome, ComponentStateSurfaces, ComponentSurfaces, ComponentTheme,
};

/// Styles of the shared terminal components for one resolved theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentStyles {
    /// Generic component theme: canvas, surfaces, text tiers, states, borders, and chrome.
    pub component: ComponentTheme,
    /// Focused-control surface (`control.focused`), defaulting to the focused border style.
    pub control_focused: Style,
    /// Source-card presentation.
    pub source: SourceViewerStyle,
    /// Diff presentation.
    pub diff: DiffViewerStyle,
    /// The review's file chrome, where the theme sets it (`source.file.*`).
    pub files: FileChromeRoles,
    /// The review's thread cards, where the theme sets them (`source.thread.*`).
    pub threads: ThreadCardRoles,
    /// Markdown presentation.
    pub markdown: MarkdownTheme,
    /// Syntax colors for code.
    pub syntax: SyntaxPalette,
    /// Border glyphs and focus effects for dialogs and modals.
    pub dialog_chrome: ComponentChrome,
    /// Which `<picture>` sources Markdown shows: the theme's light/dark appearance.
    pub markdown_color_scheme: Option<bcode_markdown_render::MarkdownColorScheme>,
}

/// The `source.file.*` roles a theme sets; the review derives the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileChromeRoles {
    /// `source.file.header`: a file's header row.
    pub header: Option<Style>,
    /// `source.file.desk`: the space between files.
    pub desk: Option<Style>,
    /// `source.file.edge`: a file's card edge and the rule between files.
    pub edge: Option<Style>,
    /// `source.file.label`: the label on the rule between files.
    pub label: Option<Style>,
}

/// The `source.thread.*` roles a theme sets; the review derives the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThreadCardRoles {
    /// `source.thread.background`: a thread card's fill.
    pub background: Option<Style>,
    /// `source.thread.border`: a viewed thread's frame.
    pub border: Option<Style>,
    /// `source.thread.unviewed`: an unviewed thread's frame.
    pub unviewed: Option<Style>,
    /// `source.thread.waiting`: the frame of a thread waiting on you.
    pub waiting: Option<Style>,
    /// `source.thread.resolved`: a resolved thread's frame.
    pub resolved: Option<Style>,
}

/// Derive component styles from `theme`, or terminal defaults when there is none.
///
/// `accent` colors the focused border and the informational state when the theme sets neither.
#[must_use]
pub fn component_styles(theme: Option<&ResolvedThemeDefinition>, accent: Color) -> ComponentStyles {
    let style = |role: &str| theme.and_then(|theme| theme.style(role));
    let color = |name: &str| theme.and_then(|theme| theme.color(name));
    let text = style("text.primary")
        .unwrap_or_else(|| color("text").map_or_else(Style::new, |color| Style::new().fg(color)));
    let muted = style("text.muted").unwrap_or_else(|| {
        color("muted").map_or_else(
            || Style::new().add_modifier(Modifier::DIM),
            |color| Style::new().fg(color),
        )
    });
    let border = style("border.default").unwrap_or(muted);
    let focused = style("border.focused").unwrap_or_else(|| Style::new().fg(accent));
    let canvas = style("canvas").unwrap_or_else(Style::new).patch(text);
    let raised = style("surface.raised").unwrap_or(canvas).patch(text);
    let overlay = style("surface.overlay").unwrap_or(canvas).patch(text);
    let selection =
        style("selection.active").unwrap_or_else(|| Style::new().add_modifier(Modifier::REVERSED));
    let info = style("state.info").unwrap_or_else(|| Style::new().fg(accent));
    let success = style("state.success").unwrap_or(text);
    let warning = style("state.warning").unwrap_or(text);
    let error = style("state.error").unwrap_or(text);
    let title = style("title").unwrap_or_else(|| info.add_modifier(Modifier::BOLD));
    let link = style("link")
        .or_else(|| style("markdown.link"))
        .unwrap_or_else(|| info.add_modifier(Modifier::UNDERLINE));
    let component = ComponentTheme {
        canvas,
        surfaces: ComponentSurfaces {
            normal: canvas,
            raised,
            overlay,
            scrim: style("surface.scrim"),
        },
        text,
        accent: style("accent").unwrap_or(info),
        title,
        focused,
        hover: style("control.hover").unwrap_or(info),
        selected: selection,
        selected_inactive: style("selection.inactive").unwrap_or(selection),
        disabled: muted.add_modifier(Modifier::DIM),
        muted,
        link,
        info,
        success,
        warning,
        error,
        state_surfaces: ComponentStateSurfaces {
            info: style("state.info.surface").unwrap_or_default(),
            success: style("state.success.surface").unwrap_or_default(),
            warning: style("state.warning.surface").unwrap_or_default(),
            error: style("state.error.surface").unwrap_or_default(),
        },
        border,
        border_focused: style("border.focused.panel").unwrap_or(focused),
        chrome: theme.map_or(ComponentChrome::DEFAULT, component_chrome),
    };
    let mut source = SourceViewerStyle::from(component);
    source.source = style("source.text").unwrap_or(source.source);
    source.border = style("source.border").unwrap_or(source.border);
    source.gutter = style("source.gutter").unwrap_or(source.gutter);
    source.truncated = style("source.truncated").unwrap_or(source.truncated);
    let mut diff = DiffViewerStyle::from(component);
    diff.text = style("diff.text").unwrap_or(diff.text);
    diff.muted = style("diff.muted").unwrap_or(diff.muted);
    diff.title = style("diff.title").unwrap_or(diff.title);
    diff.label = style("diff.label").unwrap_or(diff.label);
    diff.added = style("diff.added").unwrap_or(diff.added);
    diff.removed = style("diff.removed").unwrap_or(diff.removed);
    diff.hunk = style("diff.hunk").unwrap_or(diff.hunk);
    diff.added_row = style("diff.added_row").unwrap_or(diff.added_row);
    diff.removed_row = style("diff.removed_row").unwrap_or(diff.removed_row);
    diff.added_emphasis = style("diff.added_emphasis").unwrap_or(diff.added_emphasis);
    diff.removed_emphasis = style("diff.removed_emphasis").unwrap_or(diff.removed_emphasis);
    ComponentStyles {
        component,
        control_focused: style("control.focused").unwrap_or(focused),
        source,
        diff,
        files: FileChromeRoles {
            header: style("source.file.header"),
            desk: style("source.file.desk"),
            edge: style("source.file.edge"),
            label: style("source.file.label"),
        },
        threads: ThreadCardRoles {
            background: style("source.thread.background"),
            border: style("source.thread.border"),
            unviewed: style("source.thread.unviewed"),
            waiting: style("source.thread.waiting"),
            resolved: style("source.thread.resolved"),
        },
        markdown: markdown_theme(theme, text, muted),
        syntax: syntax_palette(theme, text, muted, info, success, warning),
        dialog_chrome: theme.map_or(ComponentChrome::DEFAULT, dialog_chrome),
        markdown_color_scheme: theme.and_then(markdown_color_scheme),
    }
}

/// BMUX glyph set for a theme border style.
///
/// `None` for no border or a leading-bar-only style that has no panel glyph set. `rules` uses
/// single-line glyphs; surfaces that support it draw only the top and bottom sides.
#[must_use]
pub const fn border_set(style: BorderStyle) -> Option<BorderSet> {
    match style {
        BorderStyle::None | BorderStyle::Left => None,
        BorderStyle::Ascii => Some(BorderSet::ASCII),
        BorderStyle::Single | BorderStyle::Rules => Some(BorderSet::SINGLE),
        BorderStyle::Rounded => Some(BorderSet::ROUNDED),
        BorderStyle::Thick => Some(BorderSet::THICK),
        BorderStyle::Double => Some(BorderSet::DOUBLE),
    }
}

/// Focus gradient for the chrome, when its effect and colors allow one.
///
/// Gradients need two explicit RGB endpoints; terminal-owned colors keep the focused border
/// style instead of guessing RGB values.
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

pub use bmux_tui_components::terminal_viewer::TerminalPalette;

/// Palette slots for ANSI colors 0-15, in index order.
pub const ANSI_SLOTS: [&str; 16] = [
    "ansi_black",
    "ansi_red",
    "ansi_green",
    "ansi_yellow",
    "ansi_blue",
    "ansi_magenta",
    "ansi_cyan",
    "ansi_white",
    "ansi_bright_black",
    "ansi_bright_red",
    "ansi_bright_green",
    "ansi_bright_yellow",
    "ansi_bright_blue",
    "ansi_bright_magenta",
    "ansi_bright_cyan",
    "ansi_bright_white",
];

/// The terminal-output palette a theme declares: its sixteen `ansi_*` slots, with the
/// optional `terminal_fg` / `terminal_bg` defaults.
///
/// `None` unless every ANSI slot is declared, so terminal output keeps the terminal's own
/// colors for themes that do not define a complete set.
#[must_use]
pub fn terminal_palette(theme: &ResolvedThemeDefinition) -> Option<TerminalPalette> {
    let mut ansi = [Color::Default; 16];
    for (color, slot) in ansi.iter_mut().zip(ANSI_SLOTS) {
        *color = theme.color(slot)?;
    }
    let mut palette = TerminalPalette::new(ansi);
    if let Some(foreground) = theme.color("terminal_fg") {
        palette = palette.with_foreground(foreground);
    }
    if let Some(background) = theme.color("terminal_bg") {
        palette = palette.with_background(background);
    }
    Some(palette)
}

/// The `<picture>` color scheme for Markdown rendered under `theme` (its
/// [`ResolvedThemeDefinition::appearance`]); `None` when the appearance is unknown.
#[must_use]
pub fn markdown_color_scheme(
    theme: &ResolvedThemeDefinition,
) -> Option<bcode_markdown_render::MarkdownColorScheme> {
    use crate::definition::ResolvedThemeVariant;
    use bcode_markdown_render::MarkdownColorScheme;
    match theme.appearance() {
        ResolvedThemeVariant::Light => Some(MarkdownColorScheme::Light),
        ResolvedThemeVariant::Dark => Some(MarkdownColorScheme::Dark),
        ResolvedThemeVariant::Unspecified => None,
    }
}

fn syntax_palette(
    theme: Option<&ResolvedThemeDefinition>,
    text: Style,
    muted: Style,
    info: Style,
    success: Style,
    warning: Style,
) -> SyntaxPalette {
    let style_color =
        |style: Style, fallback: SyntaxColor| style.fg.map_or(fallback, SyntaxColor::from_tui);
    let text_color = style_color(text, SyntaxColor::Default);
    let muted_color = style_color(muted, text_color);
    let info_color = style_color(info, text_color);
    let success_color = style_color(success, text_color);
    let warning_color = style_color(warning, text_color);
    let color = |role: &str, fallback: SyntaxColor| {
        theme
            .and_then(|theme| theme.style(role).and_then(|style| style.fg))
            .map_or(fallback, SyntaxColor::from_tui)
    };
    // Refined token colors stay unset unless the theme declares them, so those tokens keep
    // their base role color.
    let refined = |role: &str| {
        theme
            .and_then(|theme| theme.style(role).and_then(|style| style.fg))
            .map(SyntaxColor::from_tui)
    };
    SyntaxPalette {
        text: color("syntax.text", text_color),
        comment: color("syntax.comment", muted_color),
        keyword: color("syntax.keyword", info_color),
        function: color("syntax.function", info_color),
        variable: color("syntax.variable", text_color),
        string: color("syntax.string", success_color),
        number: color("syntax.number", warning_color),
        type_name: color("syntax.type", info_color),
        operator: color("syntax.operator", text_color),
        punctuation: color("syntax.punctuation", text_color),
        heading: color("syntax.heading", info_color),
        link: color("syntax.link", info_color),
        raw: color("syntax.raw", success_color),
        constant: refined("syntax.constant"),
        parameter: refined("syntax.parameter"),
        property: refined("syntax.property"),
        builtin: refined("syntax.builtin"),
        tag: refined("syntax.tag"),
        attribute: refined("syntax.attribute"),
    }
}

fn markdown_theme(
    theme: Option<&ResolvedThemeDefinition>,
    text: Style,
    muted: Style,
) -> MarkdownTheme {
    let defaults = MarkdownTheme::default();
    let role = |name: &str, fallback: Style| {
        theme
            .and_then(|theme| theme.style(name))
            .unwrap_or(fallback)
    };
    MarkdownTheme {
        text: role("markdown.text", text),
        heading: role("markdown.heading", defaults.heading),
        link: role("markdown.link", defaults.link),
        strong: role("markdown.strong", defaults.strong),
        emphasis: role("markdown.emphasis", defaults.emphasis),
        strikethrough: role("markdown.strikethrough", defaults.strikethrough),
        inline_code: role("markdown.inline_code", defaults.inline_code),
        code_block_text: role("markdown.code_block_text", defaults.code_block_text),
        code_block_border: role("markdown.code_block_border", muted),
        blockquote_bar: role("markdown.blockquote_bar", muted),
        alert_note: role("markdown.alert_note", defaults.alert_note),
        alert_tip: role("markdown.alert_tip", defaults.alert_tip),
        alert_important: role("markdown.alert_important", defaults.alert_important),
        alert_warning: role("markdown.alert_warning", defaults.alert_warning),
        alert_caution: role("markdown.alert_caution", defaults.alert_caution),
        list_marker: role("markdown.list_marker", muted),
        task_checked: role("markdown.task_checked", muted),
        task_unchecked: role("markdown.task_unchecked", muted),
        table_border: role("markdown.table_border", muted),
        horizontal_rule: role("markdown.horizontal_rule", muted),
        keyboard: role("markdown.keyboard", defaults.keyboard),
        highlight: role("markdown.highlight", defaults.highlight),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::ThemeCatalog;

    fn bundled(
        id: &str,
        variant: crate::definition::ResolvedThemeVariant,
    ) -> ResolvedThemeDefinition {
        ThemeCatalog::bundled()
            .expect("catalog")
            .resolve(&crate::definition::ThemeSelection::new(id).variant(variant))
            .unwrap_or_else(|error| panic!("{id}: {error}"))
    }

    #[test]
    fn terminal_palette_is_the_declared_ansi_slots_or_none() {
        use crate::definition::ResolvedThemeVariant;
        let theme = bundled("tokyonight", ResolvedThemeVariant::Dark);
        let palette = terminal_palette(&theme).expect("tokyonight declares all sixteen");
        for (color, slot) in palette.ansi.iter().zip(ANSI_SLOTS) {
            assert_eq!(Some(*color), theme.color(slot), "{slot}");
        }
        assert_eq!(palette.foreground, theme.color("terminal_fg"));
        assert_eq!(palette.background, theme.color("terminal_bg"));

        // Without a complete set terminal output keeps the terminal's own colors.
        assert_eq!(
            terminal_palette(&bundled("nord", ResolvedThemeVariant::Dark)),
            None
        );
        assert_eq!(
            terminal_palette(&bundled("catppuccin-mocha", ResolvedThemeVariant::Dark)),
            None
        );
    }

    #[test]
    fn markdown_color_scheme_follows_the_presented_appearance() {
        use crate::definition::ResolvedThemeVariant;
        use bcode_markdown_render::MarkdownColorScheme;
        assert_eq!(
            markdown_color_scheme(&bundled("tokyonight", ResolvedThemeVariant::Light)),
            Some(MarkdownColorScheme::Light)
        );
        assert_eq!(
            markdown_color_scheme(&bundled("tokyonight", ResolvedThemeVariant::Dark)),
            Some(MarkdownColorScheme::Dark)
        );
        // Without a variant, the background's lightness decides; terminal colors say nothing.
        assert_eq!(
            markdown_color_scheme(&bundled("bcode-light", ResolvedThemeVariant::Unspecified)),
            Some(MarkdownColorScheme::Light)
        );
        assert_eq!(
            markdown_color_scheme(&bundled("bcode-dark", ResolvedThemeVariant::Unspecified)),
            Some(MarkdownColorScheme::Dark)
        );
        assert_eq!(
            markdown_color_scheme(&bundled(
                "terminal-native",
                ResolvedThemeVariant::Unspecified
            )),
            None
        );
    }

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

    #[test]
    fn missing_theme_uses_the_accent_for_focus_and_information() {
        let styles = component_styles(None, Color::Magenta);
        assert_eq!(styles.component.focused.fg, Some(Color::Magenta));
        assert_eq!(styles.component.info.fg, Some(Color::Magenta));
        assert_eq!(styles.control_focused, styles.component.focused);
        assert_eq!(styles.component.chrome, ComponentChrome::DEFAULT);
    }

    #[test]
    fn bundled_theme_roles_reach_components_and_code_presentation() {
        let catalog = ThemeCatalog::bundled().expect("bundled themes");
        let theme = catalog
            .resolve(&crate::definition::ThemeSelection::new("bcode-dark"))
            .expect("bcode-dark resolves");
        let styles = component_styles(Some(&theme), Color::Cyan);
        if let Some(text) = theme.style("text.primary") {
            assert_eq!(styles.component.text, text);
            assert_eq!(
                styles.component.canvas.fg,
                text.fg.or(styles.component.canvas.fg)
            );
        }
        if let Some(added) = theme.style("diff.added") {
            assert_eq!(styles.diff.added, added);
        }
        if let Some(keyword) = theme.style("syntax.keyword").and_then(|style| style.fg) {
            assert_eq!(styles.syntax.keyword, SyntaxColor::from_tui(keyword));
        }
        if let Some(border) = theme.style("border.default") {
            assert_eq!(styles.component.border, border);
        }
    }
}
