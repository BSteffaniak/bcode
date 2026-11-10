//! Render and structure inputs of the V2 engine from the live application (spec sections 3.4,
//! L2, L3, L8, U5, X3, X4, D9).
//!
//! [`TranscriptV2Host::sync_render_inputs`] runs at the end of every update, outside drawing.
//! It compares the application's current inputs with the ones last applied and hands the
//! engine a new configuration only when they changed:
//!
//! * Render configuration ([`RenderConfig`]): the resolved theme definition, Markdown options
//!   (Mermaid from `[tui.markdown]`, the V2-owned `<details>` state), diff viewer options,
//!   accepted plugin visuals and the shell screens' horizontal offsets (C39).
//! * Motion ([`MotionConfig`]): the theme's chrome motion level, instant under
//!   `[tui] reduce_motion` (C22).
//! * Scrollbar ([`ScrollbarMode`]): `[tui] transcript_scrollbar` (VP4, X6).
//! * Structure configuration ([`StructureConfig`]): the presentation-policy layers (chrome
//!   density, plugin manifest defaults, theme and profile sections, configuration, user state,
//!   reasoning visibility) compiled against the host parts and the plugins' declared schemas,
//!   the plugins' declared facts ([`DeclaredPluginFacts`]) and declared target fields.
//!
//! Plugin visual adapters run here, host-side and outside drawing (D9): resident tool artifacts
//! and plugin contributions are routed through the application's plugin presentation with the
//! unit's resolved presentation and the effective frontend keymap, and everything the adapter
//! returned (render mode, header, target, summary, sections, anchors and selection rows) is
//! accepted, bounded, for an exact revision ([`super::plugin_visuals::accepted_visual`]). Mermaid diagrams and images keep their reserved placeholder
//! rows until the host-side workers' results are accepted ([`super::visuals`]); accepted
//! visuals reach the engine through the same configuration.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use bcode_config::{
    TuiDiffViewerConfig, TuiDiffViewerLayout, TuiThemeConfig, TuiTranscriptScrollbar,
};
use bcode_plugin::PluginPresentationDeclaration;
use bcode_plugin::presentation::PluginFactDeclaration;
use bcode_presentation_policy::PolicyRegistry;
use bcode_presentation_policy::models::{
    FactType, FactValue, MAX_PLUGIN_FACT_TEXT_BYTES, MAX_PLUGIN_FACTS, PresentationRule,
};
use bcode_session_view_models::{
    ToolResultView, TranscriptViewItem, TranscriptViewItemId, TranscriptViewItemKind,
};
use bcode_tui_components::plugin_visual::canonical::{
    CanonicalPluginVisual, CanonicalToolVisual, presentation_contribution,
};
use bcode_tui_theme::definition::ResolvedThemeDefinition;
use bcode_tui_transcript::layout::RenderConfig;
use bcode_tui_transcript::layout::context::{
    DiffLayout, DiffViewerOptions, MarkdownOptions, PluginVisualState, stable_hash,
};
use bcode_tui_transcript::layout::producers::visual::ShellScroll;
use bcode_tui_transcript::navigation::MotionConfig;
use bcode_tui_transcript::units::UnitKey;
use bcode_tui_transcript::units::structure::item_facts::tool_arguments;
use bcode_tui_transcript::units::structure::{
    PluginFactSource, PolicyLayers, ResolvedUnitPresentation, StructureConfig,
};
use bcode_tui_transcript::viewport::ScrollbarMode;

use super::TranscriptV2Host;
use crate::app::BmuxApp;

/// The engine scrollbar of the `[tui] transcript_scrollbar` setting (VP4, X6).
#[must_use]
pub const fn scrollbar_mode(setting: TuiTranscriptScrollbar) -> ScrollbarMode {
    match setting {
        TuiTranscriptScrollbar::Off => ScrollbarMode::Off,
        TuiTranscriptScrollbar::Overlay => ScrollbarMode::Overlay,
        TuiTranscriptScrollbar::Gutter => ScrollbarMode::Gutter,
    }
}

/// Largest arguments document scanned for declared facts; larger ones yield none.
const MAX_FACT_ARGUMENT_BYTES: usize = 1024 * 1024;
/// Most plugin adapters routed in one sync; the rest are routed by the next sync.
pub const MAX_VISUALS_PER_SYNC: usize = 32;

/// Render and structure inputs last applied to the engine, plus the V2-owned render state
/// (`<details>` open state and accepted plugin visuals).
#[derive(Debug, Default)]
pub struct RenderInputs {
    /// Theme configuration the current definition was resolved from.
    theme_config: Option<TuiThemeConfig>,
    /// Resolved theme definition (empty when resolution failed).
    theme: Option<Arc<ResolvedThemeDefinition>>,
    /// Motion inputs last applied: theme motion level and `reduce_motion`.
    motion: Option<(bcode_tui_theme::chrome::Motion, bool)>,
    /// Structure inputs last applied.
    structure: Option<StructureInputs>,
    /// Markdown and diff inputs last applied: Mermaid, diff viewer and the working directory
    /// relative Markdown links resolve against.
    shared: Option<(bool, TuiDiffViewerConfig, Option<std::path::PathBuf>)>,
    /// `<details>` open state by content key (`transcript:<unit>:...`).
    details_open: BTreeMap<String, bool>,
    /// The render configuration must be rebuilt (details or visuals changed).
    render_dirty: bool,
    /// Accepted plugin visual rows by item.
    visuals: PluginVisualState,
    /// Inputs each accepted (or rejected) visual was routed with.
    visual_keys: BTreeMap<TranscriptViewItemId, u64>,
    /// Inputs of the last complete visual reconciliation.
    visual_gate: Option<VisualGate>,
    /// Horizontal offsets of canonical shell screens (C39, [`super::plugin_visuals`]).
    shell_scroll: ShellScroll,
}

impl RenderInputs {
    /// The engine was replaced (session change): it keeps the render and structure
    /// configuration, but every resident item is new, so plugin visuals are reconciled again.
    pub const fn engine_reset(&mut self) {
        self.visual_gate = None;
    }

    /// Accept `visual` for `item` as a routed adapter would (tests without a plugin host); the
    /// configuration is rebuilt at the next flush.
    #[cfg(test)]
    pub(super) fn accept_visual_for_test(
        &mut self,
        item: TranscriptViewItemId,
        visual: bcode_tui_transcript::layout::context::AcceptedPluginVisual,
    ) {
        self.visuals.accept(item, visual);
        self.render_dirty = true;
    }

    /// Columns `item`'s shell screen is scrolled by (C39), as last requested.
    pub(super) fn shell_columns(&self, item: &TranscriptViewItemId) -> u64 {
        self.shell_scroll.columns(item)
    }

    /// Scroll `item`'s shell screen to `columns` (C39); the configuration is rebuilt at the
    /// next flush. Returns whether it changed.
    pub(super) fn set_shell_columns(&mut self, item: TranscriptViewItemId, columns: u64) -> bool {
        let changed = self.shell_scroll.set(item, columns);
        self.render_dirty |= changed;
        changed
    }
}

/// Structure inputs compared by value.
#[derive(Debug, Clone, PartialEq)]
struct StructureInputs {
    theme: (String, u64),
    declarations: BTreeMap<String, PluginPresentationDeclaration>,
    config_rules: Vec<PresentationRule>,
    user_rules: Vec<PresentationRule>,
    reasoning_visible: bool,
}

/// Inputs of one complete plugin-visual reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VisualGate {
    content: u64,
    presentation: u64,
    visual_generation: u64,
    width: u64,
    theme: u64,
    structure: u64,
    /// Effective keymap generation: adapters render its key hints.
    keymap: u64,
}

impl TranscriptV2Host {
    /// Bring the engine's render, motion and structure configuration and the accepted plugin
    /// visuals up to date with `app`. Runs outside drawing; a configuration is rebuilt only
    /// when its inputs changed. Returns whether the transcript changed.
    pub fn sync_render_inputs(&mut self, app: &mut BmuxApp, at: Instant) -> bool {
        let mut changed = false;
        let theme_changed = self.sync_theme(app);
        let theme = self
            .render
            .theme
            .clone()
            .unwrap_or_else(|| Arc::new(ResolvedThemeDefinition::empty("default", "Default")));

        let motion = (theme.chrome.motion, app.tui_config().reduce_motion);
        if self.render.motion != Some(motion) {
            self.render.motion = Some(motion);
            let update = self
                .engine
                .set_motion(MotionConfig::for_motion(motion.0, motion.1), at);
            self.absorb(update);
            changed = true;
        }

        changed |= self.set_scrollbar(scrollbar_mode(app.tui_config().transcript_scrollbar), at);

        changed |= self.sync_structure(app, &theme, at);

        let shared = (
            app.tui_config().markdown.mermaid,
            app.effective_diff_viewer_config(),
            app.working_directory().map(std::path::Path::to_path_buf),
        );
        if self.render.shared.as_ref() != Some(&shared) {
            self.render.shared = Some(shared);
            self.render.render_dirty = true;
        }
        if theme_changed {
            self.render.render_dirty = true;
        }
        if self.sync_plugin_visuals(app, &theme) {
            self.render.render_dirty = true;
        }
        changed |= self.flush_render_config(at);
        changed
    }

    /// Hand the engine a new render configuration when its inputs changed since the last one
    /// (`<details>` state, plugin visuals, accepted Markdown visuals, theme, Markdown and diff
    /// options). Returns whether a configuration was applied.
    pub(super) fn flush_render_config(&mut self, at: Instant) -> bool {
        let visuals_changed = self.visuals.take_changed();
        if !std::mem::take(&mut self.render.render_dirty) {
            if !visuals_changed {
                return false;
            }
            // Only accepted visuals changed: re-measure just their owning units (L3, L8).
            let update = self
                .engine
                .set_markdown_visuals(self.visuals.accepted().clone(), at);
            self.absorb(update);
            return true;
        }
        let theme = self
            .render
            .theme
            .clone()
            .unwrap_or_else(|| Arc::new(ResolvedThemeDefinition::empty("default", "Default")));
        let config = self.render_config(theme);
        let update = self.engine.set_render_config(config, at);
        self.absorb(update);
        true
    }

    /// Open or close one Markdown `<details>` block by content key (`transcript:<unit>:...`);
    /// only the owning unit is re-measured at the next sync (L3). Returns whether it changed.
    pub fn set_markdown_details(&mut self, key: String, open: bool) -> bool {
        if self.render.details_open.get(&key) == Some(&open) {
            return false;
        }
        self.render.details_open.insert(key, open);
        self.render.render_dirty = true;
        true
    }

    fn render_config(&self, theme: Arc<ResolvedThemeDefinition>) -> RenderConfig {
        let (mermaid, diff, base_directory) = self.render.shared.clone().unwrap_or_default();
        RenderConfig {
            theme,
            markdown: MarkdownOptions {
                details_open: self.render.details_open.clone(),
                mermaid,
                images: true,
                visuals: self.visuals.accepted().clone(),
                base_directory,
            },
            diff_viewer: diff_viewer_options(diff),
            plugin_visuals: self.render.visuals.clone(),
            shell_scroll: self.render.shell_scroll.clone(),
        }
    }

    /// Resolve the theme definition when the theme configuration changed. Returns whether
    /// the definition changed.
    fn sync_theme(&mut self, app: &mut BmuxApp) -> bool {
        if self.render.theme_config.as_ref() == Some(&app.tui_config().theme) {
            return false;
        }
        let config = app.tui_config().theme.clone();
        let resolved = app
            .resolve_theme_selection_cached(&config.name)
            .map(Arc::new);
        if resolved.is_none() {
            tracing::warn!(theme = %config.name, "V2 transcript theme did not resolve");
        }
        self.render.theme_config = Some(config);
        let changed = match (&self.render.theme, &resolved) {
            (Some(old), Some(new)) => old.fingerprint != new.fingerprint || old.id != new.id,
            (None, None) => false,
            _ => true,
        };
        self.render.theme = resolved;
        changed
    }

    /// Recompile the structure configuration when its inputs changed (U5, X3, X4). Returns
    /// whether it was applied.
    fn sync_structure(
        &mut self,
        app: &BmuxApp,
        theme: &Arc<ResolvedThemeDefinition>,
        at: Instant,
    ) -> bool {
        let unchanged = self.render.structure.as_ref().is_some_and(|inputs| {
            inputs.theme.0 == theme.id
                && inputs.theme.1 == theme.fingerprint
                && inputs.declarations == *app.plugin_presentation_declarations()
                && inputs.config_rules == app.configured_structure_rules()
                && inputs.user_rules == app.user_structure_rules()
                && inputs.reasoning_visible == app.reasoning_visible()
        });
        if unchanged {
            return false;
        }
        let inputs = StructureInputs {
            theme: (theme.id.clone(), theme.fingerprint),
            declarations: app.plugin_presentation_declarations().clone(),
            config_rules: app.configured_structure_rules().to_vec(),
            user_rules: app.user_structure_rules().to_vec(),
            reasoning_visible: app.reasoning_visible(),
        };
        let config = structure_config(&inputs, theme);
        self.render.structure = Some(inputs);
        let update = self.engine.set_structure_config(config, at);
        self.absorb(update);
        self.render.visual_gate = None;
        true
    }

    /// Route plugin visual adapters for resident items whose inputs changed and accept their
    /// rows (L8, D9). Work per sync is bounded by [`MAX_VISUALS_PER_SYNC`] adapter calls.
    /// Returns whether any accepted visual changed.
    fn sync_plugin_visuals(&mut self, app: &BmuxApp, theme: &ResolvedThemeDefinition) -> bool {
        let Some(presentation) = app.plugin_presentation() else {
            return self.clear_plugin_visuals();
        };
        let engine = &self.engine;
        let width = engine.geometry().width;
        let gate = VisualGate {
            content: self.data.content_generation(),
            presentation: presentation.revision(),
            visual_generation: presentation.visual_generation(),
            width,
            theme: theme.fingerprint,
            structure: engine.structure().config().key(),
            keymap: app.effective_keymap_generation(),
        };
        if self.render.visual_gate == Some(gate) {
            return false;
        }
        let Ok(width) = u16::try_from(width) else {
            return false;
        };
        if width == 0 {
            return false;
        }
        let context_base = VisualContextBase::of(app);
        // Adapters render (key hints) with the effective frontend keymap, as in V1.
        let keymap_host = super::plugin_visuals::VisualKeymapHost(app.effective_keymap());
        let items = &engine.session_view().snapshot().transcript.items;
        let mut routed = 0;
        let mut complete = true;
        let mut changed = false;
        let mut seen = std::collections::BTreeSet::new();
        for item in items {
            let Some(source) = plugin_visual(item, presentation) else {
                continue;
            };
            let invocation_id = source
                .visual
                .invocation_id
                .as_deref()
                .unwrap_or("unknown-invocation");
            seen.insert(item.id.clone());
            let structure = engine
                .structure()
                .unit_presentation(&UnitKey::Item(item.id.clone()));
            let key = stable_hash(&(
                item.revision,
                presentation.visual_revision(invocation_id),
                presentation.revision(),
                width,
                theme.fingerprint,
                structure.layout_hash(),
                app.effective_keymap_generation(),
            ));
            if self.render.visual_keys.get(&item.id) == Some(&key) {
                continue;
            }
            if routed == MAX_VISUALS_PER_SYNC {
                complete = false;
                break;
            }
            routed += 1;
            let accepted = context_base.route(
                presentation,
                &keymap_host,
                (item, structure, &source),
                (theme, width, app.working_directory()),
                key,
            );
            self.render.visual_keys.insert(item.id.clone(), key);
            changed |= match accepted {
                Some(visual) => {
                    let replaced = self.render.visuals.accepted(&item.id) != Some(&visual);
                    self.render.visuals.accept(item.id.clone(), visual);
                    replaced
                }
                None => self.render.visuals.remove(&item.id),
            };
        }
        if complete {
            if !self.render.shell_scroll.is_empty() {
                let resident = items
                    .iter()
                    .map(|item| &item.id)
                    .collect::<std::collections::BTreeSet<_>>();
                changed |= self.render.shell_scroll.retain(|id| resident.contains(id));
            }
            let before = self.render.visual_keys.len();
            self.render.visual_keys.retain(|id, _| seen.contains(id));
            if self.render.visual_keys.len() != before {
                self.render.visuals.retain(|id| seen.contains(id));
                changed = true;
            }
            self.render.visual_gate = Some(gate);
        }
        changed
    }

    fn clear_plugin_visuals(&mut self) -> bool {
        self.render.visual_gate = None;
        if self.render.visual_keys.is_empty() {
            return false;
        }
        self.render.visual_keys.clear();
        self.render.visuals = PluginVisualState::new();
        true
    }
}

/// The plugin visual one resident item renders through, routed as V1 routes the same item.
struct ItemVisual<'a> {
    /// Adapter input.
    visual: CanonicalPluginVisual,
    /// Working directory the invocation ran in, when the item records one.
    working_directory: Option<&'a std::path::Path>,
    /// Header title of a transcript-block visual whose adapter supplies none.
    fallback_title: String,
}

impl<'a> ItemVisual<'a> {
    fn new(
        visual: CanonicalToolVisual,
        working_directory: Option<&'a std::path::Path>,
        fallback_title: impl Into<String>,
    ) -> Self {
        let CanonicalToolVisual::Plugin(visual) = visual;
        Self {
            visual,
            working_directory,
            fallback_title: fallback_title.into(),
        }
    }
}

/// The plugin visual `item` renders through, as V1 routes it:
///
/// * A live request draft (no presentation or result yet): the draft adapter.
/// * A plugin-owned invocation presentation: its primary contribution.
/// * A final artifact result: the artifact adapter. A text result has none.
/// * A request without a result (running, waiting or a retained request): the declaring
///   plugin's request schema, when the plugin that owns the tool produced it.
/// * Plugin contributions.
fn plugin_visual<'a>(
    item: &'a TranscriptViewItem,
    presentation: &crate::plugin_tui::PluginTuiPresentation,
) -> Option<ItemVisual<'a>> {
    let draft = |draft| {
        ItemVisual::new(
            CanonicalToolVisual::from_draft_view(draft, &serde_json::json!(item.revision)),
            None,
            "Tool request draft",
        )
    };
    match &item.kind {
        TranscriptViewItemKind::ToolInvocation { tool } => {
            if let Some(pending) = tool.request_draft.as_ref()
                && tool.presentation.is_none()
                && tool.result.is_none()
                && tool.result_text.is_none()
            {
                return Some(draft(pending));
            }
            if let Some(contribution) = presentation_contribution(tool) {
                return Some(ItemVisual::new(
                    CanonicalToolVisual::from_contribution(&contribution),
                    tool.working_directory.as_deref(),
                    "Tool contribution",
                ));
            }
            match &tool.result {
                Some(ToolResultView::Artifact { artifact }) => Some(ItemVisual::new(
                    CanonicalToolVisual::from_artifact(&artifact.artifact),
                    None,
                    artifact.artifact.title.as_deref().unwrap_or("Tool result"),
                )),
                Some(_) => None,
                None if tool.result_text.is_some() => None,
                None => request_visual(tool, presentation),
            }
        }
        TranscriptViewItemKind::ToolRequest { tool } => request_visual(tool, presentation),
        TranscriptViewItemKind::ToolRequestDraft { draft: pending } => Some(draft(pending)),
        TranscriptViewItemKind::ToolContribution {
            contribution,
            invocation,
            ..
        } => Some(ItemVisual::new(
            CanonicalToolVisual::from_contribution(contribution),
            invocation
                .as_deref()
                .and_then(|invocation| invocation.working_directory.as_deref()),
            "Tool contribution",
        )),
        _ => None,
    }
}

/// The request visual of `tool`: its arguments under the request schema of the plugin that
/// declares the tool. A tool another plugin produced (a native tool observed by an external
/// harness shares its name) keeps the generic presentation.
fn request_visual<'a>(
    tool: &'a bcode_session_view_models::ToolInvocationView,
    presentation: &crate::plugin_tui::PluginTuiPresentation,
) -> Option<ItemVisual<'a>> {
    let tool_name = tool.tool_name.as_deref().unwrap_or("unknown tool");
    let (_, declaration) =
        presentation
            .tool_presentation(tool_name)
            .filter(|(declaring_plugin, _)| {
                tool.producer_plugin_id
                    .as_deref()
                    .is_none_or(|producer| producer == *declaring_plugin)
            })?;
    let visual = CanonicalToolVisual::from_request(
        &tool.tool_call_id,
        tool.producer_plugin_id.as_deref(),
        tool_name,
        declaration.effective_request_schema().to_owned(),
        declaration.effective_request_schema_version(),
        tool.arguments_json.as_deref().unwrap_or("{}"),
    )?;
    Some(ItemVisual::new(
        visual,
        tool.working_directory.as_deref(),
        tool_name,
    ))
}

/// Adapter context inputs shared by every item of one sync: diff layout and the
/// renderer-owned plugin theme.
struct VisualContextBase {
    diff_layout: bcode_plugin_sdk::tui::PluginTuiDiffLayout,
    theme: bcode_plugin_sdk::tui::PluginTuiTheme,
}

impl VisualContextBase {
    fn of(app: &BmuxApp) -> Self {
        use bcode_plugin_sdk::tui::PluginTuiDiffLayout;
        let diff = app.effective_diff_viewer_config();
        Self {
            diff_layout: match diff.layout {
                TuiDiffViewerLayout::Auto => PluginTuiDiffLayout::Auto {
                    breakpoint: diff.side_by_side_breakpoint,
                },
                TuiDiffViewerLayout::Unified => PluginTuiDiffLayout::Unified,
                TuiDiffViewerLayout::SideBySide => PluginTuiDiffLayout::SideBySide,
            },
            theme: crate::render::plugin_theme_for_app(app),
        }
    }

    /// Route `source`, the visual of `item` under `structure`, through `presentation` and
    /// accept the adapter's result at revision `key`. The adapter renders at the unit's body
    /// width under `theme` at layout `width` (the width the producer lays its rows out in, as
    /// V1 rendered adapters at the tool's inner width), in the item's working directory or
    /// else the session's.
    fn route(
        &self,
        presentation: &crate::plugin_tui::PluginTuiPresentation,
        keymap_host: &super::plugin_visuals::VisualKeymapHost<'_>,
        (item, structure, source): (
            &TranscriptViewItem,
            &ResolvedUnitPresentation,
            &ItemVisual<'_>,
        ),
        (theme, width, session_directory): (
            &ResolvedThemeDefinition,
            u16,
            Option<&std::path::Path>,
        ),
        key: u64,
    ) -> Option<bcode_tui_transcript::layout::context::AcceptedPluginVisual> {
        let body = bcode_tui_transcript::layout::producers::tool::body_width(
            item,
            structure,
            theme,
            u64::from(width),
        );
        let context = self.context(
            u16::try_from(body).unwrap_or(width).max(1),
            source.working_directory.or(session_directory),
        );
        let context = match structure {
            ResolvedUnitPresentation::Item(item) => context
                .with_presentation(crate::plugin_tui::visual_presentation(&item.presentation)),
            _ => context,
        };
        let visual = &source.visual;
        presentation
            .routed_visual_with_host(
                visual
                    .invocation_id
                    .as_deref()
                    .unwrap_or("unknown-invocation"),
                visual.revision,
                &visual.schema,
                visual.schema_version,
                visual.producer_plugin_id.as_deref(),
                &visual.payload,
                &context,
                keymap_host,
            )
            .map(|routed| {
                let mut accepted = super::plugin_visuals::accepted_visual(routed, key);
                accepted.fallback_title = Some(source.fallback_title.clone());
                accepted
            })
    }

    /// The adapter context of one item rendering `width` cells wide in `working_directory`.
    fn context(
        &self,
        width: u16,
        working_directory: Option<&std::path::Path>,
    ) -> bcode_plugin_sdk::tui::PluginTuiVisualRenderContext {
        bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
            width,
            self.diff_layout,
            working_directory.map(std::path::Path::to_path_buf),
        )
        .with_theme(self.theme)
    }
}

/// Diff viewer options of a diff viewer configuration.
#[must_use]
pub const fn diff_viewer_options(config: TuiDiffViewerConfig) -> DiffViewerOptions {
    DiffViewerOptions {
        layout: match config.layout {
            TuiDiffViewerLayout::Auto => DiffLayout::Auto,
            TuiDiffViewerLayout::Unified => DiffLayout::Unified,
            TuiDiffViewerLayout::SideBySide => DiffLayout::SideBySide,
        },
        side_by_side_breakpoint: config.side_by_side_breakpoint,
    }
}

/// The structure configuration the engine compiles for `app` under `theme`: the same layers,
/// facts and target fields as [`TranscriptV2Host::sync_render_inputs`] (previews such as the
/// appearance picker sample).
#[must_use]
pub fn app_structure_config(app: &BmuxApp, theme: &ResolvedThemeDefinition) -> StructureConfig {
    structure_config(
        &StructureInputs {
            theme: (theme.id.clone(), theme.fingerprint),
            declarations: app.plugin_presentation_declarations().clone(),
            config_rules: app.configured_structure_rules().to_vec(),
            user_rules: app.user_structure_rules().to_vec(),
            reasoning_visible: app.reasoning_visible(),
        },
        theme,
    )
}

/// Compile the structure configuration of `inputs` (U5): the policy layers against the host
/// parts and the plugins' declared schemas, with the plugins' declared facts and target fields.
fn structure_config(inputs: &StructureInputs, theme: &ResolvedThemeDefinition) -> StructureConfig {
    let plugin_layers = bcode_plugin::presentation::presentation_rule_layers(&inputs.declarations);
    let registry = PolicyRegistry::with_host_parts().with_plugin_schemas(
        &bcode_plugin::presentation::presentation_schemas(&inputs.declarations),
    );
    let (mut config, diagnostics) = PolicyLayers {
        theme: Some(theme),
        plugin_layers: &plugin_layers,
        config_rules: &inputs.config_rules,
        user_rules: &inputs.user_rules,
        reasoning_visible: inputs.reasoning_visible,
    }
    .compile(&registry);
    for diagnostic in &diagnostics {
        tracing::debug!(
            source = ?diagnostic.source,
            rule = ?diagnostic.rule,
            message = %diagnostic.message,
            "V2 presentation rule diagnostic"
        );
    }
    config.plugin_facts = Arc::new(DeclaredPluginFacts::new(&inputs.declarations));
    config.target_fields = Arc::new(bcode_plugin::presentation::presentation_target_fields(
        &inputs.declarations,
    ));
    config
}

/// Plugin-declared facts (manifest `[presentation]` facts): derived only for units the
/// declaring plugin produced, from the unit's tool arguments or contribution payload, at most
/// [`MAX_PLUGIN_FACTS`] per unit. Pure: the same item and declarations give the same facts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeclaredPluginFacts {
    generation: u64,
    by_plugin: BTreeMap<String, Vec<PluginFactDeclaration>>,
}

impl DeclaredPluginFacts {
    /// Facts declared by `declarations`; the generation is a stable hash of them.
    #[must_use]
    pub fn new(declarations: &BTreeMap<String, PluginPresentationDeclaration>) -> Self {
        let by_plugin: BTreeMap<String, Vec<PluginFactDeclaration>> = declarations
            .iter()
            .filter(|(_, declaration)| !declaration.facts.is_empty())
            .map(|(plugin, declaration)| (plugin.clone(), declaration.facts.clone()))
            .collect();
        let generation = stable_hash(&format!("{by_plugin:?}"));
        Self {
            generation,
            by_plugin,
        }
    }
}

impl PluginFactSource for DeclaredPluginFacts {
    fn generation(&self) -> u64 {
        self.generation
    }

    fn facts(
        &self,
        item: &TranscriptViewItem,
        producer: &str,
        tool: Option<&str>,
    ) -> BTreeMap<String, FactValue> {
        let Some(declared) = self.by_plugin.get(producer) else {
            return BTreeMap::new();
        };
        let (schema, payload) = match &item.kind {
            TranscriptViewItemKind::ToolContribution { contribution, .. } => (
                Some(contribution.schema.as_str()),
                Some(&contribution.payload),
            ),
            _ => (None, None),
        };
        let applicable: Vec<&PluginFactDeclaration> = declared
            .iter()
            .filter(|fact| {
                fact.tool
                    .as_deref()
                    .is_none_or(|expected| Some(expected) == tool)
                    && fact
                        .schema
                        .as_deref()
                        .is_none_or(|expected| Some(expected) == schema)
            })
            .take(MAX_PLUGIN_FACTS)
            .collect();
        let arguments = if applicable.iter().any(|fact| fact.argument.is_some()) {
            tool_arguments(item)
                .filter(|arguments| arguments.len() <= MAX_FACT_ARGUMENT_BYTES)
                .and_then(|arguments| serde_json::from_str::<serde_json::Value>(arguments).ok())
        } else {
            None
        };
        applicable
            .into_iter()
            .filter_map(|fact| {
                let value = match (&fact.argument, &fact.payload) {
                    (Some(field), _) => arguments.as_ref()?.as_object()?.get(field.as_str()),
                    (None, Some(field)) => payload?.get(field.as_str()),
                    (None, None) => None,
                }?;
                Some((fact.name.clone(), fact_value(value, fact.value_type)?))
            })
            .collect()
    }
}

/// A JSON value as a fact of `value_type`: booleans as `bool`; integers, and array or object
/// lengths, as `int`; strings as one bounded line.
fn fact_value(value: &serde_json::Value, value_type: FactType) -> Option<FactValue> {
    use serde_json::Value;
    let length = |len: usize| i64::try_from(len).ok().map(FactValue::Int);
    match (value_type, value) {
        (FactType::Bool, Value::Bool(value)) => Some(FactValue::Bool(*value)),
        (FactType::Int, Value::Number(number)) => number.as_i64().map(FactValue::Int),
        (FactType::Int, Value::Array(values)) => length(values.len()),
        (FactType::Int, Value::Object(values)) => length(values.len()),
        (FactType::String, Value::String(text)) => {
            bcode_plugin_sdk::tui_visual::bounded_optional_line(
                Some(text),
                MAX_PLUGIN_FACT_TEXT_BYTES,
            )
            .map(FactValue::Text)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
