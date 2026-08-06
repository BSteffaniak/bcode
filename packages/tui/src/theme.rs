//! Theme resolution and presentation state.

pub mod definition;
pub mod discovery;

use std::num::ParseIntError;

use bcode_config::{TuiAgentAccentPolicy, TuiThemeVariant};
use bmux_tui::style::{Color, Modifier};

use super::app::BmuxApp;

/// One bounded theme catalog row for selection and diagnostics UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeCatalogEntry {
    /// Stable theme id.
    pub id: String,
    /// Human-readable display name.
    pub display_name: String,
    /// Bundled, user, project, or explicit origin label.
    pub source: String,
    /// Whether a dark variant patch is available.
    pub has_dark_variant: bool,
    /// Whether a light variant patch is available.
    pub has_light_variant: bool,
    /// Validation state for this accepted catalog definition.
    pub validation: String,
    /// Whether this is the configured base selection.
    pub selected: bool,
}

/// One bounded effective catalog plus rejected-candidate diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeCatalogView {
    /// Selectable, fully validated definitions.
    pub entries: Vec<ThemeCatalogEntry>,
    /// Bounded secret-safe rejected candidate summaries.
    pub diagnostics: Vec<String>,
}

/// Return the bounded effective theme catalog for an app.
#[must_use]
pub fn catalog_view(app: &BmuxApp) -> ThemeCatalogView {
    let project_root = app
        .working_directory()
        .unwrap_or_else(|| std::path::Path::new("."));
    let roots = discovery::default_theme_roots(
        &bcode_config::default_config_dir(),
        project_root,
        &app.tui_config().theme.paths,
    );
    let Ok(discovered) = discovery::discover_themes(&roots) else {
        return ThemeCatalogView {
            entries: Vec::new(),
            diagnostics: vec!["bundled theme catalog is invalid".to_owned()],
        };
    };
    let entries = discovered
        .catalog
        .definitions()
        .map(|definition| {
            let source = discovered.sources.get(definition.id()).map_or_else(
                || "bundled".to_owned(),
                |external| match external.kind {
                    discovery::ThemeSourceKind::User => "user".to_owned(),
                    discovery::ThemeSourceKind::Project => "project".to_owned(),
                    discovery::ThemeSourceKind::Explicit => "explicit".to_owned(),
                },
            );
            ThemeCatalogEntry {
                id: definition.id().to_owned(),
                display_name: definition.display_name().to_owned(),
                source,
                has_dark_variant: definition.has_dark_variant(),
                has_light_variant: definition.has_light_variant(),
                validation: "valid".to_owned(),
                selected: definition.id() == app.tui_config().theme.name,
            }
        })
        .collect();
    let diagnostics = discovered
        .diagnostics
        .iter()
        .map(|diagnostic| {
            format!(
                "{}: {}",
                bcode_plugin_sdk::path::display_from_current_dir(&diagnostic.path),
                diagnostic.message
            )
        })
        .collect();
    ThemeCatalogView {
        entries,
        diagnostics,
    }
}

/// Return the stable metadata signature of the active theme input roots.
#[must_use]
pub fn active_theme_input_signature(app: &BmuxApp) -> u64 {
    let project_root = app
        .working_directory()
        .unwrap_or_else(|| std::path::Path::new("."));
    let roots = discovery::default_theme_roots(
        &bcode_config::default_config_dir(),
        project_root,
        &app.tui_config().theme.paths,
    );
    discovery::theme_input_signature(&roots)
}

/// One resolved renderer-owned container recipe and its presentation style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerPresentation {
    /// Bounded layout recipe.
    pub recipe: definition::ContainerRecipe,
    /// Container background/border style.
    pub style: bmux_tui::style::Style,
}

/// Resolved bounded transcript/container recipes used only by the TUI renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptContainerTheme {
    /// User message recipe.
    pub user: ContainerPresentation,
    /// Assistant message recipe.
    pub assistant: ContainerPresentation,
    /// Requested tool recipe.
    pub tool_requested: ContainerPresentation,
    /// Running tool recipe.
    pub tool_running: ContainerPresentation,
    /// Waiting tool recipe.
    pub tool_waiting: ContainerPresentation,
    /// Successful tool recipe.
    pub tool_succeeded: ContainerPresentation,
    /// Failed tool recipe.
    pub tool_failed: ContainerPresentation,
    /// Cancelled tool recipe.
    pub tool_cancelled: ContainerPresentation,
    /// Timed-out tool recipe.
    pub tool_timed_out: ContainerPresentation,
}

/// Resolved semantic transcript and generic tool content styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptStyleTheme {
    /// User message label.
    pub user_label: bmux_tui::style::Style,
    /// Assistant message label.
    pub assistant_label: bmux_tui::style::Style,
    /// Reasoning label.
    pub reasoning_label: bmux_tui::style::Style,
    /// System message label.
    pub system_label: bmux_tui::style::Style,
    /// Metadata text.
    pub meta: bmux_tui::style::Style,
    /// Skill label.
    pub skill_label: bmux_tui::style::Style,
    /// Skill error label.
    pub skill_error_label: bmux_tui::style::Style,
    /// Pending-submission label.
    pub pending_label: bmux_tui::style::Style,
    /// Stream-integrity label.
    pub stream_status_label: bmux_tui::style::Style,
    /// Generic detail label.
    pub detail_label: bmux_tui::style::Style,
    /// Generic detail body.
    pub detail_body: bmux_tui::style::Style,
    /// Tool title for requested state.
    pub tool_requested_title: bmux_tui::style::Style,
    /// Tool title for running state.
    pub tool_running_title: bmux_tui::style::Style,
    /// Tool title for waiting state.
    pub tool_waiting_title: bmux_tui::style::Style,
    /// Tool title for successful state.
    pub tool_succeeded_title: bmux_tui::style::Style,
    /// Tool title for failed state.
    pub tool_failed_title: bmux_tui::style::Style,
    /// Tool title for cancelled state.
    pub tool_cancelled_title: bmux_tui::style::Style,
    /// Tool title for timed-out state.
    pub tool_timed_out_title: bmux_tui::style::Style,
    /// Tool metadata such as timing.
    pub tool_metadata: bmux_tui::style::Style,
    /// Tool field label.
    pub tool_label: bmux_tui::style::Style,
    /// Tool argument value.
    pub tool_argument: bmux_tui::style::Style,
    /// Tool output body.
    pub tool_output: bmux_tui::style::Style,
    /// Tool truncation notice.
    pub tool_truncation: bmux_tui::style::Style,
}

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
    /// Informational state style.
    pub info: bmux_tui::style::Style,
    /// Successful state style.
    pub success: bmux_tui::style::Style,
    /// Warning state style.
    pub warning: bmux_tui::style::Style,
    /// Error state style.
    pub error: bmux_tui::style::Style,
    /// Markdown styles derived from semantic theme roles.
    pub markdown: bcode_markdown_render::MarkdownTheme,
    /// Semantic syntax palette for code presentation.
    pub syntax: bcode_syntax_render::SyntaxPalette,
    /// Semantic source-card presentation.
    pub source: bcode_tui_components::source_viewer::SourceViewerStyle,
    /// Semantic diff presentation.
    pub diff: bcode_tui_components::diff_viewer::DiffViewerStyle,
    /// Resolved bounded transcript/container recipes.
    pub containers: TranscriptContainerTheme,
    /// Resolved semantic transcript and generic tool content styles.
    pub transcript: TranscriptStyleTheme,
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
            info: self.info,
            success: self.success,
            warning: self.warning,
            error: self.error,
            markdown: self.markdown,
            syntax: self.syntax,
            source: self.source,
            diff: self.diff,
            containers: self.containers,
            transcript: self.transcript,
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
    /// Informational state style.
    pub info: bmux_tui::style::Style,
    /// Successful state style.
    pub success: bmux_tui::style::Style,
    /// Warning state style.
    pub warning: bmux_tui::style::Style,
    /// Error state style.
    pub error: bmux_tui::style::Style,
    /// Markdown styles derived from semantic theme roles.
    pub markdown: bcode_markdown_render::MarkdownTheme,
    /// Semantic syntax palette for code presentation.
    pub syntax: bcode_syntax_render::SyntaxPalette,
    /// Semantic source-card presentation.
    pub source: bcode_tui_components::source_viewer::SourceViewerStyle,
    /// Semantic diff presentation.
    pub diff: bcode_tui_components::diff_viewer::DiffViewerStyle,
    /// Resolved bounded transcript/container recipes.
    pub containers: TranscriptContainerTheme,
    /// Resolved semantic transcript and generic tool content styles.
    pub transcript: TranscriptStyleTheme,
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
    resolved_definition_theme(resolved.as_ref(), PENDING_AGENT_METADATA_ACCENT)
}

/// Resolve a configured theme for standalone TUI surfaces without app state.
#[must_use]
pub fn resolve_configured_theme(
    config: &bcode_config::TuiConfig,
    project_root: &std::path::Path,
) -> PresentedTheme {
    let resolved = resolve_definition(
        &config.theme.name,
        &config.theme.overlays,
        config.theme.variant,
        project_root,
        &config.theme.paths,
    );
    let accent = resolved_accent(resolved.as_ref());
    resolved_definition_theme(resolved.as_ref(), accent).presented(accent)
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
    let accent = resolve_app_accent(app, configured_theme_accent);
    resolved_definition_theme(resolved.as_ref(), accent)
}

pub(crate) fn resolve_app_accent(app: &BmuxApp, configured_theme_accent: Color) -> Color {
    match app.tui_config().theme.agent_accent {
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
    }
}

pub(crate) fn resolve_theme_selection(
    app: &BmuxApp,
    theme_name: &str,
) -> Option<definition::ResolvedThemeDefinition> {
    resolve_definition(
        theme_name,
        &app.tui_config().theme.overlays,
        app.tui_config().theme.variant,
        app.working_directory()
            .unwrap_or_else(|| std::path::Path::new(".")),
        &app.tui_config().theme.paths,
    )
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
    let variant = resolve_variant(
        variant,
        bmux_tui::capabilities::TerminalCapabilities::detect(),
    );
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

const fn resolve_variant(
    variant: TuiThemeVariant,
    capabilities: bmux_tui::capabilities::TerminalCapabilities,
) -> definition::ResolvedThemeVariant {
    match variant {
        TuiThemeVariant::Dark => definition::ResolvedThemeVariant::Dark,
        TuiThemeVariant::Light => definition::ResolvedThemeVariant::Light,
        TuiThemeVariant::Auto => match capabilities.background {
            bmux_tui::capabilities::TerminalBackground::Light => {
                definition::ResolvedThemeVariant::Light
            }
            bmux_tui::capabilities::TerminalBackground::Dark
            | bmux_tui::capabilities::TerminalBackground::Unknown => {
                definition::ResolvedThemeVariant::Dark
            }
        },
    }
}

pub(crate) fn resolved_accent(theme: Option<&definition::ResolvedThemeDefinition>) -> Color {
    theme
        .and_then(|theme| theme.style("border.focused").and_then(|style| style.fg))
        .or_else(|| theme.and_then(|theme| theme.color("accent")))
        .unwrap_or(PENDING_AGENT_METADATA_ACCENT)
}

#[allow(clippy::too_many_lines)] // Resolves one immutable presentation aggregate from semantic roles.
pub(crate) fn resolved_definition_theme(
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
    let info = style("state.info").unwrap_or_else(|| bmux_tui::style::Style::new().fg(accent));
    let success = style("state.success").unwrap_or(text);
    let warning = style("state.warning").unwrap_or(text);
    let error = style("state.error").unwrap_or(text);
    let markdown = markdown_theme(theme, text, muted);
    let syntax = syntax_palette(theme);
    let source = bcode_tui_components::source_viewer::SourceViewerStyle {
        source: style("source.text").unwrap_or(text),
        border: style("source.border").unwrap_or(border),
        gutter: style("source.gutter").unwrap_or(muted),
        truncated: style("source.truncated").unwrap_or(muted),
    };
    let default_diff = bcode_tui_components::diff_viewer::DiffViewerStyle::default();
    let diff = bcode_tui_components::diff_viewer::DiffViewerStyle {
        text: style("diff.text").unwrap_or(text),
        muted: style("diff.muted").unwrap_or(muted),
        title: style("diff.title").unwrap_or(focused),
        label: style("diff.label").unwrap_or_else(|| text.add_modifier(Modifier::BOLD)),
        added: style("diff.added").unwrap_or(default_diff.added),
        removed: style("diff.removed").unwrap_or(default_diff.removed),
        hunk: style("diff.hunk").unwrap_or(focused),
        added_row: style("diff.added_row").unwrap_or(default_diff.added_row),
        removed_row: style("diff.removed_row").unwrap_or(default_diff.removed_row),
        added_emphasis: style("diff.added_emphasis").unwrap_or(default_diff.added_emphasis),
        removed_emphasis: style("diff.removed_emphasis").unwrap_or(default_diff.removed_emphasis),
    };
    let default_container = definition::ContainerRecipe {
        layout: definition::ContainerLayout::Plain,
        width: definition::ContainerWidth::Content,
        border: definition::ContainerBorder::None,
        padding_x: 0,
        padding_y: 0,
    };
    let container = |role: &str| {
        theme
            .and_then(|theme| theme.containers.get(role).copied())
            .unwrap_or(default_container)
    };
    let presentation =
        |role: &str, style_role: &str, fallback: bmux_tui::style::Style| ContainerPresentation {
            recipe: container(role),
            style: style(style_role).unwrap_or(fallback),
        };
    let containers = TranscriptContainerTheme {
        user: presentation("transcript.user", "transcript.user.container", text),
        assistant: presentation(
            "transcript.assistant",
            "transcript.assistant.container",
            text,
        ),
        tool_requested: presentation("tool.requested", "tool.requested.container", muted),
        tool_running: presentation("tool.running", "tool.running.container", info),
        tool_waiting: presentation("tool.waiting", "tool.waiting.container", warning),
        tool_succeeded: presentation("tool.succeeded", "tool.succeeded.container", success),
        tool_failed: presentation("tool.failed", "tool.failed.container", error),
        tool_cancelled: presentation("tool.cancelled", "tool.cancelled.container", muted),
        tool_timed_out: presentation("tool.timed_out", "tool.timed_out.container", warning),
    };
    let transcript = TranscriptStyleTheme {
        user_label: style("transcript.user.label").unwrap_or(focused),
        assistant_label: style("transcript.assistant.label").unwrap_or(success),
        reasoning_label: style("transcript.reasoning.label").unwrap_or(muted),
        system_label: style("transcript.system.label").unwrap_or(muted),
        meta: style("transcript.meta").unwrap_or(muted),
        skill_label: style("transcript.skill.label").unwrap_or(focused),
        skill_error_label: style("transcript.skill_error.label").unwrap_or(error),
        pending_label: style("transcript.pending.label").unwrap_or(focused),
        stream_status_label: style("transcript.stream_status.label").unwrap_or(warning),
        detail_label: style("transcript.detail.label").unwrap_or(muted),
        detail_body: style("transcript.detail.body").unwrap_or(muted),
        tool_requested_title: style("tool.requested.title").unwrap_or(muted),
        tool_running_title: style("tool.running.title").unwrap_or(info),
        tool_waiting_title: style("tool.waiting.title").unwrap_or(warning),
        tool_succeeded_title: style("tool.succeeded.title").unwrap_or(success),
        tool_failed_title: style("tool.failed.title").unwrap_or(error),
        tool_cancelled_title: style("tool.cancelled.title").unwrap_or(muted),
        tool_timed_out_title: style("tool.timed_out.title").unwrap_or(warning),
        tool_metadata: style("tool.metadata").unwrap_or(muted),
        tool_label: style("tool.label").unwrap_or_else(|| muted.add_modifier(Modifier::BOLD)),
        tool_argument: style("tool.argument").unwrap_or(text),
        tool_output: style("tool.output").unwrap_or(text),
        tool_truncation: style("tool.truncation").unwrap_or(muted),
    };
    ResolvedTheme {
        accent,
        text,
        muted,
        border,
        focused,
        background,
        selection,
        info,
        success,
        warning,
        error,
        markdown,
        syntax,
        source,
        diff,
        containers,
        transcript,
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
            .map_or(fallback, tui_color_to_syntax)
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

const fn tui_color_to_syntax(color: Color) -> bcode_syntax_render::SyntaxColor {
    bcode_syntax_render::SyntaxColor::from_tui(color)
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

#[cfg(test)]
mod capability_tests {
    use super::*;

    #[test]
    fn auto_variant_uses_detected_background_with_dark_unknown_fallback() {
        use bmux_tui::capabilities::{TerminalBackground, TerminalCapabilities};

        for (background, expected) in [
            (
                TerminalBackground::Light,
                definition::ResolvedThemeVariant::Light,
            ),
            (
                TerminalBackground::Dark,
                definition::ResolvedThemeVariant::Dark,
            ),
            (
                TerminalBackground::Unknown,
                definition::ResolvedThemeVariant::Dark,
            ),
        ] {
            assert_eq!(
                resolve_variant(
                    TuiThemeVariant::Auto,
                    TerminalCapabilities {
                        background,
                        ..TerminalCapabilities::default()
                    }
                ),
                expected
            );
        }
    }

    #[test]
    fn explicit_variant_ignores_detected_background() {
        use bmux_tui::capabilities::{TerminalBackground, TerminalCapabilities};

        assert_eq!(
            resolve_variant(
                TuiThemeVariant::Dark,
                TerminalCapabilities {
                    background: TerminalBackground::Light,
                    ..TerminalCapabilities::default()
                }
            ),
            definition::ResolvedThemeVariant::Dark
        );
        assert_eq!(
            resolve_variant(
                TuiThemeVariant::Light,
                TerminalCapabilities {
                    background: TerminalBackground::Dark,
                    ..TerminalCapabilities::default()
                }
            ),
            definition::ResolvedThemeVariant::Light
        );
    }

    #[test]
    fn terminal_native_diff_fallbacks_preserve_changed_row_backgrounds() {
        let catalog = definition::ThemeCatalog::bundled().expect("bundled themes parse");
        for theme_id in ["terminal-native", "terminal-native-structured"] {
            let resolved = catalog
                .resolve(&definition::ThemeSelection::new(theme_id))
                .unwrap_or_else(|error| panic!("{theme_id} resolves: {error}"));
            let presented =
                resolved_definition_theme(Some(&resolved), PENDING_AGENT_METADATA_ACCENT)
                    .presented(PENDING_AGENT_METADATA_ACCENT);

            assert!(presented.diff.added_row.bg.is_some(), "{theme_id}");
            assert!(presented.diff.removed_row.bg.is_some(), "{theme_id}");
            assert!(presented.diff.added_emphasis.bg.is_some(), "{theme_id}");
            assert!(presented.diff.removed_emphasis.bg.is_some(), "{theme_id}");
            assert!(
                !presented
                    .diff
                    .added_emphasis
                    .modifiers
                    .contains(Modifier::UNDERLINE),
                "{theme_id}"
            );
            assert!(
                !presented
                    .diff
                    .removed_emphasis
                    .modifiers
                    .contains(Modifier::UNDERLINE),
                "{theme_id}"
            );
        }
    }

    #[test]
    fn missing_and_reduced_color_capabilities_keep_terminal_native_usable() {
        use bmux_tui::capabilities::{
            TerminalBackground, TerminalCapabilities, TerminalColorDepth,
        };

        let missing = TerminalCapabilities::detect_with(|_| None);
        assert_eq!(missing.background, TerminalBackground::Unknown);
        assert_eq!(missing.color_depth, TerminalColorDepth::Ansi16);
        assert_eq!(
            resolve_variant(TuiThemeVariant::Auto, missing),
            definition::ResolvedThemeVariant::Dark
        );

        let reduced = TerminalCapabilities::detect_with(|name| match name {
            "NO_COLOR" => Some(String::new()),
            "COLORFGBG" => Some("15;0".to_owned()),
            _ => None,
        });
        assert_eq!(reduced.background, TerminalBackground::Dark);
        assert_eq!(reduced.color_depth, TerminalColorDepth::Monochrome);

        let catalog = definition::ThemeCatalog::bundled().expect("bundled themes parse");
        let native = catalog
            .resolve(
                &definition::ThemeSelection::new("terminal-native")
                    .variant(resolve_variant(TuiThemeVariant::Auto, reduced)),
            )
            .expect("terminal-native resolves with reduced-color capabilities");
        let presented = resolved_definition_theme(Some(&native), PENDING_AGENT_METADATA_ACCENT)
            .presented(PENDING_AGENT_METADATA_ACCENT);
        assert!(
            presented
                .text
                .fg
                .is_none_or(|color| color == Color::Default)
        );
        assert!(
            presented
                .background
                .bg
                .is_none_or(|color| color == Color::Default)
        );
    }
}
