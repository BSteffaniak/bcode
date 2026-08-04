//! Theme resolution and presentation state.

mod definition;
mod discovery;

use std::num::ParseIntError;

use bcode_config::{TuiAgentAccentPolicy, TuiThemeVariant};
use bmux_tui::style::{Color, Modifier};

use super::app::BmuxApp;

/// Fully resolved target theme derived from app state and configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedTheme {
    /// Target accent color for chrome, dialogs, and selection affordances.
    pub accent: Color,
    /// Base terminal text style.
    pub text: bmux_tui::style::Style,
    /// Muted terminal text and separator style.
    pub muted: bmux_tui::style::Style,
    /// Default border style.
    pub border: bmux_tui::style::Style,
    /// Focused border/control style before dynamic accent animation.
    pub focused: bmux_tui::style::Style,
    /// Background style used by surfaces that explicitly request a fill.
    pub background: bmux_tui::style::Style,
    /// Selection style.
    pub selection: bmux_tui::style::Style,
    /// Markdown styles derived from semantic theme roles.
    pub markdown: bcode_markdown_render::MarkdownTheme,
    /// Semantic syntax palette for code presentation.
    pub syntax: bcode_syntax_render::SyntaxPalette,
    /// Stable resolved presentation fingerprint.
    pub fingerprint: u64,
}

impl ResolvedTheme {
    pub(crate) const fn presented(self, accent: Color) -> PresentedTheme {
        PresentedTheme {
            accent,
            text: self.text,
            muted: self.muted,
            border: self.border,
            focused: self.focused,
            background: self.background,
            selection: self.selection,
            markdown: self.markdown,
            syntax: self.syntax,
            fingerprint: self.fingerprint,
        }
    }
}

/// Theme currently presented by the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentedTheme {
    /// Presented accent color for chrome, dialogs, and selection affordances.
    pub accent: Color,
    /// Base terminal text style.
    pub text: bmux_tui::style::Style,
    /// Muted terminal text and separator style.
    pub muted: bmux_tui::style::Style,
    /// Default border style.
    pub border: bmux_tui::style::Style,
    /// Focused border/control style before dynamic accent animation.
    pub focused: bmux_tui::style::Style,
    /// Background style used by surfaces that explicitly request a fill.
    pub background: bmux_tui::style::Style,
    /// Selection style.
    pub selection: bmux_tui::style::Style,
    /// Markdown styles derived from semantic theme roles.
    pub markdown: bcode_markdown_render::MarkdownTheme,
    /// Semantic syntax palette for code presentation.
    pub syntax: bcode_syntax_render::SyntaxPalette,
    /// Stable resolved presentation fingerprint.
    pub fingerprint: u64,
}

impl From<ResolvedTheme> for PresentedTheme {
    fn from(theme: ResolvedTheme) -> Self {
        theme.presented(theme.accent)
    }
}

/// Neutral accent shown before daemon-backed agent metadata has loaded.
pub const PENDING_AGENT_METADATA_ACCENT: Color = Color::Rgb(100, 116, 139);

/// Resolve the safe initial theme before app-owned configuration is available.
#[must_use]
pub fn resolve_initial_theme() -> ResolvedTheme {
    let resolved = definition::ThemeCatalog::bundled()
        .and_then(|catalog| catalog.resolve(&definition::ThemeSelection::new("terminal-native")))
        .ok();
    resolved_theme(resolved.as_ref(), PENDING_AGENT_METADATA_ACCENT)
}

/// Resolve the target theme from app state.
#[must_use]
pub fn resolve_theme(app: &BmuxApp) -> ResolvedTheme {
    let project_root = app
        .working_directory()
        .map(std::path::Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let resolved = resolve_definition(
        &app.tui_config().theme.name,
        &app.tui_config().theme.overlays,
        app.tui_config().theme.variant,
        &project_root,
        &app.tui_config().theme.paths,
    );
    let configured_theme_accent = resolved_accent(resolved.as_ref());
    let accent = match app.tui_config().theme.agent_accent {
        TuiAgentAccentPolicy::ThemeOnly => configured_theme_accent,
        TuiAgentAccentPolicy::AgentWithThemeFallback => app
            .display_agent_accent()
            .and_then(parse_agent_accent_color)
            .unwrap_or_else(|| {
                if app.is_agent_metadata_hydrated() {
                    fallback_agent_accent_color(app.display_agent_id())
                } else {
                    configured_theme_accent
                }
            }),
    };
    resolved_theme(resolved.as_ref(), accent)
}

fn resolve_definition(
    theme_name: &str,
    overlays: &[String],
    variant: TuiThemeVariant,
    project_root: &std::path::Path,
    explicit_paths: &[std::path::PathBuf],
) -> Option<definition::ResolvedThemeDefinition> {
    let roots = discovery::default_theme_roots(
        &bcode_config::default_config_dir(),
        project_root,
        explicit_paths,
    );
    let catalog = discovery::discover_themes(&roots)
        .map(|discovered| {
            for diagnostic in &discovered.diagnostics {
                tracing::warn!(
                    path = %diagnostic.path.display(),
                    message = %diagnostic.message,
                    "theme definition skipped"
                );
            }
            tracing::debug!(
                external_theme_count = discovered.sources.len(),
                "theme catalog discovered"
            );
            discovered.catalog
        })
        .or_else(|_| definition::ThemeCatalog::bundled());
    let variant = match variant {
        TuiThemeVariant::Auto | TuiThemeVariant::Dark => definition::ResolvedThemeVariant::Dark,
        TuiThemeVariant::Light => definition::ResolvedThemeVariant::Light,
    };
    catalog
        .and_then(|catalog| {
            catalog.resolve(
                &definition::ThemeSelection::new(theme_name)
                    .overlays(overlays.to_vec())
                    .variant(variant),
            )
        })
        .ok()
}

fn resolved_accent(theme: Option<&definition::ResolvedThemeDefinition>) -> Color {
    theme
        .and_then(|theme| theme.style("border.focused").and_then(|style| style.fg))
        .or_else(|| theme.and_then(|theme| theme.color("accent")))
        .unwrap_or(PENDING_AGENT_METADATA_ACCENT)
}

fn resolved_theme(
    theme: Option<&definition::ResolvedThemeDefinition>,
    accent: Color,
) -> ResolvedTheme {
    let style = |role: &str| theme.and_then(|theme| theme.style(role));
    let color = |name: &str| theme.and_then(|theme| theme.color(name));
    let text = style("text.primary").unwrap_or_else(|| {
        color("text").map_or_else(bmux_tui::style::Style::new, |color| {
            bmux_tui::style::Style::new().fg(color)
        })
    });
    let muted = style("text.muted").unwrap_or_else(|| {
        color("muted").map_or_else(
            || bmux_tui::style::Style::new().add_modifier(Modifier::DIM),
            |color| bmux_tui::style::Style::new().fg(color),
        )
    });
    let border = style("border.default").unwrap_or(muted);
    let focused =
        style("border.focused").unwrap_or_else(|| bmux_tui::style::Style::new().fg(accent));
    let background = style("surface.base").unwrap_or_else(bmux_tui::style::Style::new);
    let selection = style("selection.active")
        .unwrap_or_else(|| bmux_tui::style::Style::new().add_modifier(Modifier::REVERSED));
    let markdown = markdown_theme(theme, text, muted);
    let syntax = syntax_palette(theme);
    ResolvedTheme {
        accent,
        text,
        muted,
        border,
        focused,
        background,
        selection,
        markdown,
        syntax,
        fingerprint: theme.map_or(0, |theme| theme.fingerprint),
    }
}

fn syntax_palette(
    theme: Option<&definition::ResolvedThemeDefinition>,
) -> bcode_syntax_render::SyntaxPalette {
    use bcode_syntax_render::{SyntaxColor, SyntaxPalette};

    let color = |role: &str, fallback: SyntaxColor| {
        theme
            .and_then(|theme| theme.style(role).and_then(|style| style.fg))
            .and_then(tui_color_to_syntax)
            .unwrap_or(fallback)
    };
    SyntaxPalette {
        text: color("syntax.text", SyntaxColor::rgb(212, 212, 212)),
        comment: color("syntax.comment", SyntaxColor::rgb(106, 153, 85)),
        keyword: color("syntax.keyword", SyntaxColor::rgb(86, 156, 214)),
        function: color("syntax.function", SyntaxColor::rgb(220, 220, 170)),
        variable: color("syntax.variable", SyntaxColor::rgb(156, 220, 254)),
        string: color("syntax.string", SyntaxColor::rgb(206, 145, 120)),
        number: color("syntax.number", SyntaxColor::rgb(181, 206, 168)),
        type_name: color("syntax.type", SyntaxColor::rgb(78, 201, 176)),
        operator: color("syntax.operator", SyntaxColor::rgb(212, 212, 212)),
        punctuation: color("syntax.punctuation", SyntaxColor::rgb(212, 212, 212)),
    }
}

const fn tui_color_to_syntax(color: Color) -> Option<bcode_syntax_render::SyntaxColor> {
    use bcode_syntax_render::SyntaxColor;

    match color {
        Color::Rgb(r, g, b) => Some(SyntaxColor::rgb(r, g, b)),
        Color::Black => Some(SyntaxColor::rgb(0, 0, 0)),
        Color::Red => Some(SyntaxColor::rgb(128, 0, 0)),
        Color::Green => Some(SyntaxColor::rgb(0, 128, 0)),
        Color::Yellow => Some(SyntaxColor::rgb(128, 128, 0)),
        Color::Blue => Some(SyntaxColor::rgb(0, 0, 128)),
        Color::Magenta => Some(SyntaxColor::rgb(128, 0, 128)),
        Color::Cyan => Some(SyntaxColor::rgb(0, 128, 128)),
        Color::White => Some(SyntaxColor::rgb(192, 192, 192)),
        Color::BrightBlack => Some(SyntaxColor::rgb(128, 128, 128)),
        Color::BrightRed => Some(SyntaxColor::rgb(255, 0, 0)),
        Color::BrightGreen => Some(SyntaxColor::rgb(0, 255, 0)),
        Color::BrightYellow => Some(SyntaxColor::rgb(255, 255, 0)),
        Color::BrightBlue => Some(SyntaxColor::rgb(0, 0, 255)),
        Color::BrightMagenta => Some(SyntaxColor::rgb(255, 0, 255)),
        Color::BrightCyan => Some(SyntaxColor::rgb(0, 255, 255)),
        Color::BrightWhite => Some(SyntaxColor::rgb(255, 255, 255)),
        Color::Default | Color::Indexed(_) => None,
    }
}

fn markdown_theme(
    theme: Option<&definition::ResolvedThemeDefinition>,
    text: bmux_tui::style::Style,
    muted: bmux_tui::style::Style,
) -> bcode_markdown_render::MarkdownTheme {
    let defaults = bcode_markdown_render::MarkdownTheme::default();
    let role = |name: &str, fallback: bmux_tui::style::Style| {
        theme
            .and_then(|theme| theme.style(name))
            .unwrap_or(fallback)
    };
    bcode_markdown_render::MarkdownTheme {
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
    }
}

/// Resolve an agent accent from explicit metadata, hydration state, and fallback palette.
#[cfg(test)]
#[must_use]
pub fn target_agent_accent(
    agent_id: &str,
    configured_accent: Option<&str>,
    agent_metadata_hydrated: bool,
) -> Color {
    configured_accent
        .and_then(parse_agent_accent_color)
        .unwrap_or_else(|| {
            if agent_metadata_hydrated {
                fallback_agent_accent_color(agent_id)
            } else {
                PENDING_AGENT_METADATA_ACCENT
            }
        })
}

fn parse_agent_accent_color(accent: &str) -> Option<Color> {
    let hex = accent.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let [red, green, blue] = parse_hex_rgb(hex).ok()?;
    Some(Color::Rgb(red, green, blue))
}

fn parse_hex_rgb(hex: &str) -> Result<[u8; 3], ParseIntError> {
    Ok([
        u8::from_str_radix(&hex[0..2], 16)?,
        u8::from_str_radix(&hex[2..4], 16)?,
        u8::from_str_radix(&hex[4..6], 16)?,
    ])
}

fn fallback_agent_accent_color(agent_id: &str) -> Color {
    const PALETTE: [Color; 6] = [
        Color::Cyan,
        Color::Rgb(167, 139, 250),
        Color::Rgb(52, 211, 153),
        Color::Rgb(245, 158, 11),
        Color::Rgb(96, 165, 250),
        Color::Rgb(244, 114, 182),
    ];
    let hash = agent_id.bytes().fold(0_usize, |hash, byte| {
        hash.wrapping_mul(33).wrapping_add(usize::from(byte))
    });
    PALETTE[hash % PALETTE.len()]
}
