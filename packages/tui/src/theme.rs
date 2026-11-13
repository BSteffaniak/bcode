//! Theme resolution and presentation state.

pub use bcode_tui_theme::{definition, discovery, roles};

use std::num::ParseIntError;

use bcode_config::{TuiAgentAccentPolicy, TuiThemeVariant};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_components::theme::{ComponentChrome, ComponentSurfaces, ComponentTheme};

use super::app::BmuxApp;

pub mod chrome;
pub mod effects;

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

/// Discover the bounded effective theme catalog for an app.
pub(crate) fn discover_theme_catalog(
    app: &BmuxApp,
) -> Result<discovery::DiscoveredThemes, definition::ThemeError> {
    let project_root = app
        .working_directory()
        .unwrap_or_else(|| std::path::Path::new("."));
    let roots = discovery::default_theme_roots(
        &bcode_config::default_config_dir(),
        project_root,
        &app.tui_config().theme.paths,
    );
    discovery::discover_themes(&roots)
}

/// Return the bounded effective theme catalog for an app.
#[must_use]
pub fn catalog_view(app: &mut BmuxApp) -> ThemeCatalogView {
    let selected = app.tui_config().theme.name.clone();
    let Some(discovered) = app.theme_catalog() else {
        return ThemeCatalogView {
            entries: Vec::new(),
            diagnostics: vec!["bundled theme catalog is invalid".to_owned()],
        };
    };
    let entries = discovered
        .catalog
        .definitions()
        .filter(|definition| definition.kind() == definition::ThemeKind::Theme)
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
                selected: definition.id() == selected,
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

/// One selectable interface profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileCatalogEntry {
    /// Stable profile id.
    pub id: String,
    /// Display name without the `(profile)` suffix.
    pub display_name: String,
    /// Optional one-line description.
    pub description: Option<String>,
    /// Whether this is the configured profile.
    pub selected: bool,
}

/// Return the selectable profiles in stable id order.
#[must_use]
pub fn profile_catalog(app: &mut BmuxApp) -> Vec<ProfileCatalogEntry> {
    let selected = app.tui_config().theme.profile.clone();
    app.theme_catalog().map_or_else(Vec::new, |discovered| {
        discovered
            .catalog
            .definitions()
            .filter(|definition| definition.kind() == definition::ThemeKind::Profile)
            .map(|definition| ProfileCatalogEntry {
                id: definition.id().to_owned(),
                display_name: definition
                    .display_name()
                    .trim_end_matches(" (profile)")
                    .to_owned(),
                description: definition.description().map(str::to_owned),
                selected: selected.as_deref() == Some(definition.id()),
            })
            .collect()
    })
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

impl ContainerPresentation {
    /// Adapt this resolved presentation into the reusable Bcode transcript recipe.
    #[must_use]
    pub const fn transcript_style(
        self,
    ) -> bcode_tui_components::transcript::TranscriptContainerStyle {
        bcode_tui_components::transcript::TranscriptContainerStyle {
            recipe: transcript_recipe(self.recipe),
            style: self.style,
            border_set: bmux_tui::chrome::BorderSet::SINGLE,
        }
    }
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

/// Resolved presentation for existing raised, overlay, and focused-control surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceTheme {
    /// Raised panels, lists, palettes, and the composer.
    pub raised: bmux_tui::style::Style,
    /// Opaque modal and dialog surface.
    pub overlay: bmux_tui::style::Style,
    /// Focused input and control presentation.
    pub control_focused: bmux_tui::style::Style,
    /// Optional full-frame modal scrim.
    pub scrim: Option<bmux_tui::style::Style>,
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
    /// Canvas style applied to the complete terminal frame.
    pub canvas: bmux_tui::style::Style,
    /// Raised, overlay, and focused-control surface presentation.
    pub surfaces: SurfaceTheme,
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
    /// Border glyphs and focus effects for dialogs and modals.
    pub dialog_chrome: ComponentChrome,
    /// Icons, disclosure markers, density, and tool borders for transcript units.
    pub unit_chrome: chrome::UnitChrome,
    /// Status separators, surface layering, and motion.
    pub effects: effects::ChromeEffects,
    /// Complete generic component theme, including v2 roles and chrome.
    pub component: ComponentTheme,
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
            canvas: self.canvas,
            surfaces: self.surfaces,
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
            dialog_chrome: self.dialog_chrome,
            unit_chrome: self.unit_chrome,
            effects: self.effects,
            component: self.component,
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
    /// Canvas style applied to the complete terminal frame.
    pub canvas: bmux_tui::style::Style,
    /// Raised, overlay, and focused-control surface presentation.
    pub surfaces: SurfaceTheme,
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
    /// Border glyphs and focus effects for dialogs and modals.
    pub dialog_chrome: ComponentChrome,
    /// Icons, disclosure markers, density, and tool borders for transcript units.
    pub unit_chrome: chrome::UnitChrome,
    /// Status separators, surface layering, and motion.
    pub effects: effects::ChromeEffects,
    /// Complete generic component theme, including v2 roles and chrome.
    pub component: ComponentTheme,
    /// Stable resolved presentation fingerprint.
    pub fingerprint: u64,
}

impl PresentedTheme {
    /// Convert Bcode's resolved presentation into the canonical generic component theme.
    ///
    /// Core roles come from this presentation's fields, so adjustments made to
    /// them (for example `NO_COLOR` handling) always reach components; roles
    /// that exist only in the component theme come from the resolved theme.
    #[must_use]
    pub const fn component_theme(&self) -> ComponentTheme {
        ComponentTheme {
            canvas: self.canvas,
            surfaces: ComponentSurfaces {
                normal: self.canvas,
                raised: self.surfaces.raised,
                overlay: self.surfaces.overlay,
                scrim: self.surfaces.scrim,
            },
            text: self.text,
            focused: self.focused,
            selected: self.selection,
            disabled: self.muted.add_modifier(Modifier::DIM),
            muted: self.muted,
            info: self.info,
            success: self.success,
            warning: self.warning,
            error: self.error,
            border: self.border,
            ..self.component
        }
    }
}

impl From<PresentedTheme> for ComponentTheme {
    fn from(theme: PresentedTheme) -> Self {
        theme.component_theme()
    }
}

impl From<&PresentedTheme> for ComponentTheme {
    fn from(theme: &PresentedTheme) -> Self {
        theme.component_theme()
    }
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
    resolve_configured_theme_with_definition(config, project_root).0
}

/// Resolve a configured theme together with the declarative definition it was presented from.
///
/// The definition is `None` when no configured or fallback theme resolves;
/// the presented theme then uses the same built-in defaults as
/// [`resolve_configured_theme`].
#[must_use]
pub fn resolve_configured_theme_with_definition(
    config: &bcode_config::TuiConfig,
    project_root: &std::path::Path,
) -> (PresentedTheme, Option<definition::ResolvedThemeDefinition>) {
    let resolved = resolve_definition(&theme_preferences(&config.theme), project_root);
    let accent = resolved_accent(resolved.as_ref());
    let presented = resolved_definition_theme(resolved.as_ref(), accent).presented(accent);
    (presented, resolved)
}

/// Resolve the target theme from app state.
#[must_use]
pub fn resolve_theme(app: &BmuxApp) -> ResolvedTheme {
    resolve_theme_with_structure(app).0
}

/// Resolve the target theme and its structural presentation rule layers.
#[must_use]
pub fn resolve_theme_with_structure(
    app: &BmuxApp,
) -> (
    ResolvedTheme,
    Vec<bcode_presentation_policy::models::RuleLayer>,
) {
    let project_root = app
        .working_directory()
        .map(std::path::Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let resolved = resolve_definition(&theme_preferences(&app.tui_config().theme), &project_root);
    let configured_theme_accent = resolved_accent(resolved.as_ref());
    let accent = resolve_app_accent(app, configured_theme_accent);
    let layers = structure_layers_for(resolved.as_ref());
    (resolved_definition_theme(resolved.as_ref(), accent), layers)
}

/// Structural rule layers a resolved theme contributes: the chrome density as
/// a host-default layer, then the theme and profile `[presentation]` layers.
#[must_use]
pub fn structure_layers_for(
    resolved: Option<&definition::ResolvedThemeDefinition>,
) -> Vec<bcode_presentation_policy::models::RuleLayer> {
    let mut layers = Vec::new();
    layers.extend(chrome::UnitChrome::from_theme(resolved).density_layer());
    if let Some(resolved) = resolved {
        layers.extend(resolved.presentation.iter().cloned());
    }
    layers
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

fn resolve_definition(
    preferences: &bcode_tui_theme::appearance::ThemePreferences,
    project_root: &std::path::Path,
) -> Option<definition::ResolvedThemeDefinition> {
    let roots = discovery::default_theme_roots(
        &bcode_config::default_config_dir(),
        project_root,
        &preferences.paths,
    );
    let variant = preferences
        .variant
        .resolve(terminal_capabilities().background);
    let resolution =
        bcode_tui_theme::selection::resolve_discovered(&roots, &preferences.selection(variant));
    for diagnostic in &resolution.diagnostics {
        tracing::warn!(
            path = %diagnostic.path.display(),
            message = %diagnostic.message,
            "theme definition skipped"
        );
    }
    tracing::debug!(
        external_theme_count = resolution.external_sources,
        "theme catalog discovered"
    );
    resolution.resolved.ok()
}

/// The shared theme selection for configured `[tui.theme]` settings.
#[must_use]
pub fn theme_preferences(
    theme: &bcode_config::TuiThemeConfig,
) -> bcode_tui_theme::appearance::ThemePreferences {
    bcode_tui_theme::appearance::ThemePreferences {
        name: theme.name.clone(),
        profile: theme.profile.clone(),
        overlays: theme.overlays.clone(),
        variant: variant_preference(theme.variant),
        paths: theme.paths.clone(),
    }
}

const fn variant_preference(
    variant: TuiThemeVariant,
) -> bcode_tui_theme::appearance::VariantPreference {
    use bcode_tui_theme::appearance::VariantPreference;
    match variant {
        TuiThemeVariant::Auto => VariantPreference::Auto,
        TuiThemeVariant::Dark => VariantPreference::Dark,
        TuiThemeVariant::Light => VariantPreference::Light,
    }
}

/// Process-wide terminal color replies; a Bcode process drives one terminal.
#[must_use]
pub fn terminal_colors() -> bmux_tui_runtime::TerminalColorsHandle {
    static COLORS: std::sync::LazyLock<bmux_tui_runtime::TerminalColorsHandle> =
        std::sync::LazyLock::new(bmux_tui_runtime::TerminalColorsHandle::default);
    COLORS.clone()
}

/// BMUX color depth for a configured policy; `auto` follows terminal detection.
#[must_use]
pub fn terminal_color_depth(
    configured: bcode_config::TuiColorDepth,
) -> bmux_tui::capabilities::TerminalColorDepth {
    use bcode_config::TuiColorDepth;
    use bmux_tui::capabilities::{TerminalCapabilities, TerminalColorDepth};
    match configured {
        // Only NO_COLOR/dumb terminals are honored automatically: COLORTERM
        // is often dropped by multiplexers and SSH on truecolor-capable
        // terminals, so reduced depths are explicit opt-ins.
        TuiColorDepth::Auto => match TerminalCapabilities::detect().color_depth {
            TerminalColorDepth::Monochrome => TerminalColorDepth::Monochrome,
            TerminalColorDepth::Ansi16
            | TerminalColorDepth::Ansi256
            | TerminalColorDepth::TrueColor => TerminalColorDepth::TrueColor,
        },
        TuiColorDepth::Truecolor => TerminalColorDepth::TrueColor,
        TuiColorDepth::Ansi256 => TerminalColorDepth::Ansi256,
        TuiColorDepth::Ansi16 => TerminalColorDepth::Ansi16,
        TuiColorDepth::None => TerminalColorDepth::Monochrome,
    }
}

/// Resolve a configured light/dark variant, detecting the terminal for `auto`.
#[must_use]
pub fn resolve_variant_for_config(variant: TuiThemeVariant) -> definition::ResolvedThemeVariant {
    variant_preference(variant).resolve(terminal_capabilities().background)
}

/// Environment capabilities, with the background replaced by the terminal's
/// reported background color when it has answered the color query.
fn terminal_capabilities() -> bmux_tui::capabilities::TerminalCapabilities {
    let mut capabilities = bmux_tui::capabilities::TerminalCapabilities::detect();
    let reported = terminal_colors().get().background_appearance();
    if reported != bmux_tui::capabilities::TerminalBackground::Unknown {
        capabilities.background = reported;
    }
    capabilities
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
    let shared = bcode_tui_theme::components::component_styles(theme, accent);
    let component_theme = shared.component;
    let text = component_theme.text;
    let muted = component_theme.muted;
    let border = component_theme.border;
    let focused = component_theme.focused;
    let canvas = component_theme.canvas;
    let surfaces = SurfaceTheme {
        raised: component_theme.surfaces.raised,
        overlay: component_theme.surfaces.overlay,
        control_focused: shared.control_focused,
        scrim: component_theme.surfaces.scrim,
    };
    let selection = component_theme.selected;
    let info = component_theme.info;
    let success = component_theme.success;
    let warning = component_theme.warning;
    let error = component_theme.error;
    let markdown = shared.markdown;
    let syntax = shared.syntax;
    let source = shared.source;
    let diff = shared.diff;
    let default_container = definition::ContainerRecipe {
        layout: definition::ContainerLayout::Plain,
        width: definition::ContainerWidth::Content,
        border: definition::ContainerBorder::None,
        padding_x: 0,
        padding_y: 0,
    };
    let unit_chrome = chrome::UnitChrome::from_theme(theme);
    let container = |role: &str| {
        let recipe = theme
            .and_then(|theme| theme.containers.get(role).copied())
            .unwrap_or(default_container);
        if role.starts_with("tool.") {
            unit_chrome.tool_container(recipe)
        } else {
            recipe
        }
    };
    let effects = effects::ChromeEffects::from_theme(theme);
    let raised = effects.raised_background(surfaces.raised, canvas);
    let presentation = |role: &str, style_role: &str, fallback: bmux_tui::style::Style| {
        let recipe = container(role);
        let style = style(style_role).unwrap_or(fallback);
        let (recipe, style) = if role.starts_with("tool.") {
            effects::ChromeEffects::tool_surface(recipe, style, raised)
        } else {
            (recipe, style)
        };
        ContainerPresentation { recipe, style }
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
        canvas,
        surfaces,
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
        dialog_chrome: shared.dialog_chrome,
        unit_chrome,
        effects,
        component: component_theme,
        fingerprint: theme.map_or(0, |theme| theme.fingerprint),
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

/// Transcript component recipe for a theme container recipe.
const fn transcript_recipe(
    recipe: definition::ContainerRecipe,
) -> bcode_tui_components::transcript::TranscriptContainerRecipe {
    use definition::{ContainerBorder, ContainerLayout, ContainerWidth};
    {
        bcode_tui_components::transcript::TranscriptContainerRecipe {
            layout: match recipe.layout {
                ContainerLayout::Plain => {
                    bcode_tui_components::transcript::TranscriptContainerLayout::Plain
                }
                ContainerLayout::LeftBar => {
                    bcode_tui_components::transcript::TranscriptContainerLayout::LeftBar
                }
                ContainerLayout::Panel => {
                    bcode_tui_components::transcript::TranscriptContainerLayout::Panel
                }
            },
            width: match recipe.width {
                ContainerWidth::Content => {
                    bcode_tui_components::transcript::TranscriptContainerWidth::Content
                }
                ContainerWidth::Full => {
                    bcode_tui_components::transcript::TranscriptContainerWidth::Full
                }
            },
            border: match recipe.border {
                ContainerBorder::None => {
                    bcode_tui_components::transcript::TranscriptContainerBorder::None
                }
                ContainerBorder::Left => {
                    bcode_tui_components::transcript::TranscriptContainerBorder::Left
                }
                ContainerBorder::All => {
                    bcode_tui_components::transcript::TranscriptContainerBorder::All
                }
            },
            padding_x: recipe.padding_x,
            padding_y: recipe.padding_y,
        }
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;

    fn resolve_variant(
        variant: TuiThemeVariant,
        capabilities: bmux_tui::capabilities::TerminalCapabilities,
    ) -> definition::ResolvedThemeVariant {
        variant_preference(variant).resolve(capabilities.background)
    }

    #[test]
    fn presented_theme_has_one_canonical_component_conversion() {
        let presented = resolve_initial_theme().presented(PENDING_AGENT_METADATA_ACCENT);
        let components = presented.component_theme();

        assert_eq!(components.canvas, presented.canvas);
        assert_eq!(components.surfaces.normal, presented.canvas);
        assert_eq!(components.surfaces.raised, presented.surfaces.raised);
        assert_eq!(components.surfaces.overlay, presented.surfaces.overlay);
        assert_eq!(components.surfaces.scrim, presented.surfaces.scrim);
        assert_eq!(components.text, presented.text);
        assert_eq!(components.selected, presented.selection);
        assert_eq!(components.border, presented.border);
    }

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
    fn explicit_rgb_bundled_themes_meet_text_contrast_thresholds() {
        const MIN_PRIMARY: f64 = 4.5;
        const MIN_MUTED: f64 = 3.0;
        let catalog = definition::ThemeCatalog::bundled().expect("bundled themes parse");
        for (theme_id, variant) in [
            ("bcode", definition::ResolvedThemeVariant::Dark),
            ("bcode", definition::ResolvedThemeVariant::Light),
            ("bcode-dark", definition::ResolvedThemeVariant::Unspecified),
            ("bcode-light", definition::ResolvedThemeVariant::Unspecified),
            ("nord", definition::ResolvedThemeVariant::Unspecified),
        ] {
            let resolved = catalog
                .resolve(&definition::ThemeSelection::new(theme_id).variant(variant))
                .unwrap_or_else(|error| panic!("{theme_id} resolves: {error}"));
            for (role, background_role, minimum) in [
                ("text.primary", "canvas", MIN_PRIMARY),
                ("text.muted", "canvas", MIN_MUTED),
                ("text.primary", "surface.raised", MIN_PRIMARY),
                ("text.muted", "surface.raised", MIN_MUTED),
                ("text.primary", "surface.overlay", MIN_PRIMARY),
                ("text.muted", "surface.overlay", MIN_MUTED),
                ("selection.active", "selection.active", MIN_PRIMARY),
            ] {
                let foreground = resolved.style(role).and_then(|style| style.fg);
                let background = resolved.style(background_role).and_then(|style| style.bg);
                let (
                    Some(Color::Rgb(red, green, blue)),
                    Some(Color::Rgb(bg_red, bg_green, bg_blue)),
                ) = (foreground, background)
                else {
                    continue;
                };
                let ratio = contrast_ratio([red, green, blue], [bg_red, bg_green, bg_blue]);
                assert!(
                    ratio >= minimum,
                    "{theme_id} {variant:?} {role} on {background_role} contrast {ratio:.2} is below {minimum:.1}"
                );
            }
            for (semantic_role, background_role) in [
                ("control.focused", "surface.raised"),
                ("control.focused", "surface.overlay"),
            ] {
                assert_explicit_contrast(
                    theme_id,
                    variant,
                    semantic_role,
                    background_role,
                    resolved.style(semantic_role).and_then(|style| style.fg),
                    resolved.style(background_role).and_then(|style| style.bg),
                    MIN_MUTED,
                );
            }
            for (semantic_role, background_name) in [
                ("state.success", "success_background"),
                ("state.warning", "warning_background"),
                ("state.error", "error_background"),
                ("tool.succeeded.title", "success_background"),
                ("tool.waiting.title", "warning_background"),
                ("tool.failed.title", "error_background"),
            ] {
                assert_explicit_contrast(
                    theme_id,
                    variant,
                    semantic_role,
                    background_name,
                    resolved.style(semantic_role).and_then(|style| style.fg),
                    resolved.color(background_name),
                    MIN_MUTED,
                );
            }
        }
    }

    fn assert_explicit_contrast(
        theme_id: &str,
        variant: definition::ResolvedThemeVariant,
        semantic_role: &str,
        background_role: &str,
        foreground: Option<Color>,
        background: Option<Color>,
        minimum: f64,
    ) {
        let (Some(Color::Rgb(red, green, blue)), Some(Color::Rgb(bg_red, bg_green, bg_blue))) =
            (foreground, background)
        else {
            return;
        };
        let ratio = contrast_ratio([red, green, blue], [bg_red, bg_green, bg_blue]);
        assert!(
            ratio >= minimum,
            "{theme_id} {variant:?} {semantic_role} on {background_role} contrast {ratio:.2} is below {minimum:.1}"
        );
    }

    fn contrast_ratio(foreground: [u8; 3], background: [u8; 3]) -> f64 {
        let foreground = relative_luminance(foreground);
        let background = relative_luminance(background);
        let lighter = foreground.max(background);
        let darker = foreground.min(background);
        (lighter + 0.05) / (darker + 0.05)
    }

    fn relative_luminance(color: [u8; 3]) -> f64 {
        let channel = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.040_45 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.0722_f64.mul_add(
            channel(color[2]),
            0.2126_f64.mul_add(channel(color[0]), 0.7152 * channel(color[1])),
        )
    }

    #[test]
    fn bundled_theme_syntax_roles_resolve_to_declared_colors() {
        use bcode_syntax_render::SyntaxColor;

        let catalog = definition::ThemeCatalog::bundled().expect("bundled themes parse");
        for theme_id in [
            "terminal-native",
            "bcode-dark",
            "bcode-light",
            "nord",
            "monochrome",
        ] {
            let resolved = catalog
                .resolve(&definition::ThemeSelection::new(theme_id))
                .unwrap_or_else(|error| panic!("{theme_id} resolves: {error}"));
            let presented =
                resolved_definition_theme(Some(&resolved), PENDING_AGENT_METADATA_ACCENT)
                    .presented(PENDING_AGENT_METADATA_ACCENT);

            for (role, actual) in [
                ("syntax.text", presented.syntax.text),
                ("syntax.comment", presented.syntax.comment),
                ("syntax.keyword", presented.syntax.keyword),
                ("syntax.function", presented.syntax.function),
                ("syntax.variable", presented.syntax.variable),
                ("syntax.string", presented.syntax.string),
                ("syntax.number", presented.syntax.number),
                ("syntax.type", presented.syntax.type_name),
                ("syntax.operator", presented.syntax.operator),
                ("syntax.punctuation", presented.syntax.punctuation),
                ("syntax.heading", presented.syntax.heading),
                ("syntax.link", presented.syntax.link),
                ("syntax.raw", presented.syntax.raw),
            ] {
                let expected = resolved
                    .style(role)
                    .and_then(|style| style.fg)
                    .map_or(actual, SyntaxColor::from_tui);
                assert_eq!(actual, expected, "{theme_id} {role}");
            }
        }
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
                .canvas
                .bg
                .is_none_or(|color| color == Color::Default)
        );
    }

    #[test]
    fn theme_structure_layers_put_density_below_theme_rules() {
        use bcode_presentation_policy::models::{LayerSource, RuleLayer};
        assert_eq!(super::structure_layers_for(None), Vec::new());
        let mut theme = definition::ResolvedThemeDefinition::empty("plain", "Plain");
        theme.presentation.push(RuleLayer {
            source: LayerSource::Theme("plain".to_owned()),
            rules: Vec::new(),
        });
        let sources = |theme: &definition::ResolvedThemeDefinition| {
            super::structure_layers_for(Some(theme))
                .into_iter()
                .map(|layer| layer.source)
                .collect::<Vec<_>>()
        };
        assert_eq!(sources(&theme), [LayerSource::Theme("plain".to_owned())]);
        theme.chrome.density = bcode_tui_theme::chrome::Density::Comfortable;
        assert_eq!(
            sources(&theme),
            [LayerSource::Host, LayerSource::Theme("plain".to_owned())]
        );
    }

    #[test]
    fn tool_border_chrome_reshapes_only_tool_containers() {
        let mut theme = definition::ResolvedThemeDefinition::empty("plain", "Plain");
        let plain = super::resolved_definition_theme(Some(&theme), Color::Cyan);
        theme.chrome.borders.tool = Some(bcode_tui_theme::chrome::BorderStyle::Left);
        let left = super::resolved_definition_theme(Some(&theme), Color::Cyan);
        assert_eq!(left.containers.user, plain.containers.user);
        assert_eq!(left.containers.assistant, plain.containers.assistant);
        for recipe in [
            left.containers.tool_requested,
            left.containers.tool_running,
            left.containers.tool_succeeded,
            left.containers.tool_failed,
            left.containers.tool_timed_out,
        ] {
            assert_eq!(recipe.recipe.layout, definition::ContainerLayout::LeftBar);
            assert_eq!(recipe.recipe.border, definition::ContainerBorder::Left);
        }
        assert_eq!(
            left.unit_chrome.tool_border,
            Some(bcode_tui_theme::chrome::BorderStyle::Left)
        );
    }
}
