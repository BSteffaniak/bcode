//! TUI rendering.

use bcode_config::{TuiDiffViewerConfig, TuiDiffViewerLayout};
use bcode_plugin_sdk::tui::{PluginTuiDiffLayout, PluginTuiVisualRenderContext};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

thread_local! {
    static DIFF_VIEWER_CONFIG: Cell<TuiDiffViewerConfig> = const { Cell::new(TuiDiffViewerConfig {
        layout: TuiDiffViewerLayout::Auto,
        side_by_side_breakpoint: 120,
    }) };
    static MARKDOWN_DETAILS_OPEN: RefCell<BTreeMap<String, bool>> = const { RefCell::new(BTreeMap::new()) };
    static PLUGIN_VISUAL_THEME: Cell<Option<bcode_plugin_sdk::tui::PluginTuiTheme>> = const { Cell::new(None) };
    static TRANSCRIPT_THEME: Cell<Option<super::theme::PresentedTheme>> = const { Cell::new(None) };
}

/// Synchronize layout-affecting Markdown presentation state for row generation.
pub fn set_markdown_details_open(details_open: &BTreeMap<String, bool>) {
    MARKDOWN_DETAILS_OPEN.with(|state| details_open.clone_into(&mut state.borrow_mut()));
}

/// Synchronize renderer-owned plugin visual presentation for row generation.
pub fn set_plugin_visual_theme(theme: &super::theme::PresentedTheme) {
    PLUGIN_VISUAL_THEME.with(|state| state.set(Some(plugin_tui_theme(theme))));
    TRANSCRIPT_THEME.with(|state| state.set(Some(*theme)));
}

fn semantic_theme() -> bcode_plugin_sdk::tui::PluginTuiTheme {
    PLUGIN_VISUAL_THEME.with(|state| {
        state.get().unwrap_or_else(|| {
            let initial = super::theme::resolve_initial_theme();
            plugin_tui_theme(&initial.presented(initial.accent))
        })
    })
}

pub fn semantic_state_theme() -> super::theme::PresentedTheme {
    TRANSCRIPT_THEME.with(|state| {
        state.get().unwrap_or_else(|| {
            let initial = super::theme::resolve_initial_theme();
            initial.presented(initial.accent)
        })
    })
}

const MARKDOWN_BODY_INDENT: u16 = 2;

#[derive(Debug, Clone, Copy)]
pub struct TranscriptItemLayout {
    outer_width: u16,
    content_width: u16,
    content_x: u16,
    bottom_rows: usize,
    container: Option<super::theme::ContainerPresentation>,
}

impl TranscriptItemLayout {
    pub(super) fn resolve(
        theme: &super::theme::PresentedTheme,
        item: &TranscriptItem,
        width: u16,
    ) -> Self {
        let container = transcript_item_container(theme, item).filter(|presentation| {
            !matches!(
                presentation.recipe.layout,
                super::theme::definition::ContainerLayout::Plain
            )
        });
        let outer_width = width.max(1);
        let Some(presentation) = container else {
            return Self {
                outer_width,
                content_width: outer_width,
                content_x: 0,
                bottom_rows: 0,
                container: None,
            };
        };
        let metrics = container_metrics(presentation.recipe, usize::from(outer_width));
        let border_rows = usize::from(matches!(
            presentation.recipe.border,
            super::theme::definition::ContainerBorder::All
        ));
        let padding_rows = usize::from(presentation.recipe.padding_y);
        Self {
            outer_width,
            content_width: u16::try_from(metrics.content.max(1)).unwrap_or(u16::MAX),
            content_x: u16::try_from(metrics.left_border.saturating_add(metrics.left_padding))
                .unwrap_or(u16::MAX),
            bottom_rows: border_rows.saturating_add(padding_rows),
            container: Some(presentation),
        }
    }

    fn markdown_width(self) -> u16 {
        self.content_width
            .saturating_sub(MARKDOWN_BODY_INDENT)
            .max(1)
    }

    pub(super) const fn markdown_x(self) -> u16 {
        self.content_x.saturating_add(MARKDOWN_BODY_INDENT)
    }

    pub(super) const fn bottom_rows(self) -> usize {
        self.bottom_rows
    }
}

#[derive(Debug, Clone, Copy)]
struct ContainerMetrics {
    left_border: usize,
    left_padding: usize,
    content: usize,
}

fn container_metrics(
    recipe: super::theme::definition::ContainerRecipe,
    container_width: usize,
) -> ContainerMetrics {
    use super::theme::definition::ContainerBorder;

    let left_border = usize::from(!matches!(recipe.border, ContainerBorder::None));
    let right_border = usize::from(matches!(recipe.border, ContainerBorder::All));
    let interior = container_width
        .saturating_sub(left_border)
        .saturating_sub(right_border);
    let horizontal_padding = usize::from(recipe.padding_x);
    let left_padding = horizontal_padding.min(interior);
    let right_padding = horizontal_padding.min(interior.saturating_sub(left_padding));
    let content = interior
        .saturating_sub(left_padding)
        .saturating_sub(right_padding);
    ContainerMetrics {
        left_border,
        left_padding,
        content,
    }
}

fn apply_container_recipe(rows: &mut Vec<Line>, start: usize, layout: TranscriptItemLayout) {
    let Some(presentation) = layout.container else {
        return;
    };
    bcode_tui_components::transcript::apply_transcript_container(
        rows,
        start,
        presentation.transcript_style(),
        layout.outer_width,
    );
}

fn transcript_item_container(
    theme: &super::theme::PresentedTheme,
    item: &TranscriptItem,
) -> Option<super::theme::ContainerPresentation> {
    match item.kind() {
        TranscriptItemKind::UserMessage => Some(theme.containers.user),
        TranscriptItemKind::AssistantMessage => Some(theme.containers.assistant),
        _ => tool_container_presentation(theme, item),
    }
}

fn tool_container_presentation(
    theme: &super::theme::PresentedTheme,
    item: &TranscriptItem,
) -> Option<super::theme::ContainerPresentation> {
    use bcode_session_view_models::ToolInvocationViewStatus;

    match item.kind() {
        TranscriptItemKind::ToolRequest { status, timing, .. } => {
            if timing.timed_out == Some(true) {
                Some(theme.containers.tool_timed_out)
            } else {
                Some(match status {
                    Some(ToolInvocationViewStatus::Running) => theme.containers.tool_running,
                    Some(ToolInvocationViewStatus::Waiting) => theme.containers.tool_waiting,
                    Some(ToolInvocationViewStatus::Finished) => theme.containers.tool_succeeded,
                    Some(ToolInvocationViewStatus::Failed) => theme.containers.tool_failed,
                    Some(ToolInvocationViewStatus::Cancelled) => theme.containers.tool_cancelled,
                    Some(ToolInvocationViewStatus::Requested) | None => {
                        theme.containers.tool_requested
                    }
                })
            }
        }
        TranscriptItemKind::ToolResult {
            is_error, timing, ..
        } => {
            if timing.timed_out == Some(true) {
                Some(theme.containers.tool_timed_out)
            } else if *is_error {
                Some(theme.containers.tool_failed)
            } else {
                Some(theme.containers.tool_succeeded)
            }
        }
        _ => None,
    }
}

fn markdown_details_open() -> BTreeMap<String, bool> {
    MARKDOWN_DETAILS_OPEN.with(|state| state.borrow().clone())
}

fn plugin_visual_context(
    width: u16,
    working_directory: Option<&std::path::Path>,
) -> PluginTuiVisualRenderContext {
    DIFF_VIEWER_CONFIG.with(|config| {
        let config = config.get();
        let diff_layout = match config.layout {
            TuiDiffViewerLayout::Auto => PluginTuiDiffLayout::Auto {
                breakpoint: config.side_by_side_breakpoint,
            },
            TuiDiffViewerLayout::Unified => PluginTuiDiffLayout::Unified,
            TuiDiffViewerLayout::SideBySide => PluginTuiDiffLayout::SideBySide,
        };
        let context = PluginTuiVisualRenderContext::new(
            width,
            diff_layout,
            working_directory.map(std::path::Path::to_path_buf),
        );
        PLUGIN_VISUAL_THEME.with(|theme| {
            if let Some(theme) = theme.get() {
                context.with_theme(theme)
            } else {
                context
            }
        })
    })
}

/// Build renderer-owned plugin presentation from the active app theme.
#[must_use]
pub fn plugin_theme_for_app(app: &BmuxApp) -> bcode_plugin_sdk::tui::PluginTuiTheme {
    plugin_tui_theme(&app.presented_theme())
}

fn plugin_tui_theme(theme: &super::theme::PresentedTheme) -> bcode_plugin_sdk::tui::PluginTuiTheme {
    let components = theme.component_theme();
    let syntax_color = |color: bcode_syntax_render::SyntaxColor| {
        bcode_plugin_sdk::tui::PluginTuiSyntaxColor::from_tui(color.to_tui())
    };
    let syntax = theme.syntax;
    bcode_plugin_sdk::tui::PluginTuiTheme {
        component_theme_version: bcode_plugin_sdk::tui::PLUGIN_TUI_COMPONENT_THEME_VERSION,
        canvas: components.canvas,
        text: components.text,
        muted: components.muted,
        border: components.border,
        focused: components.focused,
        selection: components.selected,
        source: bcode_plugin_sdk::tui::PluginTuiSourceTheme {
            source: theme.source.source,
            border: theme.source.border,
            gutter: theme.source.gutter,
            truncated: theme.source.truncated,
        },
        diff: bcode_plugin_sdk::tui::PluginTuiDiffTheme {
            text: theme.diff.text,
            muted: theme.diff.muted,
            title: theme.diff.title,
            label: theme.diff.label,
            added: theme.diff.added,
            removed: theme.diff.removed,
            hunk: theme.diff.hunk,
            added_row: theme.diff.added_row,
            removed_row: theme.diff.removed_row,
            added_emphasis: theme.diff.added_emphasis,
            removed_emphasis: theme.diff.removed_emphasis,
        },
        syntax: bcode_plugin_sdk::tui::PluginTuiSyntaxTheme {
            text: syntax_color(syntax.text),
            comment: syntax_color(syntax.comment),
            keyword: syntax_color(syntax.keyword),
            function: syntax_color(syntax.function),
            variable: syntax_color(syntax.variable),
            string: syntax_color(syntax.string),
            number: syntax_color(syntax.number),
            type_name: syntax_color(syntax.type_name),
            operator: syntax_color(syntax.operator),
            punctuation: syntax_color(syntax.punctuation),
        },
    }
}

use std::time::{Duration, Instant};

#[cfg(test)]
use bcode_markdown_render::render_markdown;
use bcode_markdown_render::{
    MarkdownContributionKind, MarkdownDocumentContext, MarkdownRenderOptions, render_markdown_lines,
};
use bcode_plugin_sdk::tui::PluginTuiVisualRenderMode;
use bcode_session_view_models::TextFormat;
use bmux_tui::chrome::Panel;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Span, Style, Widget};
#[cfg(test)]
use bmux_tui::style::Color;
use bmux_tui::style::Modifier;
use bmux_tui_components::text_input::TextInputControl;

use super::activity::ActivityState;
use super::app::{BmuxApp, DaemonConnectionState, composer_policy};
use super::pending_submission::{PendingSubmission, PendingSubmissionState};
use super::time_format::{format_millis, unix_time_millis};
use super::tool_render_projection::{CanonicalPluginVisual, CanonicalToolVisual};
use super::transcript::{
    ToolTiming, TranscriptItem, TranscriptItemKind, TranscriptStreamIntegrity,
};
use super::transcript_layout::TranscriptLayoutSignature;
use bmux_tui::text_width::{display_width as text_display_width, truncate_to_display_width};

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const MAX_COMPOSER_ROWS: u16 = 6;
const MAX_INLINE_TOOL_TEXT_ROWS: usize = 28;
const LATEST_BAR_ACTIVE_WINDOW: Duration = Duration::from_millis(420);
#[derive(Debug, Clone, Copy)]
pub struct TuiTheme {
    pub text: Style,
    pub muted: Style,
    pub border: Style,
    pub focused: Style,
    pub raised: Style,
    pub overlay: Style,
    pub scrim: Option<Style>,
    pub selection: Style,
    pub info: Style,
    pub success: Style,
    pub warning: Style,
    pub error: Style,
}

impl TuiTheme {
    pub const fn modal_theme(self) -> bmux_tui_components::modal_frame::ModalTheme {
        let theme = bmux_tui_components::modal_frame::ModalTheme::new(
            self.overlay,
            self.border.patch(self.overlay),
            self.focused.patch(self.overlay),
            self.text.patch(self.overlay),
            self.muted.patch(self.overlay),
            self.focused.patch(self.overlay),
        );
        if let Some(scrim) = self.scrim {
            theme.with_scrim(scrim)
        } else {
            theme
        }
    }

    #[must_use]
    pub const fn for_app(app: &BmuxApp) -> Self {
        let theme = app.presented_theme();
        Self {
            text: theme.text,
            muted: theme.muted,
            border: theme.border,
            focused: theme.focused.patch(Style::new().fg(theme.accent)),
            raised: theme.surfaces.raised,
            overlay: theme.surfaces.overlay,
            scrim: theme.surfaces.scrim,
            selection: theme.selection,
            info: theme.info,
            success: theme.success,
            warning: theme.warning,
            error: theme.error,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn for_agent(
        agent_id: &str,
        configured_accent: Option<&str>,
        agent_metadata_hydrated: bool,
    ) -> Self {
        let theme = super::theme::resolve_initial_theme();
        Self {
            text: theme.text,
            muted: theme.muted,
            border: theme.border,
            focused: theme
                .focused
                .patch(Style::new().fg(super::theme::target_agent_accent(
                    agent_id,
                    configured_accent,
                    agent_metadata_hydrated,
                ))),
            raised: theme.surfaces.raised,
            overlay: theme.surfaces.overlay,
            scrim: theme.surfaces.scrim,
            selection: theme.selection,
            info: theme.info,
            success: theme.success,
            warning: theme.warning,
            error: theme.error,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn for_theme_id(theme_id: &str) -> Self {
        let catalog =
            super::theme::definition::ThemeCatalog::bundled().expect("bundled themes parse");
        let resolved = catalog
            .resolve(&super::theme::definition::ThemeSelection::new(theme_id))
            .unwrap_or_else(|error| panic!("{theme_id} resolves: {error}"));
        let theme = super::theme::resolved_definition_theme(
            Some(&resolved),
            super::theme::PENDING_AGENT_METADATA_ACCENT,
        )
        .presented(super::theme::PENDING_AGENT_METADATA_ACCENT);
        Self {
            text: theme.text,
            muted: theme.muted,
            border: theme.border,
            focused: theme.focused.patch(Style::new().fg(theme.accent)),
            raised: theme.surfaces.raised,
            overlay: theme.surfaces.overlay,
            scrim: theme.surfaces.scrim,
            selection: theme.selection,
            info: theme.info,
            success: theme.success,
            warning: theme.warning,
            error: theme.error,
        }
    }
}

/// Prepared geometry for one TUI frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout {
    area: Rect,
    header: Rect,
    pub(crate) body: Rect,
    latest_bar: Option<Rect>,
    status: Rect,
    composer: Rect,
    composer_content: Rect,
}

impl FrameLayout {
    /// Return the transcript body area.
    #[must_use]
    pub const fn body(self) -> Rect {
        self.body
    }

    /// Return the animated latest-content bar area, when visible.
    #[must_use]
    pub const fn latest_bar(self) -> Option<Rect> {
        self.latest_bar
    }

    /// Return the status-line area.
    #[must_use]
    pub const fn status(self) -> Rect {
        self.status
    }

    /// Return the composer panel area.
    #[must_use]
    pub const fn composer(self) -> Rect {
        self.composer
    }
}

/// Compute the transcript area for a full terminal frame.
#[must_use]
pub fn transcript_area_for_frame(app: &BmuxApp, area: Rect) -> Rect {
    if area.is_empty() {
        return area;
    }
    let composer_height = composer_height(app, area);
    let composer_y = area.bottom().saturating_sub(composer_height);
    let body_height = composer_y.saturating_sub(area.y.saturating_add(2));
    let body = Rect::new(area.x, area.y.saturating_add(1), area.width, body_height);
    transcript_area_for_body(app, body)
}

/// Prepare derived frame projections before rendering.
#[cfg(test)]
pub fn prepare_frame(app: &mut BmuxApp, area: Rect) -> Option<FrameLayout> {
    prepare_frame_with_bottom_dock(app, area, 0).map(|(layout, _dock)| layout)
}

/// Prepare a frame while reserving a non-overlapping dock at the bottom of the body.
///
/// The returned dock is excluded from transcript projection and rendering.
pub fn prepare_frame_with_bottom_dock(
    app: &mut BmuxApp,
    area: Rect,
    dock_height: u16,
) -> Option<(FrameLayout, Rect)> {
    let layout = frame_layout(app, area)?;
    app.set_composer_content_area(layout.composer_content);
    let (layout, dock) = layout.with_bottom_dock(app, dock_height);
    super::transcript_projection::prepare_for_body(app, layout.body_without_latest_bar());
    let refreshed = frame_layout(app, area)
        .map_or(layout, |layout| layout.with_bottom_dock(app, dock_height).0);
    Some((refreshed, dock))
}

/// Render one TUI frame.
#[cfg(test)]
pub fn render(app: &mut BmuxApp, frame: &mut Frame<'_>) {
    if let Some(layout) = prepare_frame(app, frame.area()) {
        render_prepared(app, frame, layout);
    }
}

/// Render one TUI frame after [`prepare_frame`] has synchronized projections.
#[cfg(test)]
pub fn render_prepared(app: &mut BmuxApp, frame: &mut Frame<'_>, layout: FrameLayout) {
    render_prepared_damage(app, frame, layout, |_| true);
}

/// Render only prepared layout regions selected by terminal-space damage.
pub fn render_prepared_damage(
    app: &mut BmuxApp,
    frame: &mut Frame<'_>,
    layout: FrameLayout,
    intersects: impl Fn(Rect) -> bool,
) {
    if layout.area.is_empty() {
        return;
    }

    let theme = TuiTheme::for_app(app);
    if intersects(layout.header) {
        frame.fill(layout.header, " ", app.presented_theme().canvas);
        render_header(app, layout.header, frame, theme);
    }
    if intersects(layout.composer) {
        frame.fill(layout.composer, " ", app.presented_theme().canvas);
        render_composer(app, layout.composer, frame, theme);
    }
    if intersects(layout.body) {
        frame.fill(layout.body, " ", app.presented_theme().canvas);
        app.transcript_markdown_cache().retain_resident_iter(
            app.transcript().iter(),
            app.transcript_projection_revision(),
        );
        let focused_regions = transcript_markdown_regions(app, layout.body);
        if app.begin_markdown_semantics_reconciliation(layout.body.width) {
            let footnote_rows = transcript_markdown_footnote_rows(app, layout.body.width);
            app.reconcile_markdown_footnote_rows(footnote_rows);
            let fragment_rows = transcript_markdown_fragment_rows(app, layout.body.width);
            app.reconcile_markdown_fragments(fragment_rows);
            let resident_details = transcript_markdown_details_ids(app, layout.body.width);
            app.reconcile_markdown_details(&resident_details);
        }
        let mut visible = Vec::new();
        let mut seen_ids = std::collections::BTreeSet::new();
        for region in focused_regions {
            if seen_ids.insert(region.contribution_id.clone()) {
                visible.push(crate::markdown_interaction::VisibleMarkdownContribution {
                    id: region.contribution_id,
                    kind: region.contribution_kind,
                });
            }
        }
        app.reconcile_markdown_interactions(visible);
        render_body(app, layout.body, frame);
        render_markdown_source_view(app, layout.body, frame);
    }
    if let Some(latest_bar) = layout.latest_bar
        && intersects(latest_bar)
    {
        render_latest_bar(app, latest_bar, frame, Instant::now());
    }
    if intersects(layout.status) {
        frame.fill(layout.status, " ", app.presented_theme().canvas);
        render_status(app, layout.status, frame, theme);
    }
}

impl FrameLayout {
    fn with_bottom_dock(self, app: &BmuxApp, requested_height: u16) -> (Self, Rect) {
        let full_body = self.body_without_latest_bar();
        let maximum_height = full_body.height.saturating_sub(1);
        let dock_height = requested_height.min(maximum_height);
        if dock_height == 0 {
            return (
                self,
                Rect::new(full_body.x, full_body.bottom(), full_body.width, 0),
            );
        }
        let dock = Rect::new(
            full_body.x,
            full_body.bottom().saturating_sub(dock_height),
            full_body.width,
            dock_height,
        );
        let latest_bar_height = u16::from(app.newer_transcript_content_below());
        let transcript_height = full_body
            .height
            .saturating_sub(dock_height)
            .saturating_sub(latest_bar_height);
        let body = Rect::new(full_body.x, full_body.y, full_body.width, transcript_height);
        let latest_bar = (latest_bar_height > 0).then_some(Rect::new(
            full_body.x,
            body.bottom(),
            full_body.width,
            1,
        ));
        (
            Self {
                body,
                latest_bar,
                ..self
            },
            dock,
        )
    }

    const fn body_without_latest_bar(self) -> Rect {
        let latest_bar_height = if self.latest_bar.is_some() { 1 } else { 0 };
        Rect::new(
            self.body.x,
            self.body.y,
            self.body.width,
            self.body.height.saturating_add(latest_bar_height),
        )
    }
}

fn frame_layout(app: &BmuxApp, area: Rect) -> Option<FrameLayout> {
    if area.is_empty() {
        return None;
    }

    let header = Rect::new(area.x, area.y, area.width, 1);
    let composer_height = composer_height(app, area);
    let composer = composer_area(area, composer_height);
    let body_height = composer.y.saturating_sub(area.y.saturating_add(2));
    let body = Rect::new(area.x, area.y.saturating_add(1), area.width, body_height);
    let latest_bar_height = u16::from(app.newer_transcript_content_below());
    let body = Rect::new(
        body.x,
        body.y,
        body.width,
        body.height.saturating_sub(latest_bar_height),
    );
    let latest_bar =
        (latest_bar_height > 0).then_some(Rect::new(area.x, body.bottom(), area.width, 1));
    let status = Rect::new(
        area.x,
        composer.y.saturating_sub(1),
        area.width,
        u16::from(composer.y > area.y.saturating_add(1)),
    );
    Some(FrameLayout {
        area,
        header,
        body,
        latest_bar,
        status,
        composer,
        composer_content: composer_panel(TuiTheme::for_app(app)).inner_area(composer),
    })
}

#[cfg(test)]
#[test]
fn transcript_selection_scene_reflows_without_changing_logical_source_ranges() {
    let session_id = bcode_session_models::SessionId::new();
    let history = [bcode_session_models::SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence: 1,
        timestamp_ms: 1,
        session_id,
        provenance: None,
        kind: bcode_session_models::SessionEventKind::UserMessage {
            client_id: bcode_session_models::ClientId::new(),
            text: "alpha beta".to_owned(),
            admission: bcode_session_models::TurnAdmissionMetadata::default(),
        },
    }];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let wide_layout = prepare_frame(&mut app, Rect::new(0, 0, 40, 10)).expect("wide layout");
    let wide = super::root_program::transcript_selection_scene(&app, wide_layout.body());
    let wide_ranges = wide
        .fragments()
        .iter()
        .map(|fragment| {
            (
                fragment.content_id.clone(),
                fragment.source_range.clone(),
                fragment.revision,
            )
        })
        .collect::<Vec<_>>();

    let narrow_layout = prepare_frame(&mut app, Rect::new(0, 0, 12, 10)).expect("narrow layout");
    let narrow = super::root_program::transcript_selection_scene(&app, narrow_layout.body());
    let narrow_ranges = narrow
        .fragments()
        .iter()
        .map(|fragment| {
            (
                fragment.content_id.clone(),
                fragment.source_range.clone(),
                fragment.revision,
            )
        })
        .collect::<Vec<_>>();

    assert!(!wide_ranges.is_empty());
    assert_eq!(narrow_ranges, wide_ranges);
    assert!(wide.validate().is_ok());
    assert!(narrow.validate().is_ok());
    assert!(
        narrow
            .fragments()
            .iter()
            .all(|fragment| { fragment.area.intersection(narrow_layout.body()) == fragment.area })
    );
}

#[cfg(test)]
#[test]
fn details_state_survives_reconstruction_resize_and_cache_reuse_then_drops_on_replacement() {
    let source = "<details><summary>More</summary>Body that wraps across several cells.</details>";
    let item = TranscriptItem::with_format("System", source.to_owned(), TextFormat::Markdown);
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    let initial = render_markdown(source, &markdown_render_options(&app, &item, 24));
    let details = initial
        .contributions
        .iter()
        .find(|item| matches!(item.kind, MarkdownContributionKind::Details { .. }))
        .expect("details contribution");
    let details_id = details.id.clone();
    app.reconcile_markdown_interactions(vec![
        crate::markdown_interaction::VisibleMarkdownContribution {
            id: details_id.clone(),
            kind: details.kind.clone(),
        },
    ]);
    assert!(app.activate_markdown_contribution(&details_id));

    let narrow = render_markdown(source, &markdown_render_options(&app, &item, 24));
    let wide = render_markdown(source, &markdown_render_options(&app, &item, 60));
    for rendered in [&narrow, &wide] {
        let text = rendered
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(text.contains("▼ More"));
        assert!(text.contains("Body that wraps"));
    }
    let cached = render_markdown(source, &markdown_render_options(&app, &item, 24));
    assert_eq!(narrow.lines, cached.lines);
    assert_eq!(narrow.geometry, cached.geometry);
    assert_eq!(narrow.layout_signature, cached.layout_signature);

    let replacement = TranscriptItem::with_format(
        "System",
        "<details><summary>Changed</summary>Replacement</details>".to_owned(),
        TextFormat::Markdown,
    );
    let replacement_ids =
        transcript_markdown_details_ids_for_items(&app, std::iter::once(&replacement), 24);
    app.reconcile_markdown_details(&replacement_ids);
    assert!(!app.markdown_details_open().contains_key(&details_id));
}

#[cfg(test)]
#[test]
fn markdown_details_expand_and_collapse() {
    let source = include_str!("../tests/fixtures/details_interaction.md");
    let item = TranscriptItem::with_format("System", source.to_owned(), TextFormat::Markdown);
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    let initial = render_markdown(source, &markdown_render_options(&app, &item, 80));
    let details = initial
        .contributions
        .iter()
        .find(|contribution| {
            matches!(
                &contribution.kind,
                MarkdownContributionKind::Details { summary, .. }
                    if summary.contains("Retry algorithm")
            )
        })
        .expect("retry details contribution");
    let details_id = details.id.clone();
    let initial_text = initial
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.content.as_str())
        .collect::<String>();
    assert!(initial_text.contains("▶ Retry algorithm"));
    assert!(!initial_text.contains("delay(n)"));

    app.reconcile_markdown_interactions(vec![
        crate::markdown_interaction::VisibleMarkdownContribution {
            id: details_id.clone(),
            kind: details.kind.clone(),
        },
    ]);
    assert!(app.activate_markdown_contribution(&details_id));
    let expanded = render_markdown(source, &markdown_render_options(&app, &item, 80));
    let expanded_text = expanded
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.content.as_str())
        .collect::<String>();
    assert!(expanded_text.contains("▼ Retry algorithm"));
    assert!(expanded_text.contains("delay(n)"));
    assert!(expanded_text.contains("Attempt"));

    assert!(app.activate_markdown_contribution(&details_id));
    let collapsed = render_markdown(source, &markdown_render_options(&app, &item, 80));
    let collapsed_text = collapsed
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.content.as_str())
        .collect::<String>();
    assert!(collapsed_text.contains("▶ Retry algorithm"));
    assert!(!collapsed_text.contains("delay(n)"));
}

#[cfg(test)]
#[test]
fn local_plain_markdown_and_json_system_notes_keep_distinct_formats() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.push_system_plain("* literal".to_owned());
    app.push_system_markdown("* rendered".to_owned());
    app.push_system_json(r#"{"value":1}"#.to_owned());

    assert_eq!(app.local_notices().len(), 3);
    assert!(app.session_view_snapshot().transcript.items.is_empty());
    assert_eq!(
        app.local_notices()[0].text_format(),
        bcode_session_view_models::TextFormat::PlainText
    );
    assert_eq!(
        app.local_notices()[1].text_format(),
        bcode_session_view_models::TextFormat::Markdown
    );
    assert_eq!(
        app.local_notices()[2].text_format(),
        bcode_session_view_models::TextFormat::Json
    );
}

#[cfg(test)]
#[test]
fn streamed_text_integrity_is_visible_without_rewriting_source_bytes() {
    let body = "retained suffix";
    for (integrity, expected) in [
        (
            TranscriptStreamIntegrity::Incomplete,
            "Earlier streamed text is unavailable; showing the retained checkpoint.",
        ),
        (
            TranscriptStreamIntegrity::Degraded,
            "Stream integrity is degraded; waiting for authoritative resynchronization.",
        ),
    ] {
        let item =
            TranscriptItem::new("Assistant", body.to_owned()).with_stream_integrity(integrity);
        let rows = transcript_item_rows(
            std::slice::from_ref(&item),
            0,
            80,
            None,
            TuiDiffViewerConfig::default(),
        );
        let rendered = rows
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(rendered.contains(expected));
        assert!(rendered.contains(body));
        assert_eq!(item.text(), body);
    }
}

#[cfg(test)]
#[test]
fn transcript_layout_and_semantics_share_one_markdown_projection() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.push_system_markdown("# Heading\n\n[guide](https://example.com)".to_owned());
    let area = Rect::new(0, 0, 80, 24);

    prepare_frame(&mut app, area).expect("prepared frame");
    let item = &app.transcript()[0];
    let first = transcript_markdown_projection(&app, item, area.width);
    let second = transcript_markdown_projection(&app, item, area.width);

    assert!(std::sync::Arc::ptr_eq(&first, &second));
    assert_eq!(app.transcript_markdown_cache().render_count(), 1);
}

#[cfg(test)]
#[test]
fn pure_scroll_reuses_markdown_projection_without_rendering() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.push_system_markdown(
        (0..80)
            .map(|index| format!("## Heading {index}\n\nParagraph {index}."))
            .collect::<Vec<_>>()
            .join("\n\n"),
    );
    let area = Rect::new(0, 0, 60, 12);

    prepare_frame(&mut app, area).expect("initial frame");
    assert_eq!(app.transcript_markdown_cache().render_count(), 1);
    assert!(app.scroll_transcript_up(4));
    prepare_frame(&mut app, area).expect("scrolled frame");
    assert_eq!(app.transcript_markdown_cache().render_count(), 1);
}

#[cfg(test)]
#[test]
fn bottom_dock_never_overlaps_transcript_and_preserves_one_row() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    let terminal = Rect::new(0, 0, 80, 20);
    let normal = prepare_frame(&mut app, terminal).expect("normal frame layout");
    let normal_body_height = normal.body_without_latest_bar().height;
    let (layout, dock) =
        prepare_frame_with_bottom_dock(&mut app, terminal, 100).expect("frame layout");
    let transcript = transcript_area_for_body(&app, layout.body);

    assert_eq!(dock.height, normal_body_height.saturating_sub(1));
    assert_eq!(transcript.height, 1);
    assert!(transcript.bottom() <= dock.y);
    assert_eq!(transcript.width, dock.width);
}

#[cfg(test)]
#[test]
fn docked_frame_preserves_anchored_transcript_top_and_latest_indicator() {
    let session_id = bcode_session_models::SessionId::new();
    let history = (0..30)
        .map(|sequence| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: format!("context line {sequence}"),
            },
        })
        .collect::<Vec<_>>();
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let terminal = Rect::new(0, 0, 60, 16);
    prepare_frame(&mut app, terminal).expect("normal frame");
    assert!(app.scroll_transcript_up(8));
    let normal_top = app.transcript_top_row(transcript_area_for_frame(&app, terminal).height);
    let normal_has_newer = app.newer_transcript_content_below();

    let (docked, dock) =
        prepare_frame_with_bottom_dock(&mut app, terminal, 6).expect("docked frame");
    let docked_top = app.transcript_top_row(docked.body.height);
    assert_eq!(docked_top, normal_top);
    assert!(normal_has_newer);
    assert!(app.newer_transcript_content_below());
    assert!(docked.body.bottom() <= dock.y);

    prepare_frame_with_bottom_dock(&mut app, terminal, 0).expect("restored frame");
    let restored_top = app.transcript_top_row(transcript_area_for_frame(&app, terminal).height);
    assert_eq!(restored_top, normal_top);
}

#[cfg(test)]
#[tokio::test]
async fn explanatory_assistant_context_remains_visible_above_question_dock() {
    let session_id = bcode_session_models::SessionId::new();
    let history = vec![bcode_session_models::SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence: 1,
        timestamp_ms: 1,
        session_id,
        provenance: None,
        kind: bcode_session_models::SessionEventKind::AssistantMessage {
            text: "Review this explanation before choosing.".to_owned(),
        },
    }];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let plugin = bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/question-plugin/bcode-plugin.toml"),
        bcode_question_plugin::static_plugin(),
    );
    let runtime = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection::all_enabled(),
        &[plugin],
    )
    .expect("question plugin runtime");
    let mut surface = super::interactive_surface::InteractiveSurfaceState::open(
        &runtime,
        "question-call-question",
        "bcode.question.inline",
        &serde_json::json!({
            "questions": [{
                "header": null,
                "question": "Proceed with the explained choice?",
                "options": [{"label": "Yes", "value": "yes", "description": null}],
                "control": "radio",
                "selection_mode": "single",
                "custom": false,
                "custom_mode": "additional",
                "required": true
            }]
        })
        .to_string(),
        &crate::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
    )
    .await
    .expect("question surface");
    let terminal = Rect::new(0, 0, 64, 18);
    let preferred_height = surface.preferred_height(terminal.width);
    let (layout, dock) =
        prepare_frame_with_bottom_dock(&mut app, terminal, preferred_height).expect("docked frame");
    let mut buffer = bmux_tui::buffer::Buffer::empty(terminal);
    let mut frame = Frame::new(&mut buffer);
    render_prepared(&mut app, &mut frame, layout);
    surface.render_for_test(dock, &mut frame);

    let context_row = (0..terminal.height)
        .find(|row| {
            buffer
                .row_symbols(*row)
                .is_some_and(|line| line.contains("Review this explanation"))
        })
        .expect("assistant context row");
    let question_row = (0..terminal.height)
        .find(|row| {
            buffer
                .row_symbols(*row)
                .is_some_and(|line| line.contains("Proceed with the explained choice?"))
        })
        .expect("question row");
    assert!(context_row < dock.y);
    assert!(question_row >= dock.y);
    assert!(context_row < question_row);
}

#[cfg(test)]
#[test]
fn resolved_canvas_fills_the_normal_frame_without_opaque_terminal_native_fallback() {
    let area = Rect::new(0, 0, 48, 14);

    let mut opaque = BmuxApp::new_with_history(None, &[], &[], false);
    assert!(opaque.apply_theme("bcode-dark"));
    let mut opaque_buffer = bmux_tui::buffer::Buffer::empty(area);
    let mut opaque_frame = Frame::new(&mut opaque_buffer);
    render(&mut opaque, &mut opaque_frame);
    assert_eq!(
        opaque_buffer
            .get(bmux_tui::geometry::Point::new(47, 7))
            .map(|cell| cell.style.bg),
        Some(Some(Color::Rgb(11, 16, 32)))
    );

    let mut native = BmuxApp::new_with_history(None, &[], &[], false);
    assert!(native.apply_theme("terminal-native"));
    let mut native_buffer = bmux_tui::buffer::Buffer::empty(area);
    let mut native_frame = Frame::new(&mut native_buffer);
    render(&mut native, &mut native_frame);
    assert!(
        native_buffer
            .get(bmux_tui::geometry::Point::new(47, 7))
            .is_some_and(|cell| cell.style.bg.is_none_or(|color| color == Color::Default))
    );
}

#[cfg(test)]
#[test]
fn bottom_dock_handles_short_narrow_and_resized_terminals() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    for terminal in [
        Rect::new(0, 0, 8, 6),
        Rect::new(0, 0, 24, 8),
        Rect::new(0, 0, 120, 40),
        Rect::new(0, 0, 18, 7),
    ] {
        let (layout, dock) = prepare_frame_with_bottom_dock(&mut app, terminal, 100)
            .expect("constrained frame layout");
        let transcript = transcript_area_for_body(&app, layout.body);
        assert!(transcript.bottom() <= dock.y);
        assert!(dock.right() <= terminal.right());
        assert!(dock.bottom() <= terminal.bottom());
        assert!(transcript.height >= 1 || layout.body.height == 0);
    }
}

#[cfg(test)]
#[test]
fn zero_height_bottom_dock_preserves_normal_layout() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    let terminal = Rect::new(0, 0, 80, 20);
    let normal = prepare_frame(&mut app, terminal).expect("normal frame layout");
    let (docked, dock) =
        prepare_frame_with_bottom_dock(&mut app, terminal, 0).expect("docked frame layout");

    assert_eq!(normal, docked);
    assert_eq!(dock.height, 0);
}

fn render_latest_bar(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>, now: Instant) {
    if area.is_empty() {
        return;
    }
    frame.push_hit(
        HitRegion::new("latest-bar", area)
            .role(HitRole::ListItem)
            .layer(1),
    );
    let line = latest_bar_line(
        area.width,
        app.jump_to_latest_key_label(),
        app.latest_hidden_activity_at(),
        app.latest_hidden_activity_burst(),
        app.latest_bar_animation_started_at(),
        now,
    );
    frame.fill(area, " ", latest_bar_background_style());
    frame.write_line_with_fallback_style(area, &line, latest_bar_background_style());
}

fn latest_bar_line(
    width: u16,
    key_label: &str,
    latest_hidden_activity_at: Option<Instant>,
    latest_hidden_activity_burst: u8,
    animation_started_at: Instant,
    now: Instant,
) -> Line {
    let active_age = latest_hidden_activity_at.and_then(|at| {
        let age = now.saturating_duration_since(at);
        (age < LATEST_BAR_ACTIVE_WINDOW).then_some(age)
    });
    active_age.map_or_else(
        || stale_latest_bar_line(width, key_label),
        |active_age| {
            active_latest_bar_line(
                width,
                key_label,
                latest_bar_effective_burst(latest_hidden_activity_burst, active_age),
                animation_started_at,
                now,
            )
        },
    )
}

fn latest_bar_effective_burst(burst: u8, active_age: Duration) -> u8 {
    let age_ms = active_age.as_millis();
    let window_ms = LATEST_BAR_ACTIVE_WINDOW.as_millis().max(1);
    let remaining = window_ms.saturating_sub(age_ms);
    let scaled = (u128::from(burst) * remaining).div_ceil(window_ms);
    u8::try_from(scaled.clamp(1, 8)).unwrap_or(1)
}

fn active_latest_bar_line(
    width: u16,
    key_label: &str,
    burst: u8,
    animation_started_at: Instant,
    now: Instant,
) -> Line {
    let width = usize::from(width);
    let text = latest_bar_message(width, key_label);
    let text = centered_bar_text(&text, width);
    let text_width = text_display_width(&text);
    let left_width = width.saturating_sub(text_width) / 2;
    let right_width = width.saturating_sub(text_width).saturating_sub(left_width);
    let phase = latest_bar_phase(
        animation_started_at,
        now,
        latest_bar_active_frame_duration(burst),
    );
    let mut spans = Vec::new();
    push_latest_bar_glow_rail(&mut spans, left_width, phase, burst, false);
    spans.push(Span::styled(
        text,
        latest_bar_background_style()
            .patch(latest_bar_active_text_style())
            .add_modifier(Modifier::BOLD),
    ));
    push_latest_bar_glow_rail(
        &mut spans,
        right_width,
        phase.saturating_add(left_width / 3),
        burst,
        true,
    );
    Line::from_spans(spans)
}

fn stale_latest_bar_line(width: u16, key_label: &str) -> Line {
    let theme = semantic_state_theme();
    bcode_tui_components::activity::stale_latest_activity_line(
        width,
        key_label,
        bcode_tui_components::activity::LatestActivityStyle {
            background: theme.canvas,
            muted: theme.muted,
            info: theme.info,
        },
    )
}

fn latest_bar_message(width: usize, key_label: &str) -> String {
    if width < 30 {
        format!("messages below · {key_label}")
    } else {
        format!("New messages below · {key_label} to jump")
    }
}

fn centered_bar_text(text: &str, width: usize) -> String {
    truncate_to_display_width(text, width)
}

fn latest_bar_phase(started_at: Instant, now: Instant, frame: Duration) -> usize {
    usize::try_from(
        now.saturating_duration_since(started_at).as_millis() / frame.as_millis().max(1),
    )
    .unwrap_or_default()
}

fn latest_bar_active_frame_duration(burst: u8) -> Duration {
    Duration::from_millis(
        210_u64
            .saturating_sub(u64::from(burst).saturating_mul(21))
            .max(36),
    )
}

fn push_latest_bar_glow_rail(
    spans: &mut Vec<Span>,
    width: usize,
    phase: usize,
    burst: u8,
    reverse: bool,
) {
    const LOW_GLYPHS: [&str; 3] = ["·", "•", "▾"];
    const HIGH_GLYPHS: [&str; 3] = ["·", "◆", "▾"];
    let glyphs = if burst >= 5 { HIGH_GLYPHS } else { LOW_GLYPHS };
    if width == 0 {
        return;
    }
    let intensity = usize::from(burst.min(8));
    let period = 18_usize.saturating_sub(intensity.saturating_mul(2)).max(4);
    let trail = 1_usize.saturating_add(intensity);
    let phase_step = 1_usize.saturating_add(intensity / 3);
    for column in 0..width {
        let wave_column = if reverse {
            width.saturating_sub(column).saturating_sub(1)
        } else {
            column
        };
        let wave = wave_column.saturating_add(phase.saturating_mul(phase_step)) % period;
        let distance = wave.min(period.saturating_sub(wave));
        let glyph_index = match distance {
            0 => 2,
            1 | 2 => 1,
            _ if distance <= trail => 0,
            _ => usize::MAX,
        };
        if glyph_index == usize::MAX {
            spans.push(Span::styled(" ", latest_bar_background_style()));
            continue;
        }
        let role_style = if distance == 0 {
            semantic_state_theme().info
        } else {
            semantic_state_theme().focused
        };
        let mut style = latest_bar_background_style().patch(role_style);
        if distance == 0 || (intensity >= 3 && distance <= 1) || (intensity >= 7 && distance <= 2) {
            style = style.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(glyphs[glyph_index], style));
    }
}

fn latest_bar_active_text_style() -> Style {
    semantic_state_theme().text
}

fn latest_bar_background_style() -> Style {
    semantic_state_theme().canvas
}

fn composer_height(app: &BmuxApp, area: Rect) -> u16 {
    if area.height == 0 {
        return 0;
    }
    let content_width = area.width.saturating_sub(4).max(1);
    let rows = TextInputControl::new(&composer_policy())
        .visible_rows_for_width(app.composer_state(), content_width);
    let content_rows = rows.clamp(1, MAX_COMPOSER_ROWS);
    content_rows
        .saturating_add(2)
        .min(area.height.saturating_sub(2).max(3))
        .min(area.height)
}

const fn composer_area(area: Rect, composer_height: u16) -> Rect {
    Rect::new(
        area.x,
        area.bottom().saturating_sub(composer_height),
        area.width,
        composer_height,
    )
}

fn composer_panel(theme: TuiTheme) -> Panel {
    bcode_tui_components::composer::composer_panel(bcode_tui_components::composer::ComposerStyle {
        border: theme.focused,
        surface: theme.raised,
    })
}

fn render_header(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>, theme: TuiTheme) {
    if area.is_empty() {
        return;
    }

    let line = Line::from_spans(header_spans(app, usize::from(area.width), theme));
    frame.write_line(area, &line);
}

fn header_spans(app: &BmuxApp, width: usize, theme: TuiTheme) -> Vec<Span> {
    let muted = theme.muted;
    let accent = theme.focused;
    let session_title = app
        .session_title()
        .map_or_else(|| "Untitled session".to_owned(), ToOwned::to_owned);
    let mut line = ChromeLine::new(" · ", muted)
        .required(
            "bcode".to_owned(),
            theme.focused.add_modifier(Modifier::BOLD),
            false,
        )
        .required(app.display_agent_id().to_owned(), accent, false)
        .required(app.model_header_label(), Style::new(), false)
        .required(session_title, Style::new(), true)
        .optional(
            format!(
                "provider {}",
                app.selected_provider_plugin_id().unwrap_or("auto")
            ),
            accent,
            50,
            false,
        );

    line = line.optional(
        super::build_info().display_version().to_owned(),
        muted,
        5,
        false,
    );

    if let Some(session_id) = app.session_id() {
        line = line.optional(short_session_id(&session_id.to_string()), muted, 10, false);
    }

    line.spans(width)
}

fn short_session_id(session_id: &str) -> String {
    format!("#{}", session_id.chars().take(8).collect::<String>())
}

#[cfg(test)]
mod header_tests {
    use super::*;

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|span| span.content.as_str()).collect()
    }

    #[test]
    fn full_frame_contains_build_version_in_header() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 160, 24));
        render(&mut app, &mut Frame::new(&mut buffer));
        let header = buffer.row_symbols(0).expect("header row");
        assert!(header.contains(super::super::build_info().display_version()));
    }

    #[test]
    fn header_shows_build_version_when_space_is_available() {
        let app = BmuxApp::new_with_history(None, &[], &[], false);
        let rendered = text(&header_spans(&app, 200, TuiTheme::for_app(&app)));
        assert!(rendered.contains("bcode"));
        assert!(rendered.contains(super::super::build_info().display_version()));
    }

    #[test]
    fn header_omits_build_version_before_required_context() {
        let app = BmuxApp::new_with_history(None, &[], &[], false);
        let rendered = text(&header_spans(&app, 50, TuiTheme::for_app(&app)));
        assert!(rendered.contains("bcode"));
        assert!(rendered.contains(app.display_agent_id()));
        assert!(!rendered.contains(super::super::build_info().display_version()));
        assert!(!rendered.ends_with(" · "));
    }
}

fn render_markdown_source_view(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>) {
    let Some((_, source)) = app.markdown_source_view() else {
        return;
    };
    let area = area.intersection(Rect::new(
        area.x.saturating_add(2),
        area.y.saturating_add(1),
        area.width.saturating_sub(4),
        area.height.saturating_sub(2),
    ));
    if area.is_empty() {
        return;
    }
    frame.fill(area, " ", app.presented_theme().canvas);
    let title = Line::from_spans(vec![Span::styled(
        " Mermaid source · Alt+Enter closes ",
        TuiTheme::for_app(app).info.add_modifier(Modifier::BOLD),
    )]);
    frame.write_line(Rect::new(area.x, area.y, area.width, 1), &title);
    for (offset, line) in source
        .lines()
        .take(usize::from(area.height.saturating_sub(1)))
        .enumerate()
    {
        frame.write_line(
            Rect::new(
                area.x,
                area.y
                    .saturating_add(1)
                    .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX)),
                area.width,
                1,
            ),
            &Line::from_spans(vec![Span::styled(
                truncate_to_display_width(line, usize::from(area.width)),
                TuiTheme::for_app(app).text,
            )]),
        );
    }
}

fn render_body(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    render_transcript(app, area, frame);
    frame.push_hit(
        HitRegion::new("transcript", area)
            .role(HitRole::Scroll)
            .layer(0),
    );
}

pub const fn transcript_area_for_body(_app: &BmuxApp, area: Rect) -> Rect {
    area
}

pub fn markdown_render_options(
    app: &BmuxApp,
    item: &TranscriptItem,
    width: u16,
) -> MarkdownRenderOptions {
    let layout = TranscriptItemLayout::resolve(&app.presented_theme(), item, width);
    let mut options = MarkdownRenderOptions::new(layout.markdown_width())
        .with_theme(app.presented_theme().markdown)
        .with_syntax_palette(app.presented_theme().syntax)
        .with_document_id(format!("transcript:{}", item.id().get()))
        .with_streaming(item.streaming())
        .with_link_destination_fallbacks(true)
        .with_mermaid_contributions(1600, 1200)
        .with_rich_reserved_rows(
            crate::markdown_image::RESERVED_IMAGE_ROWS,
            crate::markdown_mermaid::RESERVED_MERMAID_ROWS,
        );
    if let Some(base_directory) = app.working_directory() {
        options = options.with_document_context(MarkdownDocumentContext {
            base_directory: Some(base_directory.to_path_buf()),
            ..MarkdownDocumentContext::default()
        });
    }
    let document_prefix = format!("transcript:{}:", item.id().get());
    let details_open = app
        .markdown_details_open()
        .iter()
        .filter(|(id, _)| id.starts_with(&document_prefix))
        .map(|(id, open)| (id.clone(), *open))
        .collect();
    options.with_details_open(details_open)
}

fn render_transcript(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    if app.transcript().is_empty() && app.pending_submissions().is_empty() {
        return;
    }

    let top_row = app.transcript_top_row(area.height);
    let mut y = area.y;
    for visible in app
        .transcript_layout()
        .visible_lines_from_top(top_row, area.height)
    {
        if y >= area.bottom() {
            break;
        }
        if let Some(row) = app.transcript_layout().line(visible) {
            frame.write_line(Rect::new(area.x, y, area.width, 1), row);
            y = y.saturating_add(1);
        }
    }
    render_transcript_markdown_hits(app, area, top_row, frame);
}

#[derive(Debug)]
struct MarkdownTranscriptRegion {
    contribution_id: String,
    contribution_kind: MarkdownContributionKind,
    rect_index: usize,
    rect: Rect,
}

/// One rich Markdown contribution projected into the current transcript frame.
#[derive(Debug, Clone)]
pub struct MarkdownRichRegion {
    /// Stable owner-qualified contribution identity.
    pub contribution_id: String,
    /// Typed semantic payload.
    pub contribution_kind: MarkdownContributionKind,
    /// Clipped visible geometry, or `None` for resident off-screen contributions.
    pub visible_rect: Option<Rect>,
}

fn render_transcript_markdown_hits(
    app: &BmuxApp,
    area: Rect,
    _top_row: usize,
    frame: &mut Frame<'_>,
) {
    for region in transcript_markdown_regions(app, area) {
        if app.focused_markdown_contribution() == Some(region.contribution_id.as_str()) {
            for y in region.rect.y..region.rect.bottom() {
                for x in region.rect.x..region.rect.right() {
                    if let Some(cell) = frame
                        .buffer_mut()
                        .get_mut(bmux_tui::geometry::Point::new(x, y))
                    {
                        cell.style = cell.style.add_modifier(Modifier::REVERSED);
                    }
                }
            }
        }
        frame.push_hit(
            HitRegion::new(
                format!("markdown:{}:{}", region.contribution_id, region.rect_index),
                region.rect,
            )
            .role(HitRole::Action)
            .layer(1),
        );
    }
}

fn transcript_markdown_footnote_rows(
    app: &BmuxApp,
    width: u16,
) -> std::collections::BTreeMap<String, usize> {
    let mut rows = std::collections::BTreeMap::new();
    for (index, item) in app.transcript().iter().enumerate() {
        if item.text_format() != TextFormat::Markdown {
            continue;
        }
        let Some(entry_start) = app.transcript_layout().entry_start_row(
            super::transcript_layout::VisibleTranscriptSource::Transcript,
            index,
        ) else {
            continue;
        };
        let content_offset = transcript_markdown_content_row_offset(app, item, index, width);
        let rendered = transcript_markdown_projection(app, item, width);
        for geometry in &rendered.geometry {
            let is_footnote = rendered.contributions.iter().any(|contribution| {
                contribution.id == geometry.contribution_id
                    && matches!(
                        contribution.kind,
                        MarkdownContributionKind::FootnoteReference { .. }
                            | MarkdownContributionKind::FootnoteDefinition { .. }
                    )
            });
            if is_footnote && let Some(rect) = geometry.rects.first() {
                rows.insert(
                    geometry.contribution_id.clone(),
                    entry_start
                        .saturating_add(content_offset)
                        .saturating_add(usize::from(rect.y)),
                );
            }
        }
    }
    rows
}

fn transcript_markdown_fragment_rows(
    app: &BmuxApp,
    width: u16,
) -> std::collections::BTreeMap<String, usize> {
    let mut fragments = std::collections::BTreeMap::new();
    for (index, item) in app.transcript().iter().enumerate() {
        if item.text_format() != TextFormat::Markdown {
            continue;
        }
        let Some(entry_start) = app.transcript_layout().entry_start_row(
            super::transcript_layout::VisibleTranscriptSource::Transcript,
            index,
        ) else {
            continue;
        };
        let content_offset = transcript_markdown_content_row_offset(app, item, index, width);
        let rendered = transcript_markdown_projection(app, item, width);
        for anchor in &rendered.anchors {
            fragments.entry(anchor.fragment.clone()).or_insert_with(|| {
                entry_start
                    .saturating_add(content_offset)
                    .saturating_add(usize::from(anchor.row))
            });
        }
    }
    fragments
}

fn transcript_markdown_details_ids(
    app: &BmuxApp,
    width: u16,
) -> std::collections::BTreeSet<String> {
    transcript_markdown_details_ids_for_items(app, app.transcript().iter(), width)
}

fn transcript_markdown_details_ids_for_items<'a>(
    app: &BmuxApp,
    items: impl Iterator<Item = &'a TranscriptItem>,
    width: u16,
) -> std::collections::BTreeSet<String> {
    items
        .filter(|item| item.text_format() == TextFormat::Markdown)
        .flat_map(|item| {
            transcript_markdown_projection(app, item, width)
                .contributions
                .iter()
                .filter(|contribution| {
                    matches!(contribution.kind, MarkdownContributionKind::Details { .. })
                })
                .map(|contribution| contribution.id.clone())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn visible_markdown_entry_indexes(app: &BmuxApp, area: Rect) -> std::collections::BTreeSet<usize> {
    app.transcript_layout()
        .visible_transcript_entry_indexes(app.transcript_top_row(area.height), area.height)
}

/// Collect resident rich Markdown contributions and their current visible geometry.
#[must_use]
pub fn transcript_markdown_rich_regions(app: &BmuxApp, area: Rect) -> Vec<MarkdownRichRegion> {
    let top_row = app.transcript_top_row(area.height);
    let visible_indexes = visible_markdown_entry_indexes(app, area);
    let mut rich = Vec::new();
    for index in visible_indexes {
        let Some(item) = app.transcript().get(index) else {
            continue;
        };
        if item.text_format() != TextFormat::Markdown {
            continue;
        }
        let Some(entry_start) = app.transcript_layout().entry_start_row(
            super::transcript_layout::VisibleTranscriptSource::Transcript,
            index,
        ) else {
            continue;
        };
        let layout = TranscriptItemLayout::resolve(&app.presented_theme(), item, area.width);
        let content_offset = transcript_markdown_content_row_offset(app, item, index, area.width);
        let rendered = transcript_markdown_projection(app, item, area.width);
        for contribution in &rendered.contributions {
            if !matches!(
                contribution.kind,
                MarkdownContributionKind::Image { .. } | MarkdownContributionKind::Mermaid { .. }
            ) {
                continue;
            }
            let visible_rect = rendered
                .geometry
                .iter()
                .find(|geometry| geometry.contribution_id == contribution.id)
                .and_then(|geometry| {
                    geometry.rects.iter().find_map(|rect| {
                        let global_row = entry_start
                            .saturating_add(content_offset)
                            .saturating_add(usize::from(rect.y));
                        let viewport_row = global_row.checked_sub(top_row)?;
                        let projected = markdown_screen_rect(area, viewport_row, layout, *rect)
                            .intersection(area);
                        (!projected.is_empty()).then_some(projected)
                    })
                });
            rich.push(MarkdownRichRegion {
                contribution_id: contribution.id.clone(),
                contribution_kind: contribution.kind.clone(),
                visible_rect,
            });
        }
    }
    rich
}

fn markdown_screen_rect(
    area: Rect,
    viewport_row: usize,
    layout: TranscriptItemLayout,
    rect: bcode_markdown_render::MarkdownCellRect,
) -> Rect {
    Rect::new(
        area.x
            .saturating_add(layout.markdown_x())
            .saturating_add(rect.x),
        area.y
            .saturating_add(u16::try_from(viewport_row).unwrap_or(u16::MAX)),
        rect.width,
        rect.height,
    )
}

fn transcript_markdown_regions(app: &BmuxApp, area: Rect) -> Vec<MarkdownTranscriptRegion> {
    let top_row = app.transcript_top_row(area.height);
    let visible_indexes = visible_markdown_entry_indexes(app, area);
    let mut regions = Vec::new();
    for index in visible_indexes {
        let Some(item) = app.transcript().get(index) else {
            continue;
        };
        if item.text_format() != TextFormat::Markdown {
            continue;
        }
        let Some(entry_start) = app.transcript_layout().entry_start_row(
            super::transcript_layout::VisibleTranscriptSource::Transcript,
            index,
        ) else {
            continue;
        };
        let layout = TranscriptItemLayout::resolve(&app.presented_theme(), item, area.width);
        let content_offset = transcript_markdown_content_row_offset(app, item, index, area.width);
        let rendered = transcript_markdown_projection(app, item, area.width);
        for geometry in &rendered.geometry {
            let Some(contribution) = rendered
                .contributions
                .iter()
                .find(|contribution| contribution.id == geometry.contribution_id)
            else {
                continue;
            };
            if !markdown_contribution_actionable(&contribution.kind) {
                continue;
            }
            for (rect_index, rect) in geometry.rects.iter().enumerate() {
                let global_row = entry_start
                    .saturating_add(content_offset)
                    .saturating_add(usize::from(rect.y));
                let Some(viewport_row) = global_row.checked_sub(top_row) else {
                    continue;
                };
                let clipped =
                    markdown_screen_rect(area, viewport_row, layout, *rect).intersection(area);
                if clipped.is_empty() {
                    continue;
                }
                regions.push(MarkdownTranscriptRegion {
                    contribution_id: geometry.contribution_id.clone(),
                    contribution_kind: contribution.kind.clone(),
                    rect_index,
                    rect: clipped,
                });
            }
        }
    }
    regions
}

pub fn transcript_markdown_projection_for_layout(
    app: &BmuxApp,
    item: &TranscriptItem,
    width: u16,
) -> Option<std::sync::Arc<bcode_markdown_render::MarkdownRenderResult>> {
    (item.text_format() == TextFormat::Markdown).then(|| {
        let options = markdown_render_options(app, item, width);
        app.transcript_markdown_cache()
            .get(item.id().get(), item.revision(), &options)
            .or_else(|| {
                app.transcript_markdown_cache()
                    .get_previous_compatible(item.id().get(), &options)
            })
            .unwrap_or_else(|| app.transcript_markdown_cache().project(item, options))
    })
}

fn transcript_markdown_projection(
    app: &BmuxApp,
    item: &TranscriptItem,
    width: u16,
) -> std::sync::Arc<bcode_markdown_render::MarkdownRenderResult> {
    transcript_markdown_projection_for_layout(app, item, width)
        .expect("Markdown projection requested for non-Markdown transcript item")
}

fn transcript_markdown_content_row_offset(
    app: &BmuxApp,
    item: &TranscriptItem,
    index: usize,
    width: u16,
) -> usize {
    let entry_rows = app
        .transcript_layout()
        .entry_row_count(
            super::transcript_layout::VisibleTranscriptSource::Transcript,
            index,
        )
        .unwrap_or_default();
    let layout = TranscriptItemLayout::resolve(&app.presented_theme(), item, width);
    let markdown_rows = transcript_markdown_projection(app, item, width).lines.len();
    entry_rows
        .saturating_sub(markdown_rows)
        .saturating_sub(1)
        .saturating_sub(layout.bottom_rows)
}

#[cfg(test)]
fn markdown_hit_regions_for_item(
    item: &TranscriptItem,
    item_start_row: usize,
    top_row: usize,
    area: Rect,
) -> Vec<HitRegion> {
    let layout = TranscriptItemLayout::resolve(&semantic_state_theme(), item, area.width);
    let rendered = render_markdown(
        item.text(),
        &MarkdownRenderOptions::new(layout.markdown_width())
            .with_document_id(format!("transcript:{}", item.id().get()))
            .with_streaming(item.streaming()),
    );
    let content_offset = {
        let rows = transcript_item_rows(
            std::slice::from_ref(item),
            0,
            area.width,
            None,
            TuiDiffViewerConfig::default(),
        );
        rows.len()
            .saturating_sub(rendered.lines.len())
            .saturating_sub(1)
            .saturating_sub(layout.bottom_rows)
    };
    rendered
        .geometry
        .iter()
        .filter_map(|geometry| {
            rendered
                .contributions
                .iter()
                .find(|contribution| contribution.id == geometry.contribution_id)
                .map(|contribution| (geometry, contribution))
        })
        .filter(|(_, contribution)| markdown_contribution_actionable(&contribution.kind))
        .flat_map(|(geometry, _)| {
            geometry
                .rects
                .iter()
                .enumerate()
                .filter_map(move |(rect_index, rect)| {
                    let global_row = item_start_row
                        .saturating_add(content_offset)
                        .saturating_add(usize::from(rect.y));
                    let viewport_row = global_row.checked_sub(top_row)?;
                    let clipped =
                        markdown_screen_rect(area, viewport_row, layout, *rect).intersection(area);
                    (!clipped.is_empty()).then(|| {
                        HitRegion::new(
                            format!("markdown:{}:{rect_index}", geometry.contribution_id),
                            clipped,
                        )
                        .role(HitRole::Action)
                        .layer(1)
                    })
                })
        })
        .collect()
}

const fn markdown_contribution_actionable(kind: &MarkdownContributionKind) -> bool {
    match kind {
        MarkdownContributionKind::Link { destination, .. }
        | MarkdownContributionKind::GitHubIssue { destination, .. } => matches!(
            destination,
            bcode_markdown_render::MarkdownDestination::Web(_)
                | bcode_markdown_render::MarkdownDestination::LocalPath(_)
                | bcode_markdown_render::MarkdownDestination::Fragment(_)
        ),
        MarkdownContributionKind::Details { .. }
        | MarkdownContributionKind::FootnoteReference { .. }
        | MarkdownContributionKind::FootnoteDefinition { .. }
        | MarkdownContributionKind::Mermaid { .. } => true,
        MarkdownContributionKind::Image { .. }
        | MarkdownContributionKind::InlineMath { .. }
        | MarkdownContributionKind::DisplayMath { .. } => false,
    }
}

#[cfg(test)]
#[test]
fn markdown_hit_regions_map_clip_and_filter_actions() {
    let item = TranscriptItem::with_format(
        "System",
        "before [safe](https://example.com) and [unsafe](javascript:alert(1)) after".to_owned(),
        TextFormat::Markdown,
    );
    let area = Rect::new(10, 5, 24, 3);
    let regions = markdown_hit_regions_for_item(&item, 7, 7, area);

    assert_eq!(regions.len(), 1);
    assert!(regions[0].id.as_str().contains("link:"));
    assert_eq!(regions[0].role, HitRole::Action);
    assert_eq!(regions[0].layer, 1);
    assert!(regions[0].area.x >= area.x && regions[0].area.right() <= area.right());
    assert!(regions[0].area.y >= area.y && regions[0].area.bottom() <= area.bottom());
}

#[cfg(test)]
#[test]
fn markdown_hit_regions_follow_scroll_resize_and_replacement() {
    let mut original = TranscriptItem::with_format(
        "System",
        "[a wrapped contribution](https://example.com)".to_owned(),
        TextFormat::Markdown,
    );
    let area = Rect::new(3, 4, 12, 2);
    let visible = markdown_hit_regions_for_item(&original, 10, 10, area);
    let scrolled = markdown_hit_regions_for_item(&original, 10, 11, area);
    let resized = markdown_hit_regions_for_item(&original, 10, 10, Rect::new(3, 4, 30, 3));

    assert!(!visible.is_empty());
    assert!(scrolled.iter().all(|region| region.area.y >= area.y));
    assert_ne!(
        visible.iter().map(|region| region.area).collect::<Vec<_>>(),
        scrolled
            .iter()
            .map(|region| region.area)
            .collect::<Vec<_>>()
    );
    assert_ne!(
        visible.iter().map(|region| region.area).collect::<Vec<_>>(),
        resized.iter().map(|region| region.area).collect::<Vec<_>>()
    );
    assert_eq!(
        visible[0]
            .id
            .as_str()
            .split(':')
            .take(3)
            .collect::<Vec<_>>(),
        resized[0]
            .id
            .as_str()
            .split(':')
            .take(3)
            .collect::<Vec<_>>()
    );

    let cached_again = markdown_hit_regions_for_item(&original, 10, 10, area);
    assert_eq!(visible, cached_again);

    original.append_text(" trailing stream text");
    let appended = markdown_hit_regions_for_item(&original, 10, 10, area);
    assert_eq!(visible, appended);

    let reconstructed =
        TranscriptItem::with_format("System", original.text().to_owned(), TextFormat::Markdown);
    let reconstructed_regions = markdown_hit_regions_for_item(&reconstructed, 10, 10, area);
    assert_eq!(
        appended
            .iter()
            .map(|region| region.area)
            .collect::<Vec<_>>(),
        reconstructed_regions
            .iter()
            .map(|region| region.area)
            .collect::<Vec<_>>()
    );

    let replacement = TranscriptItem::with_format(
        "System",
        "plain replacement".to_owned(),
        TextFormat::Markdown,
    );
    assert!(markdown_hit_regions_for_item(&replacement, 10, 10, area).is_empty());

    let details = TranscriptItem::with_format(
        "System",
        "<details><summary>More</summary>Body</details>".to_owned(),
        TextFormat::Markdown,
    );
    assert!(
        markdown_hit_regions_for_item(&details, 10, 10, area)
            .iter()
            .any(|region| region.id.as_str().contains(":details:"))
    );
    let changed_details = TranscriptItem::with_format(
        "System",
        "<details><summary>Changed</summary>Body</details>".to_owned(),
        TextFormat::Markdown,
    );
    let changed_regions = markdown_hit_regions_for_item(&changed_details, 10, 10, area);
    assert!(changed_regions.iter().all(|region| {
        !visible
            .iter()
            .any(|old| old.id == region.id && old.area == region.area)
    }));
}

#[cfg(test)]
pub fn transcript_item_rows_from_item(
    item: &TranscriptItem,
    width: u16,
    plugin_host: Option<&crate::plugin_tui::PluginTuiPresentation>,
    diff_viewer_config: TuiDiffViewerConfig,
) -> Vec<Line> {
    transcript_item_rows_from_item_with_markdown(item, width, plugin_host, diff_viewer_config, None)
}

pub fn transcript_item_rows_from_item_with_markdown(
    item: &TranscriptItem,
    width: u16,
    plugin_host: Option<&crate::plugin_tui::PluginTuiPresentation>,
    diff_viewer_config: TuiDiffViewerConfig,
    markdown: Option<&bcode_markdown_render::MarkdownRenderResult>,
) -> Vec<Line> {
    DIFF_VIEWER_CONFIG.with(|config| config.set(diff_viewer_config));
    let mut rows = Vec::new();
    push_transcript_item_rows(&mut rows, item, width, plugin_host, markdown);
    rows
}

#[cfg(test)]
pub fn transcript_item_rows(
    transcript: &[TranscriptItem],
    index: usize,
    width: u16,
    plugin_host: Option<&crate::plugin_tui::PluginTuiPresentation>,
    diff_viewer_config: TuiDiffViewerConfig,
) -> Vec<Line> {
    transcript_item_rows_from_item(&transcript[index], width, plugin_host, diff_viewer_config)
}

pub fn pending_submission_rows(pending: &PendingSubmission, width: u16) -> Vec<Line> {
    if matches!(pending.state(), PendingSubmissionState::Sent) {
        return Vec::new();
    }
    let mut rows = Vec::new();
    push_pending_submission_rows(&mut rows, pending, width);
    rows
}

pub fn history_banner_rows(has_older_history: bool, loading_older_history: bool) -> Vec<Line> {
    history_banner_text(has_older_history, loading_older_history).map_or_else(Vec::new, |text| {
        vec![Line::from_spans(vec![Span::styled(
            text,
            semantic_theme().muted,
        )])]
    })
}

pub const fn history_banner_text(
    has_older_history: bool,
    loading_older_history: bool,
) -> Option<&'static str> {
    if loading_older_history {
        Some("Loading older history…")
    } else if has_older_history {
        Some("Scroll up to load older history")
    } else {
        None
    }
}

pub fn transcript_item_signature(
    item: &TranscriptItem,
    width: u16,
    _inline_view_config: (),
) -> TranscriptLayoutSignature {
    TranscriptLayoutSignature::new(format!(
        "item:{}:{}:{width}:{}:{}",
        item.id().get(),
        item.revision(),
        match item.stream_integrity() {
            Some(TranscriptStreamIntegrity::Incomplete) => "incomplete",
            Some(TranscriptStreamIntegrity::Degraded) => "degraded",
            None => "healthy",
        },
        terminal_elapsed_signature_fragment(item).unwrap_or_default()
    ))
}

pub fn terminal_elapsed_signature_fragment(item: &TranscriptItem) -> Option<String> {
    let timing = item.tool_timing()?;
    if !item.tool_is_active() {
        return None;
    }
    let now_ms = unix_time_millis(std::time::SystemTime::now());
    let elapsed = timing
        .started_at_ms
        .map(|started_at_ms| format_millis(now_ms.saturating_sub(started_at_ms)))
        .unwrap_or_default();
    let timeout = timing.timeout_ms.map(format_millis).unwrap_or_default();
    Some(format!("{elapsed}:{timeout}"))
}

pub fn pending_submission_signature(
    pending: &PendingSubmission,
    width: u16,
) -> TranscriptLayoutSignature {
    TranscriptLayoutSignature::new(format!(
        "pending:{width}:{:?}:{}",
        pending.state(),
        pending.text()
    ))
}

#[allow(clippy::too_many_lines)]
fn push_transcript_item_rows(
    rows: &mut Vec<Line>,
    item: &TranscriptItem,
    width: u16,
    plugin_host: Option<&crate::plugin_tui::PluginTuiPresentation>,
    markdown: Option<&bcode_markdown_render::MarkdownRenderResult>,
) {
    let item_start = rows.len();
    let layout = TranscriptItemLayout::resolve(&semantic_state_theme(), item, width);
    let width = layout.content_width;
    if let Some(integrity) = item.stream_integrity() {
        let message = match integrity {
            TranscriptStreamIntegrity::Incomplete => {
                "Earlier streamed text is unavailable; showing the retained checkpoint."
            }
            TranscriptStreamIntegrity::Degraded => {
                "Stream integrity is degraded; waiting for authoritative resynchronization."
            }
        };
        let theme = semantic_state_theme();
        push_wrapped_styled_text(
            rows,
            Vec::new(),
            "Stream status",
            width,
            theme.transcript.stream_status_label,
            theme.transcript.meta,
        );
        push_wrapped_styled_text(
            rows,
            vec![Span::styled("  ", theme.transcript.meta)],
            message,
            width,
            theme.transcript.meta,
            theme.transcript.meta,
        );
    }
    match item.kind() {
        TranscriptItemKind::UserMessage => {
            if let Some(markdown) = markdown {
                push_markdown_projection_block(
                    rows,
                    &item.display_role(),
                    markdown,
                    semantic_state_theme().transcript.user_label,
                    width,
                    true,
                );
            } else {
                push_formatted_block(
                    rows,
                    &item.display_role(),
                    item.text(),
                    item.text_format(),
                    semantic_state_theme().transcript.user_label,
                    true,
                    width,
                );
            }
        }
        TranscriptItemKind::AssistantMessage => {
            push_assistant_rows(rows, item, width, markdown);
        }
        TranscriptItemKind::ReasoningMessage => {
            push_reasoning_rows(rows, item, width, markdown);
        }
        TranscriptItemKind::ToolRequest {
            tool_call_id,
            producer_plugin_id,
            tool_name,
            working_directory,
            timing: _,
            active: _,
            status,
        } => {
            let canonical_request_visual = plugin_host
                .and_then(|presentation| presentation.tool_presentation(tool_name))
                .and_then(|(_, declaration)| {
                    CanonicalToolVisual::from_request(
                        tool_call_id,
                        producer_plugin_id.as_deref(),
                        tool_name,
                        declaration.effective_request_schema().to_owned(),
                        declaration.effective_request_schema_version(),
                        item.text(),
                    )
                });
            if canonical_request_visual.as_ref().is_some_and(|visual| {
                push_canonical_tool_visual_rows(
                    rows,
                    visual,
                    working_directory.as_deref(),
                    width,
                    plugin_host,
                )
            }) {
                rows.push(Line::default());
            } else {
                let context = ToolRequestRenderContext {
                    tool_name,
                    status: *status,
                };
                push_tool_request_rows(rows, item, &context, width);
            }
        }
        TranscriptItemKind::ToolResult {
            tool_call_id: _,
            tool_name,
            arguments_json: _,
            working_directory,
            result,
            artifact,
            is_error,
            ..
        } => {
            push_tool_result_rows(
                rows,
                item,
                &ToolResultRenderContext {
                    tool_name: tool_name.as_deref(),
                    result,
                    artifact: artifact.as_deref(),
                    working_directory: working_directory.as_deref(),
                    is_error: *is_error,
                    has_file_preview: false,
                },
                width,
                plugin_host,
            );
        }
        TranscriptItemKind::PermissionRequest {
            permission_id,
            tool_call_id,
            tool_name,
            ..
        } => {
            push_permission_request_rows(rows, item, permission_id, tool_call_id, tool_name, width);
        }
        TranscriptItemKind::PermissionResult { approved } => {
            push_detail_block(
                rows,
                "Permission",
                item.text(),
                if *approved {
                    semantic_state_theme().success
                } else {
                    semantic_state_theme().error
                },
                width,
            );
        }
        TranscriptItemKind::System => {
            if let Some(markdown) = markdown {
                push_markdown_projection_block(
                    rows,
                    &item.display_role(),
                    markdown,
                    semantic_state_theme().transcript.system_label,
                    width,
                    false,
                );
            } else {
                push_formatted_block(
                    rows,
                    &item.display_role(),
                    item.text(),
                    item.text_format(),
                    semantic_state_theme().transcript.system_label,
                    false,
                    width,
                );
            }
        }
        TranscriptItemKind::Meta => {
            push_meta_block(rows, item.text(), width);
        }
        TranscriptItemKind::Skill => {
            push_detail_block(
                rows,
                "Skill",
                item.text(),
                semantic_state_theme().transcript.skill_label,
                width,
            );
        }
        TranscriptItemKind::SkillError => {
            push_detail_block(
                rows,
                "Skill error",
                item.text(),
                semantic_state_theme().transcript.skill_error_label,
                width,
            );
        }
        TranscriptItemKind::ToolRequestDraft { draft } => {
            let artifact = bcode_session_models::ToolArtifact {
                artifact_id: format!("{}-request-draft-{}", draft.tool_call_id, draft.generation),
                producer_plugin_id: draft
                    .producer_plugin_id
                    .clone()
                    .unwrap_or_else(|| "bcode.unknown".to_owned()),
                schema: draft.schema.clone(),
                schema_version: draft.schema_version,
                tool_call_id: Some(draft.tool_call_id.clone()),
                title: Some("Tool request draft".to_owned()),
                metadata: serde_json::json!({
                    "tool_name": draft.tool_name,
                    "argument_bytes": draft.argument_bytes,
                    "preview_start_offset": draft.preview_start_offset,
                    "preview": draft.preview,
                    "truncated": draft.truncated,
                    "streaming": true,
                }),
                refs: Vec::new(),
            };
            let visual = CanonicalToolVisual::from_artifact(&artifact);
            if canonical_plugin_visual_available(&visual, plugin_host)
                && push_canonical_tool_visual_rows(rows, &visual, None, width, plugin_host)
            {
                rows.push(Line::default());
            } else {
                push_meta_block(rows, item.text(), width);
            }
        }
        TranscriptItemKind::ToolContribution {
            contribution,
            placement,
            invocation,
        } => {
            let artifact = bcode_session_models::ToolArtifact {
                artifact_id: format!(
                    "{}-{}",
                    contribution.invocation_id, contribution.contribution_id
                ),
                producer_plugin_id: contribution.producer_id.clone(),
                schema: contribution.schema.clone(),
                schema_version: contribution.schema_version,
                tool_call_id: Some(contribution.invocation_id.clone()),
                title: Some("Tool contribution".to_owned()),
                metadata: {
                    let mut payload = contribution.payload.clone();
                    if let Some(object) = payload.as_object_mut() {
                        object.insert(
                            "_bcode_presentation_revision".to_owned(),
                            serde_json::Value::from(contribution.sequence),
                        );
                    }
                    payload
                },
                refs: Vec::new(),
            };
            let visual = CanonicalToolVisual::from_artifact(&artifact);
            let CanonicalToolVisual::Plugin(plugin_visual) = &visual;
            let working_directory = invocation
                .as_deref()
                .and_then(|invocation| invocation.working_directory.as_deref());
            if let Some(routed) = resolve_canonical_plugin_visual(
                plugin_visual,
                working_directory,
                width,
                plugin_host,
            ) {
                match routed.render_mode {
                    PluginTuiVisualRenderMode::FullBlock => {
                        rows.extend(routed.rows);
                        rows.push(Line::default());
                    }
                    PluginTuiVisualRenderMode::TranscriptBlock => {
                        let mut timing = item.tool_timing();
                        if let Some(timeout_ms) = routed.header.timeout_ms {
                            timing.get_or_insert_default().timeout_ms = Some(timeout_ms);
                        }
                        push_tool_block_header(
                            rows,
                            routed
                                .header
                                .title
                                .as_deref()
                                .unwrap_or("Tool contribution"),
                            timing,
                            invocation.as_deref().map(|invocation| invocation.status),
                            invocation
                                .as_deref()
                                .and_then(|invocation| invocation.is_error)
                                .unwrap_or(false),
                            width,
                        );
                        rows.extend(routed.rows);
                        rows.push(Line::default());
                    }
                    PluginTuiVisualRenderMode::Inline => {
                        push_tool_block_header(
                            rows,
                            "Tool contribution",
                            item.tool_timing(),
                            None,
                            false,
                            width,
                        );
                        rows.extend(routed.rows);
                        rows.push(Line::default());
                    }
                }
            } else if matches!(
                placement,
                bcode_session_models::ToolContributionPlacement::Request
                    | bcode_session_models::ToolContributionPlacement::Progress
                    | bcode_session_models::ToolContributionPlacement::Result
            ) {
                push_tool_invocation_fallback_rows(rows, invocation.as_deref(), item, width);
            }
        }
        TranscriptItemKind::Interaction { interaction: _ } => {
            push_detail_block(
                rows,
                &item.display_role(),
                item.text(),
                semantic_state_theme().info,
                width,
            );
        }
        TranscriptItemKind::Generic => {
            push_detail_block(
                rows,
                &item.display_role(),
                item.text(),
                semantic_state_theme().transcript.detail_label,
                width,
            );
        }
    }
    apply_container_recipe(rows, item_start, layout);
}

fn push_assistant_rows(
    rows: &mut Vec<Line>,
    item: &TranscriptItem,
    width: u16,
    markdown: Option<&bcode_markdown_render::MarkdownRenderResult>,
) {
    let title = if item.streaming() {
        "Bcode …"
    } else {
        "Bcode"
    };
    let heading_style = if item.streaming() {
        semantic_state_theme().transcript.tool_running_title
    } else {
        semantic_state_theme().transcript.assistant_label
    };
    if let Some(markdown) = markdown {
        push_markdown_projection_block(rows, title, markdown, heading_style, width, true);
    } else if item.text_format() == TextFormat::Markdown {
        push_markdown_block_with_streaming(
            rows,
            title,
            item.text(),
            heading_style,
            width,
            true,
            item.streaming(),
        );
    } else {
        push_formatted_block(
            rows,
            title,
            item.text(),
            item.text_format(),
            heading_style,
            true,
            width,
        );
    }
}

fn push_formatted_block(
    rows: &mut Vec<Line>,
    title: &str,
    body: &str,
    text_format: TextFormat,
    heading_style: Style,
    prominent: bool,
    width: u16,
) {
    match text_format {
        TextFormat::Markdown => {
            push_markdown_block(rows, title, body, heading_style, width, prominent);
        }
        TextFormat::PlainText => push_block(rows, title, body, heading_style, prominent, width),
        TextFormat::Json => {
            let body = serde_json::from_str::<serde_json::Value>(body).map_or_else(
                |_| body.to_owned(),
                |value| serde_json::to_string_pretty(&value).unwrap_or_else(|_| body.to_owned()),
            );
            push_block(rows, title, &body, heading_style, prominent, width);
        }
    }
}

fn push_markdown_block(
    rows: &mut Vec<Line>,
    title: &str,
    body: &str,
    heading_style: Style,
    width: u16,
    prominent: bool,
) {
    push_markdown_block_with_streaming(rows, title, body, heading_style, width, prominent, false);
}

fn push_markdown_projection_block(
    rows: &mut Vec<Line>,
    title: &str,
    rendered: &bcode_markdown_render::MarkdownRenderResult,
    heading_style: Style,
    width: u16,
    prominent: bool,
) {
    let heading_style = if prominent {
        heading_style.add_modifier(Modifier::BOLD)
    } else {
        heading_style
    };
    push_wrapped_styled_text(rows, Vec::new(), title, width, heading_style, heading_style);
    if rendered.lines.is_empty() {
        rows.push(Line::from_spans(vec![
            Span::styled("  ", muted_style()),
            Span::styled(
                "·",
                if prominent {
                    Style::new()
                } else {
                    muted_style()
                },
            ),
        ]));
    } else {
        for line in &rendered.lines {
            let mut spans = vec![Span::styled(
                " ".repeat(usize::from(MARKDOWN_BODY_INDENT)),
                muted_style(),
            )];
            spans.extend(line.spans.iter().cloned());
            rows.push(Line::from_spans(spans));
        }
    }
    rows.push(Line::default());
}

fn push_markdown_block_with_streaming(
    rows: &mut Vec<Line>,
    title: &str,
    body: &str,
    heading_style: Style,
    width: u16,
    prominent: bool,
    streaming: bool,
) {
    let heading_style = if prominent {
        heading_style.add_modifier(Modifier::BOLD)
    } else {
        heading_style
    };
    push_wrapped_styled_text(rows, Vec::new(), title, width, heading_style, heading_style);

    if body.is_empty() {
        rows.push(Line::from_spans(vec![
            Span::styled("  ", muted_style()),
            Span::styled(
                "·",
                if prominent {
                    Style::new()
                } else {
                    muted_style()
                },
            ),
        ]));
    } else {
        for line in render_markdown_lines(
            body,
            MarkdownRenderOptions::new(width.saturating_sub(MARKDOWN_BODY_INDENT).max(1))
                .with_theme(semantic_state_theme().markdown)
                .with_syntax_palette(semantic_state_theme().syntax)
                .with_streaming(streaming)
                .with_details_open(markdown_details_open()),
        ) {
            let mut spans = vec![Span::styled(
                " ".repeat(usize::from(MARKDOWN_BODY_INDENT)),
                muted_style(),
            )];
            spans.extend(line.spans);
            rows.push(Line::from_spans(spans));
        }
    }

    rows.push(Line::default());
}

#[cfg(test)]
#[test]
#[ignore = "manual deterministic renderer parsing and layout baseline"]
fn markdown_and_json_layout_work_per_revision_baseline_report() {
    let markdown = "## Heading\n\n- one\n- two\n\n```rust\nfn main() {}\n```\n".repeat(64);
    let json = serde_json::json!({
        "items": (0..256).map(|index| serde_json::json!({"index": index, "value": "x".repeat(64)})).collect::<Vec<_>>()
    })
    .to_string();
    for (kind, body, format) in [
        ("markdown", markdown, TextFormat::Markdown),
        ("json", json, TextFormat::Json),
    ] {
        let started = Instant::now();
        let mut emitted_rows = 0_usize;
        let revisions = 100_usize;
        for revision in 0..revisions {
            let mut rows = Vec::new();
            push_formatted_block(
                &mut rows,
                kind,
                &format!("revision {revision}\n{body}"),
                format,
                Style::new().fg(Color::Green),
                false,
                100,
            );
            emitted_rows = emitted_rows.saturating_add(rows.len());
        }
        println!(
            "BCODE_PERF_CASE {}",
            serde_json::json!({
                "domain": "renderer_parse_layout",
                "format": kind,
                "revisions": revisions,
                "input_bytes_per_revision": body.len(),
                "emitted_rows": emitted_rows,
                "parse_layout_us": u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            })
        );
    }
}

#[cfg(test)]
#[test]
fn tui_markdown_message_block_indents_content_and_reserves_width() {
    let mut rows = Vec::new();
    push_markdown_block(
        &mut rows,
        "Bcode",
        "1234567890",
        Style::new().fg(Color::Green),
        8,
        true,
    );

    let text = rows
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>();

    assert_eq!(text, ["Bcode", "  123456", "  7890", ""]);
    assert!(text.iter().all(|line| line.chars().count() <= 8));
}

#[cfg(test)]
#[test]
fn tui_markdown_message_block_preserves_table_borders_after_indent() {
    let mut rows = Vec::new();
    push_markdown_block(
        &mut rows,
        "Bcode",
        "| A | B |\n|---|---|\n| 1 | 2 |",
        Style::new().fg(Color::Green),
        20,
        true,
    );

    let text = rows
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>();

    assert!(text.iter().any(|line| line.starts_with("  ┌")));
    assert!(text.iter().any(|line| line.starts_with("  │ A")));
    assert!(text.iter().all(|line| line.chars().count() <= 20));
}

#[cfg(test)]
fn visible_rows_snapshot(rows: &[Line]) -> String {
    rows.iter()
        .enumerate()
        .map(|(index, line)| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>();
            format!("{index:02} │ {text}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
#[test]
fn structured_transcript_recipe_changes_chrome_without_changing_body_text() {
    let catalog = super::theme::definition::ThemeCatalog::bundled().expect("bundled themes parse");
    let native = catalog
        .resolve(&super::theme::definition::ThemeSelection::new(
            "terminal-native",
        ))
        .expect("native resolves");
    let structured = catalog
        .resolve(&super::theme::definition::ThemeSelection::new(
            "terminal-native-structured",
        ))
        .expect("structured resolves");
    let native = super::theme::resolved_definition_theme(
        Some(&native),
        super::theme::PENDING_AGENT_METADATA_ACCENT,
    )
    .presented(super::theme::PENDING_AGENT_METADATA_ACCENT);
    let structured = super::theme::resolved_definition_theme(
        Some(&structured),
        super::theme::PENDING_AGENT_METADATA_ACCENT,
    )
    .presented(super::theme::PENDING_AGENT_METADATA_ACCENT);
    let item = TranscriptItem::with_format(
        "You",
        "same semantic body".to_owned(),
        TextFormat::PlainText,
    );

    set_plugin_visual_theme(&native);
    let native_rows =
        transcript_item_rows_from_item(&item, 40, None, TuiDiffViewerConfig::default());
    set_plugin_visual_theme(&structured);
    let structured_rows =
        transcript_item_rows_from_item(&item, 40, None, TuiDiffViewerConfig::default());

    let text = |rows: &[Line]| {
        rows.iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_str())
            .collect::<String>()
    };
    assert!(text(&native_rows).contains("same semantic body"));
    assert!(text(&structured_rows).contains("same semantic body"));
    assert!(!text(&native_rows).contains('│'));
    assert!(text(&structured_rows).contains('│'));
}

#[cfg(test)]
#[test]
fn structured_transcript_wraps_at_resolved_content_once() {
    let initial_theme = super::theme::resolve_initial_theme();
    let initial_theme = initial_theme.presented(initial_theme.accent);
    let catalog = super::theme::definition::ThemeCatalog::bundled().expect("bundled themes parse");
    let structured = catalog
        .resolve(&super::theme::definition::ThemeSelection::new(
            "terminal-native-structured",
        ))
        .expect("structured resolves");
    let structured = super::theme::resolved_definition_theme(
        Some(&structured),
        super::theme::PENDING_AGENT_METADATA_ACCENT,
    )
    .presented(super::theme::PENDING_AGENT_METADATA_ACCENT);
    set_plugin_visual_theme(&structured);

    for format in [TextFormat::PlainText, TextFormat::Markdown] {
        let item = TranscriptItem::with_format("You", "12345678901234567890".to_owned(), format);
        let rows = transcript_item_rows_from_item(&item, 20, None, TuiDiffViewerConfig::default());
        let lines = rows
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>();

        assert!(lines.iter().any(|line| line == "│   123456789012345"));
        assert!(lines.iter().any(|line| line == "│   67890"));
        assert!(!lines.iter().any(|line| line == "│ 678"));
    }
    set_plugin_visual_theme(&initial_theme);
}

#[cfg(test)]
#[test]
fn transcript_layout_derives_arbitrary_container_insets() {
    let catalog = super::theme::definition::ThemeCatalog::bundled().expect("bundled themes parse");
    let structured = catalog
        .resolve(&super::theme::definition::ThemeSelection::new(
            "terminal-native-structured",
        ))
        .expect("structured resolves");
    let mut structured = super::theme::resolved_definition_theme(
        Some(&structured),
        super::theme::PENDING_AGENT_METADATA_ACCENT,
    )
    .presented(super::theme::PENDING_AGENT_METADATA_ACCENT);
    structured.containers.user.recipe.padding_x = 3;
    let item = TranscriptItem::with_format("You", "body".to_owned(), TextFormat::Markdown);

    let layout = TranscriptItemLayout::resolve(&structured, &item, 20);

    assert_eq!(layout.content_width, 13);
    assert_eq!(layout.markdown_width(), 11);
    assert_eq!(layout.markdown_x(), 6);
    assert_eq!(
        plugin_visual_context(layout.content_width, None).width(),
        13
    );
}

#[cfg(test)]
#[test]
fn markdown_regions_follow_resolved_container_geometry() {
    let initial_theme = super::theme::resolve_initial_theme();
    let initial_theme = initial_theme.presented(initial_theme.accent);
    let catalog = super::theme::definition::ThemeCatalog::bundled().expect("bundled themes parse");
    let structured = catalog
        .resolve(&super::theme::definition::ThemeSelection::new(
            "terminal-native-structured",
        ))
        .expect("structured resolves");
    let mut structured = super::theme::resolved_definition_theme(
        Some(&structured),
        super::theme::PENDING_AGENT_METADATA_ACCENT,
    )
    .presented(super::theme::PENDING_AGENT_METADATA_ACCENT);
    structured.containers.user.recipe.border = super::theme::definition::ContainerBorder::All;
    structured.containers.user.recipe.padding_x = 3;
    structured.containers.user.recipe.padding_y = 2;
    set_plugin_visual_theme(&structured);
    let item = TranscriptItem::with_format(
        "You",
        "[guide](https://example.com)".to_owned(),
        TextFormat::Markdown,
    );
    let width = 20;
    let rows = transcript_item_rows_from_item(&item, width, None, TuiDiffViewerConfig::default());
    let (row, column) = rows
        .iter()
        .enumerate()
        .find_map(|(row, line)| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>();
            text.find("guide")
                .map(|byte| (row, text_display_width(&text[..byte])))
        })
        .expect("link is rendered");
    let area = Rect::new(3, 4, width, u16::try_from(rows.len()).unwrap_or(u16::MAX));
    let regions = markdown_hit_regions_for_item(&item, 0, 0, area);
    let region = regions.first().expect("link region");

    assert_eq!(
        region.area.x,
        area.x + u16::try_from(column).unwrap_or(u16::MAX)
    );
    assert_eq!(
        region.area.y,
        area.y + u16::try_from(row).unwrap_or(u16::MAX)
    );
    set_plugin_visual_theme(&initial_theme);
}

#[cfg(test)]
fn semantic_state_snapshot(theme_id: &str, rows: &[Line]) -> String {
    let lines = rows
        .iter()
        .map(|line| {
            let spans = line
                .spans
                .iter()
                .filter(|span| !span.content.is_empty())
                .map(|span| format!("{:?}:{:?}", span.content, span.style))
                .collect::<Vec<_>>()
                .join(" | ");
            format!("[{spans}]")
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("== {theme_id} ==\n{lines}")
}

#[cfg(test)]
#[allow(clippy::too_many_lines)] // Explicit transcript/tool fixture matrix.
fn semantic_state_matrix_items() -> Vec<TranscriptItem> {
    use bcode_session_view_models::ToolInvocationViewStatus;

    let mut items = vec![
        TranscriptItem::with_format("You", "user body".to_owned(), TextFormat::PlainText),
        TranscriptItem::with_format(
            "Bcode",
            "assistant **body**".to_owned(),
            TextFormat::Markdown,
        ),
        TranscriptItem::with_kind(
            "Reasoning",
            "reasoning body".to_owned(),
            false,
            TranscriptItemKind::ReasoningMessage,
        ),
        TranscriptItem::with_kind(
            "System",
            "system body".to_owned(),
            false,
            TranscriptItemKind::System,
        ),
        TranscriptItem::with_kind(
            "Meta",
            "metadata body".to_owned(),
            false,
            TranscriptItemKind::Meta,
        ),
        TranscriptItem::with_kind(
            "Skill",
            "skill body".to_owned(),
            false,
            TranscriptItemKind::Skill,
        ),
        TranscriptItem::with_kind(
            "Skill error",
            "skill failure".to_owned(),
            false,
            TranscriptItemKind::SkillError,
        ),
        TranscriptItem::with_kind(
            "Detail",
            "detail body".to_owned(),
            false,
            TranscriptItemKind::Generic,
        ),
        super::transcript::permission_request_item(
            "permission-1",
            "call-permission",
            "example.permission",
            r#"{"path":"src/lib.rs"}"#,
            Some("policy"),
            Some("side effect requires approval"),
        ),
    ];
    items.extend(
        [
            (ToolInvocationViewStatus::Requested, ToolTiming::default()),
            (ToolInvocationViewStatus::Running, ToolTiming::default()),
            (ToolInvocationViewStatus::Waiting, ToolTiming::default()),
            (ToolInvocationViewStatus::Finished, ToolTiming::default()),
            (ToolInvocationViewStatus::Failed, ToolTiming::default()),
            (ToolInvocationViewStatus::Cancelled, ToolTiming::default()),
            (
                ToolInvocationViewStatus::Failed,
                ToolTiming {
                    timed_out: Some(true),
                    ..ToolTiming::default()
                },
            ),
        ]
        .into_iter()
        .map(|(status, timing)| {
            TranscriptItem::with_kind(
                "Tool",
                "{}".to_owned(),
                false,
                TranscriptItemKind::ToolRequest {
                    tool_call_id: format!("call-{}", tool_status_label(status)),
                    producer_plugin_id: None,
                    tool_name: "example".to_owned(),
                    working_directory: None,
                    active: matches!(
                        status,
                        ToolInvocationViewStatus::Running | ToolInvocationViewStatus::Waiting
                    ),
                    status: Some(status),
                    timing,
                },
            )
        }),
    );
    items.push(super::transcript::tool_result_item(
        "call-result",
        Some("example"),
        Some(r#"{"path":"src/lib.rs"}"#),
        "first output row\nsecond output row",
        false,
    ));
    items.push(super::transcript::tool_result_item(
        "call-error",
        Some("example"),
        None,
        "failure output",
        true,
    ));
    items
}

#[cfg(test)]
const fn semantic_matrix_themes() -> [(
    &'static str,
    &'static str,
    super::theme::definition::ResolvedThemeVariant,
); 9] {
    use super::theme::definition::ResolvedThemeVariant::{Dark, Light, Unspecified};

    [
        ("terminal-native", "terminal-native", Unspecified),
        (
            "terminal-native-structured",
            "terminal-native-structured",
            Unspecified,
        ),
        ("bcode-dark", "bcode-dark", Unspecified),
        ("bcode-light", "bcode-light", Unspecified),
        ("bcode:auto-dark", "bcode", Dark),
        ("bcode:auto-light", "bcode", Light),
        ("nord", "nord", Unspecified),
        ("monochrome", "monochrome", Unspecified),
        ("high-contrast", "high-contrast", Unspecified),
    ]
}

#[cfg(test)]
#[test]
fn transcript_and_tool_state_matrix_snapshots_cross_theme_styles() {
    let catalog = super::theme::definition::ThemeCatalog::bundled().expect("bundled themes parse");
    let mut snapshot = String::new();
    for (snapshot_id, theme_id, variant) in semantic_matrix_themes() {
        let resolved = catalog
            .resolve(&super::theme::definition::ThemeSelection::new(theme_id).variant(variant))
            .unwrap_or_else(|error| panic!("{snapshot_id} resolves: {error}"));
        let theme = super::theme::resolved_definition_theme(
            Some(&resolved),
            super::theme::PENDING_AGENT_METADATA_ACCENT,
        )
        .presented(super::theme::PENDING_AGENT_METADATA_ACCENT);
        set_plugin_visual_theme(&theme);
        let rows = semantic_state_matrix_items()
            .iter()
            .flat_map(|item| {
                transcript_item_rows_from_item(item, 44, None, TuiDiffViewerConfig::default())
            })
            .collect::<Vec<_>>();
        if !snapshot.is_empty() {
            snapshot.push('\n');
        }
        snapshot.push_str(&semantic_state_snapshot(snapshot_id, &rows));
    }
    insta::assert_snapshot!("transcript_tool_state_matrix_cross_theme", snapshot);
}

#[cfg(test)]
#[test]
#[allow(clippy::too_many_lines)] // One explicit cross-theme semantic state matrix.
fn transcript_and_tool_state_matrix_is_bounded_across_bundled_themes() {
    use bcode_session_view_models::ToolInvocationViewStatus;

    let catalog = super::theme::definition::ThemeCatalog::bundled().expect("bundled themes parse");
    let width = 32;
    for (snapshot_id, theme_id, variant) in semantic_matrix_themes() {
        let resolved = catalog
            .resolve(&super::theme::definition::ThemeSelection::new(theme_id).variant(variant))
            .unwrap_or_else(|error| panic!("{snapshot_id} resolves: {error}"));
        let theme = super::theme::resolved_definition_theme(
            Some(&resolved),
            super::theme::PENDING_AGENT_METADATA_ACCENT,
        )
        .presented(super::theme::PENDING_AGENT_METADATA_ACCENT);
        set_plugin_visual_theme(&theme);

        let messages = [
            TranscriptItem::with_format("You", "user body".to_owned(), TextFormat::PlainText),
            TranscriptItem::with_format("Bcode", "assistant body".to_owned(), TextFormat::Markdown),
            TranscriptItem::with_kind(
                "Reasoning",
                "reasoning body".to_owned(),
                false,
                TranscriptItemKind::ReasoningMessage,
            ),
            TranscriptItem::with_kind(
                "System",
                "system body".to_owned(),
                false,
                TranscriptItemKind::System,
            ),
            TranscriptItem::with_kind(
                "Meta",
                "metadata body".to_owned(),
                false,
                TranscriptItemKind::Meta,
            ),
            TranscriptItem::with_kind(
                "Skill",
                "skill body".to_owned(),
                false,
                TranscriptItemKind::Skill,
            ),
            TranscriptItem::with_kind(
                "Skill error",
                "skill failure".to_owned(),
                false,
                TranscriptItemKind::SkillError,
            ),
            TranscriptItem::with_kind(
                "Detail",
                "detail body".to_owned(),
                false,
                TranscriptItemKind::Generic,
            ),
        ];
        for item in &messages {
            let rows =
                transcript_item_rows_from_item(item, width, None, TuiDiffViewerConfig::default());
            assert!(!rows.is_empty(), "{snapshot_id} omitted {:?}", item.kind());
            assert!(
                rows.iter()
                    .all(|line| spans_width(&line.spans) <= usize::from(width)),
                "{snapshot_id} overflowed {:?}",
                item.kind()
            );
        }

        for (status, timing) in [
            (ToolInvocationViewStatus::Requested, ToolTiming::default()),
            (ToolInvocationViewStatus::Running, ToolTiming::default()),
            (ToolInvocationViewStatus::Waiting, ToolTiming::default()),
            (ToolInvocationViewStatus::Finished, ToolTiming::default()),
            (ToolInvocationViewStatus::Failed, ToolTiming::default()),
            (ToolInvocationViewStatus::Cancelled, ToolTiming::default()),
            (
                ToolInvocationViewStatus::Failed,
                ToolTiming {
                    timed_out: Some(true),
                    ..ToolTiming::default()
                },
            ),
        ] {
            let item = TranscriptItem::with_kind(
                "Tool",
                "{}".to_owned(),
                false,
                TranscriptItemKind::ToolRequest {
                    tool_call_id: "call-1".to_owned(),
                    producer_plugin_id: None,
                    tool_name: "example".to_owned(),
                    working_directory: None,
                    active: matches!(
                        status,
                        ToolInvocationViewStatus::Running | ToolInvocationViewStatus::Waiting
                    ),
                    status: Some(status),
                    timing,
                },
            );
            let rows =
                transcript_item_rows_from_item(&item, width, None, TuiDiffViewerConfig::default());
            let visible = rows
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_str())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(visible.contains(tool_status_label(status)));
            assert!(
                rows.iter()
                    .all(|line| spans_width(&line.spans) <= usize::from(width))
            );
            if timing.timed_out == Some(true) {
                assert!(
                    visible.contains("tim") && visible.contains("out"),
                    "{snapshot_id} omitted timeout cue: {visible}"
                );
            }
        }
    }
}

#[cfg(test)]
#[test]
fn reduced_color_themes_retain_modifier_redundancy() {
    let catalog = super::theme::definition::ThemeCatalog::bundled().expect("bundled themes parse");
    for theme_id in ["monochrome", "high-contrast"] {
        let resolved = catalog
            .resolve(&super::theme::definition::ThemeSelection::new(theme_id))
            .unwrap_or_else(|error| panic!("{theme_id} resolves: {error}"));
        for role in [
            "border.focused",
            "selection.active",
            "state.info",
            "state.success",
            "state.warning",
            "state.error",
            "tool.requested.title",
            "tool.running.title",
            "tool.waiting.title",
            "tool.succeeded.title",
            "tool.failed.title",
            "tool.cancelled.title",
            "tool.timed_out.title",
            "diff.added",
            "diff.removed",
            "diff.hunk",
            "diff.added_emphasis",
            "diff.removed_emphasis",
        ] {
            let style = resolved
                .style(role)
                .unwrap_or_else(|| panic!("{theme_id} resolves {role}"));
            assert!(
                !style.modifiers.is_empty(),
                "{theme_id} {role} must retain a non-color cue"
            );
        }
    }
}

#[cfg(test)]
#[test]
fn monochrome_tool_states_retain_explicit_text_and_modifier_cues() {
    use bcode_session_view_models::ToolInvocationViewStatus;

    let catalog = super::theme::definition::ThemeCatalog::bundled().expect("bundled themes parse");
    let resolved = catalog
        .resolve(&super::theme::definition::ThemeSelection::new("monochrome"))
        .expect("monochrome resolves");
    let theme = super::theme::resolved_definition_theme(
        Some(&resolved),
        super::theme::PENDING_AGENT_METADATA_ACCENT,
    )
    .presented(super::theme::PENDING_AGENT_METADATA_ACCENT);
    set_plugin_visual_theme(&theme);

    for (status, timing) in [
        (ToolInvocationViewStatus::Requested, ToolTiming::default()),
        (ToolInvocationViewStatus::Running, ToolTiming::default()),
        (ToolInvocationViewStatus::Waiting, ToolTiming::default()),
        (ToolInvocationViewStatus::Finished, ToolTiming::default()),
        (ToolInvocationViewStatus::Failed, ToolTiming::default()),
        (ToolInvocationViewStatus::Cancelled, ToolTiming::default()),
        (
            ToolInvocationViewStatus::Failed,
            ToolTiming {
                timed_out: Some(true),
                ..ToolTiming::default()
            },
        ),
    ] {
        let item = TranscriptItem::with_kind(
            "Tool",
            "{}".to_owned(),
            false,
            TranscriptItemKind::ToolRequest {
                tool_call_id: "call-monochrome".to_owned(),
                producer_plugin_id: None,
                tool_name: "example".to_owned(),
                working_directory: None,
                active: matches!(
                    status,
                    ToolInvocationViewStatus::Running | ToolInvocationViewStatus::Waiting
                ),
                status: Some(status),
                timing,
            },
        );
        let rows = transcript_item_rows_from_item(&item, 44, None, TuiDiffViewerConfig::default());
        let visible = rows
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_str())
            .collect::<String>();
        let status_style_has_modifier = rows
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| {
                span.content.contains(tool_status_label(status))
                    || (timing.timed_out == Some(true)
                        && span.content.to_ascii_lowercase().contains("timed out"))
            })
            .any(|span| !span.style.modifiers.is_empty());

        if timing.timed_out == Some(true) {
            assert!(visible.to_ascii_lowercase().contains("timed out"));
        } else {
            assert!(visible.contains(tool_status_label(status)));
        }
        assert!(
            status_style_has_modifier,
            "monochrome {status:?} lacks a non-color modifier cue: {rows:?}"
        );
    }
}

#[cfg(test)]
#[test]
#[allow(clippy::too_many_lines)] // One explicit rich-content cross-theme regression matrix.
fn rich_content_matrix_is_bounded_and_semantic_across_themes() {
    use bcode_markdown_render::MarkdownRenderOptions;
    use bcode_tui_components::diff_viewer::{
        DiffViewerInput, DiffViewerLayout, diff_viewer_rows_with_style,
    };
    use bcode_tui_components::source_preview::{SourcePreviewOptions, source_preview_lines};
    use bcode_tui_components::terminal_viewer::{
        TerminalViewerInput, TerminalViewerSizing, terminal_viewer_rows,
    };

    let catalog = super::theme::definition::ThemeCatalog::bundled().expect("bundled themes parse");
    let code = (0..96)
        .map(|line| format!("let value_{line} = {line};"))
        .collect::<Vec<_>>()
        .join("\n");
    let markdown = format!(
        "# Streaming\n\n> [!WARNING]\n> themed alert\n\n| Name | Value |\n|:--|--:|\n| alpha | one |\n| beta | two |\n\n<details><summary>More</summary>detail body</details>\n\n```rust\n{code}"
    );
    let mut markdown_signatures = Vec::new();

    for theme_id in ["terminal-native", "bcode-dark", "bcode-light"] {
        let resolved = catalog
            .resolve(&super::theme::definition::ThemeSelection::new(theme_id))
            .unwrap_or_else(|error| panic!("{theme_id} resolves: {error}"));
        let theme = super::theme::resolved_definition_theme(
            Some(&resolved),
            super::theme::PENDING_AGENT_METADATA_ACCENT,
        )
        .presented(super::theme::PENDING_AGENT_METADATA_ACCENT);

        for width in [18, 80] {
            let rendered = render_markdown(
                &markdown,
                &MarkdownRenderOptions::new(width)
                    .with_theme(theme.markdown)
                    .with_syntax_palette(theme.syntax)
                    .with_streaming(true)
                    .with_document_id(format!("rich-{theme_id}-{width}")),
            );
            assert!(!rendered.lines.is_empty());
            assert!(
                rendered
                    .lines
                    .iter()
                    .all(|line| spans_width(&line.spans) <= usize::from(width)),
                "{theme_id} Markdown overflow at width {width}"
            );
            let visible = rendered
                .lines
                .iter()
                .flat_map(|line| &line.spans)
                .map(|span| span.content.as_str())
                .collect::<String>();
            assert!(visible.contains("Streaming"));
            assert!(visible.contains("WARNING"));
            assert!(visible.contains("More"));
            markdown_signatures.push((theme_id, width, rendered.layout_signature));
        }

        for layout in [DiffViewerLayout::Unified, DiffViewerLayout::SideBySide] {
            let rows = diff_viewer_rows_with_style(
                DiffViewerInput {
                    label: "src/lib.rs",
                    old_text: "fn old() {\n    let value = 1;\n}\n",
                    new_text: "fn new() {\n    let value = 2;\n}\n",
                    old_start_line: 1,
                    new_start_line: 1,
                    line_numbers_known: true,
                    title: "Updated",
                    subtitle: Some("semantic diff"),
                    argument_bytes: Some(128),
                    truncated: false,
                    syntax_palette: Some(theme.syntax),
                    layout,
                },
                72,
                theme.diff,
            );
            let visible = rows
                .iter()
                .flat_map(|line| &line.spans)
                .map(|span| span.content.as_str())
                .collect::<String>();
            assert!(visible.contains('+') && visible.contains('-'));
            assert!(rows.iter().all(|line| spans_width(&line.spans) <= 72));
        }

        let source = source_preview_lines(
            "fn one() {}\nfn two() {}\nfn three() {}",
            &SourcePreviewOptions::new("rust", 24)
                .syntax_palette(theme.syntax)
                .max_lines(2)
                .line_prefix("  │ ", theme.source.gutter)
                .source_style(theme.source.source)
                .truncated_message("  … preview truncated", theme.source.truncated),
        );
        assert!(source.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains("preview truncated"))
        }));
        assert!(source.iter().all(|line| spans_width(&line.spans) <= 24));
    }

    assert_ne!(markdown_signatures[2].2, markdown_signatures[4].2);

    let raw = terminal_viewer_rows(
        TerminalViewerInput {
            output: "\u{1b}[31mRED\u{1b}[0m plain",
            columns: 20,
            rows: 4,
            exit_code: Some(0),
            timed_out: Some(false),
            elapsed: None,
            show_status: false,
            output_truncated: false,
            output_bytes: None,
            retained_output_bytes: None,
            sizing: TerminalViewerSizing::Compact,
        },
        24,
    );
    assert!(
        raw.iter()
            .flat_map(|line| &line.spans)
            .any(|span| { span.content.contains("RED") && span.style.fg == Some(Color::Red) })
    );
}

#[cfg(test)]
#[test]
fn full_width_panel_fills_exactly_and_all_border_closes() {
    let catalog = super::theme::definition::ThemeCatalog::bundled().expect("bundled themes parse");
    let structured = catalog
        .resolve(&super::theme::definition::ThemeSelection::new(
            "terminal-native-structured",
        ))
        .expect("structured resolves");
    let structured = super::theme::resolved_definition_theme(
        Some(&structured),
        super::theme::PENDING_AGENT_METADATA_ACCENT,
    )
    .presented(super::theme::PENDING_AGENT_METADATA_ACCENT);
    set_plugin_visual_theme(&structured);

    let mut failed = super::transcript::tool_result_item(
        "call-1",
        Some("example"),
        Some(r#"{"path":"very/long/path"}"#),
        "failure output",
        true,
    );
    failed.set_tool_timed_out(Some(false));
    let rows = transcript_item_rows_from_item(&failed, 24, None, TuiDiffViewerConfig::default());

    assert!(rows.len() >= 5);
    assert_eq!(
        visible_rows_snapshot(&rows[..1]),
        "00 │ ┌──────────────────────┐"
    );
    assert_eq!(
        visible_rows_snapshot(&rows[rows.len() - 1..]),
        "00 │ └──────────────────────┘"
    );
    assert!(
        rows.iter()
            .all(|line| spans_width(&line.spans) == usize::from(24_u16))
    );
}

#[cfg(test)]
#[test]
fn structured_tool_recipes_cover_normalized_state_matrix() {
    use bcode_session_view_models::ToolInvocationViewStatus;

    let catalog = super::theme::definition::ThemeCatalog::bundled().expect("bundled themes parse");
    let structured = catalog
        .resolve(&super::theme::definition::ThemeSelection::new(
            "terminal-native-structured",
        ))
        .expect("structured resolves");
    let structured = super::theme::resolved_definition_theme(
        Some(&structured),
        super::theme::PENDING_AGENT_METADATA_ACCENT,
    )
    .presented(super::theme::PENDING_AGENT_METADATA_ACCENT);
    set_plugin_visual_theme(&structured);

    for (status, timing) in [
        (ToolInvocationViewStatus::Requested, ToolTiming::default()),
        (ToolInvocationViewStatus::Running, ToolTiming::default()),
        (ToolInvocationViewStatus::Waiting, ToolTiming::default()),
        (ToolInvocationViewStatus::Finished, ToolTiming::default()),
        (ToolInvocationViewStatus::Failed, ToolTiming::default()),
        (ToolInvocationViewStatus::Cancelled, ToolTiming::default()),
        (
            ToolInvocationViewStatus::Failed,
            ToolTiming {
                timed_out: Some(true),
                ..ToolTiming::default()
            },
        ),
    ] {
        let item = TranscriptItem::with_kind(
            "Tool",
            "{}".to_owned(),
            false,
            TranscriptItemKind::ToolRequest {
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: None,
                tool_name: "example".to_owned(),
                working_directory: None,
                active: matches!(
                    status,
                    ToolInvocationViewStatus::Running | ToolInvocationViewStatus::Waiting
                ),
                status: Some(status),
                timing,
            },
        );
        let rows = transcript_item_rows_from_item(&item, 40, None, TuiDiffViewerConfig::default());
        assert!(
            rows.iter()
                .flat_map(|line| line.spans.iter())
                .any(|span| span.content.contains('│')),
            "structured tool state {status:?} did not render container chrome"
        );
        assert!(rows.iter().all(|line| spans_width(&line.spans) <= 40));
    }
}

#[cfg(test)]
#[test]
fn snapshots_realistic_user_markdown_at_normal_and_constrained_widths() {
    const MARKDOWN: &str = "# Request\n\nPlease update `src/lib.rs` with **care**.\n\n1. Read the file.\n2. Run:\n\n   ```sh\n   cargo test\n   ```\n\n> Keep the change focused.\n\n| Check | State |\n| --- | --- |\n| tests | required |";
    let mut output = String::new();
    for width in [60_u16, 24] {
        let item = TranscriptItem::with_format("You", MARKDOWN.to_owned(), TextFormat::Markdown);
        let rows = transcript_item_rows(&[item], 0, width, None, TuiDiffViewerConfig::default());
        output.push_str("== width ");
        output.push_str(&width.to_string());
        output.push_str(" ==\n");
        output.push_str(&visible_rows_snapshot(&rows));
        output.push('\n');
    }
    insta::assert_snapshot!("user_markdown_width_60_24", output.trim_end());
}

#[cfg(test)]
#[test]
fn markdown_user_table_switches_to_stacked_layout_when_constrained() {
    let item = TranscriptItem::with_format(
        "You",
        "| Check | State |\n|---|---|\n| tests | required |".to_owned(),
        TextFormat::Markdown,
    );
    let rows = transcript_item_rows(&[item], 0, 18, None, TuiDiffViewerConfig::default());
    let text = visible_rows_snapshot(&rows);

    assert!(text.contains("Check: tests"));
    assert!(text.contains("State: required"));
    assert!(!text.contains('┌'));
}

#[cfg(test)]
#[test]
fn display_role_suffix_remains_plain_title_chrome() {
    let item = TranscriptItem::with_format("You", "# Body".to_owned(), TextFormat::Markdown)
        .with_display_label("Plugin **literal**".to_owned());
    let rows = transcript_item_rows(&[item], 0, 40, None, TuiDiffViewerConfig::default());
    let text = visible_rows_snapshot(&rows);

    assert!(text.contains("00 │ You · Plugin **literal**"));
    assert!(text.contains("01 │   Body"));
}

#[cfg(test)]
#[test]
fn reasoning_renders_completed_neutral_chrome_and_multipart_boundaries() {
    let item = TranscriptItem::with_identity(
        "Reasoning summary",
        "Designing invocation context\n\nRefactoring lifecycle metadata\n\nPlanning timing integration"
            .to_owned(),
        false,
        TextFormat::Markdown,
        TranscriptItemKind::ReasoningMessage,
    );
    let rows = transcript_item_rows(&[item], 0, 40, None, TuiDiffViewerConfig::default());
    let text = visible_rows_snapshot(&rows);

    assert!(text.contains("00 │ Reasoning"));
    assert!(!text.contains("Reasoning …"));
    assert!(text.contains("Designing invocation context"));
    assert!(text.contains("Refactoring lifecycle metadata"));
    assert!(text.contains("Planning timing integration"));
    assert!(!text.contains("contextRefactoring"));
    assert!(!text.contains("metadataPlanning"));
}

#[cfg(test)]
#[test]
fn assistant_and_reasoning_keep_markdown_body_and_streaming_chrome() {
    let assistant = TranscriptItem::with_identity(
        "Assistant",
        "**bold**".to_owned(),
        true,
        TextFormat::Markdown,
        TranscriptItemKind::AssistantMessage,
    );
    let reasoning = TranscriptItem::with_identity(
        "Reasoning summary",
        "- step".to_owned(),
        true,
        TextFormat::Markdown,
        TranscriptItemKind::ReasoningMessage,
    );
    let assistant_rows =
        transcript_item_rows(&[assistant], 0, 30, None, TuiDiffViewerConfig::default());
    let reasoning_rows =
        transcript_item_rows(&[reasoning], 0, 30, None, TuiDiffViewerConfig::default());
    let assistant_text = visible_rows_snapshot(&assistant_rows);
    let reasoning_text = visible_rows_snapshot(&reasoning_rows);

    assert!(assistant_text.contains("00 │ Bcode …"));
    assert!(assistant_text.contains("01 │   bold"));
    assert!(reasoning_text.contains("00 │ Reasoning …"));
    assert!(reasoning_text.contains("01 │   •  step"));
}

#[cfg(test)]
#[test]
fn shared_system_items_render_markdown_and_plain_text_distinctly() {
    let markdown =
        TranscriptItem::with_format("System", "* value".to_owned(), TextFormat::Markdown);
    let plain = TranscriptItem::with_format("Plugin", "* value".to_owned(), TextFormat::PlainText)
        .with_display_label("example".to_owned());
    let markdown_rows =
        transcript_item_rows(&[markdown], 0, 30, None, TuiDiffViewerConfig::default());
    let plain_rows = transcript_item_rows(&[plain], 0, 30, None, TuiDiffViewerConfig::default());

    assert!(visible_rows_snapshot(&markdown_rows).contains("  •  value"));
    let plain_text = visible_rows_snapshot(&plain_rows);
    assert!(plain_text.contains("00 │ Plugin · example"));
    assert!(plain_text.contains("01 │   * value"));
}

#[cfg(test)]
#[test]
fn skill_markdown_remains_xss_protected_when_rendered() {
    let markdown = super::slash_commands::format_skill_details_markdown(
        "Unsafe",
        "unsafe",
        "test",
        None,
        "<script>alert(1)</script>",
    );
    let item = TranscriptItem::with_format("System", markdown, TextFormat::Markdown);
    let rows = transcript_item_rows(&[item], 0, 80, None, TuiDiffViewerConfig::default());
    let text = visible_rows_snapshot(&rows);

    assert!(!text.contains("<script>"));
    assert!(text.contains("&amp;lt;script&amp;gt;"));
}

#[cfg(test)]
#[test]
fn snapshots_plain_and_json_user_messages() {
    let plain = TranscriptItem::with_format(
        "You",
        "* literal list\n# literal heading\n| literal | pipe |".to_owned(),
        TextFormat::PlainText,
    );
    let json = TranscriptItem::with_format(
        "You",
        r#"{"items":["one","two"]}"#.to_owned(),
        TextFormat::Json,
    );
    let plain_rows = transcript_item_rows(&[plain], 0, 30, None, TuiDiffViewerConfig::default());
    let json_rows = transcript_item_rows(&[json], 0, 30, None, TuiDiffViewerConfig::default());
    insta::assert_snapshot!(
        "user_plain_and_json_width_30",
        format!(
            "== plain ==\n{}\n== json ==\n{}",
            visible_rows_snapshot(&plain_rows),
            visible_rows_snapshot(&json_rows)
        )
    );
}

#[cfg(test)]
#[test]
fn tui_help_markdown_renders_at_normal_and_constrained_widths() {
    const HELP: &str = "# TUI help\n\n* Use the command palette for sessions, plugin commands, cancellation, and context compaction.\n* Transcript scrolling, composer history, session picker, and permissions honor configured keybindings where wired.\n* In permission dialogs, approve or deny directly, or move focus and confirm.";
    let mut snapshot = String::new();
    for width in [80_u16, 24] {
        let mut rows = Vec::new();
        push_markdown_block(
            &mut rows,
            "System",
            HELP,
            semantic_theme().muted,
            width,
            false,
        );
        let text = rows
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(text[0], "System");
        assert_eq!(text[1], "  TUI help");
        assert!(text.iter().any(|line| line.starts_with("  •  Use")));
        assert!(
            rows.iter()
                .all(|line| spans_width(&line.spans) <= usize::from(width))
        );
        snapshot.push_str("== width ");
        snapshot.push_str(&width.to_string());
        snapshot.push_str(" ==\n");
        snapshot.push_str(&visible_rows_snapshot(&rows));
        snapshot.push('\n');
    }
    insta::assert_snapshot!("tui_help_markdown_width_80_24", snapshot.trim_end());
}

#[cfg(test)]
#[test]
fn terminal_elapsed_signature_uses_typed_activity_not_streaming_flag() {
    let mut terminal = super::transcript::tool_result_item(
        "call-final",
        Some("example.run"),
        Some("{}"),
        "done",
        false,
    );
    terminal.streaming = true;
    terminal.set_tool_started_at_ms(Some(1_000));
    terminal.set_tool_duration_ms(Some(2_500));

    assert!(!terminal.tool_is_active());
    assert_eq!(terminal_elapsed_signature_fragment(&terminal), None);
}

#[cfg(test)]
#[test]
fn shared_tool_presentation_fixtures_render_semantic_content_in_tui() {
    for fixture in crate::renderer_fixtures::renderer_tool_presentation_fixtures() {
        let terminal = super::transcript::terminal_item_from_shared(&fixture.item);
        let rows = transcript_item_rows(&[terminal], 0, 100, None, TuiDiffViewerConfig::default());
        let rendered = visible_rows_snapshot(&rows);
        for expected in &fixture.expected {
            assert!(
                rendered.contains(expected),
                "{} missing {expected:?}: {rendered}",
                fixture.name
            );
        }
        for forbidden in &fixture.forbidden {
            assert!(
                !rendered.contains(forbidden),
                "{} exposed {forbidden:?}: {rendered}",
                fixture.name
            );
        }
        for revision in &fixture.revisions {
            let terminal = super::transcript::terminal_item_from_shared(revision);
            let rows =
                transcript_item_rows(&[terminal], 0, 100, None, TuiDiffViewerConfig::default());
            let revised = visible_rows_snapshot(&rows);
            let bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { tool } =
                &revision.kind
            else {
                unreachable!("shared fixture revisions are tool invocations");
            };
            assert!(revised.contains(tool.tool_name.as_deref().unwrap_or("Tool")));
            for forbidden in &fixture.forbidden {
                assert!(
                    !revised.contains(forbidden),
                    "{} exposed {forbidden:?}",
                    fixture.name
                );
            }
        }
        let bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { tool } =
            &fixture.item.kind
        else {
            unreachable!("shared fixtures are tool invocations");
        };
        assert!(rendered.contains(tool_status_label(tool.status)));
    }
}

#[cfg(test)]
#[test]
fn every_producer_family_renders_canonical_lifecycle_fallback_in_tui() {
    let producer_families = [
        "shell",
        "filesystem",
        "vim-edit",
        "document",
        "ocr",
        "web-search",
        "git",
        "worktree",
    ];
    let fixtures = crate::renderer_fixtures::renderer_tool_presentation_fixtures();
    for name in producer_families {
        let fixture = fixtures
            .iter()
            .find(|fixture| fixture.name == name)
            .unwrap_or_else(|| panic!("missing {name} producer fixture"));
        for status in [
            bcode_session_view_models::ToolInvocationViewStatus::Requested,
            bcode_session_view_models::ToolInvocationViewStatus::Running,
            bcode_session_view_models::ToolInvocationViewStatus::Waiting,
            bcode_session_view_models::ToolInvocationViewStatus::Finished,
            bcode_session_view_models::ToolInvocationViewStatus::Failed,
            bcode_session_view_models::ToolInvocationViewStatus::Cancelled,
        ] {
            let mut variant = fixture.item.clone();
            variant.streaming = matches!(
                status,
                bcode_session_view_models::ToolInvocationViewStatus::Running
                    | bcode_session_view_models::ToolInvocationViewStatus::Waiting
            );
            let bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { tool } =
                &mut variant.kind
            else {
                panic!("{name} fixture must be a tool invocation");
            };
            tool.status = status;
            tool.presentation = None;
            tool.result_text = matches!(
                status,
                bcode_session_view_models::ToolInvocationViewStatus::Finished
                    | bcode_session_view_models::ToolInvocationViewStatus::Failed
            )
            .then(|| format!("{name} terminal result"));
            tool.is_error = matches!(
                status,
                bcode_session_view_models::ToolInvocationViewStatus::Failed
            )
            .then_some(true);
            let tool_name = tool.tool_name.clone();
            let result_text = tool.result_text.clone();
            let terminal = super::transcript::terminal_item_from_shared(&variant);
            let rows =
                transcript_item_rows(&[terminal], 0, 100, None, TuiDiffViewerConfig::default());
            let rendered = visible_rows_snapshot(&rows);
            assert!(rendered.contains(tool_name.as_deref().unwrap_or("Tool")));
            let lifecycle_fragment = match status {
                bcode_session_view_models::ToolInvocationViewStatus::Finished => "ok",
                other => tool_status_label(other),
            };
            assert!(
                rendered.contains(lifecycle_fragment),
                "{name} {status:?} missing {lifecycle_fragment:?}: {rendered}"
            );
            if let Some(result) = &result_text {
                assert!(rendered.contains(result));
            }
        }
    }
}

#[cfg(test)]
#[test]
fn unsupported_tool_presentation_uses_semantic_fallback_without_opaque_payload() {
    let secret = "opaque-presentation-secret";
    let shared = bcode_session_view_models::TranscriptViewItem {
        output_location: None,
        id: bcode_session_view_models::TranscriptViewItemId::tool("call-fallback"),
        revision: 4,
        sequence: Some(1),
        timestamp_ms: Some(1),
        streaming: false,
        kind: bcode_session_view_models::TranscriptViewItemKind::ToolInvocation {
            tool: Box::new(bcode_session_view_models::ToolInvocationView {
                tool_call_id: "call-fallback".to_owned(),
                producer_plugin_id: Some("example.plugin".to_owned()),
                tool_name: Some("example.run".to_owned()),
                arguments_json: Some(r#"{"target":"fixture"}"#.to_owned()),
                working_directory: None,
                request_draft: None,
                status: bcode_session_view_models::ToolInvocationViewStatus::Finished,
                result_text: Some("semantic result".to_owned()),
                is_error: Some(false),
                result: None,
                presentation: Some(bcode_session_view_models::ToolPresentationView {
                    producer_id: "example.plugin".to_owned(),
                    generation: 0,
                    revision: 3,
                    retention: bcode_tool::ToolPresentationRetention::RetainLatest,
                    schema: "example.unsupported".to_owned(),
                    schema_version: 1,
                    artifact: None,
                    payload: serde_json::json!({"secret": secret}),
                }),
                timing: bcode_session_view_models::ToolTimingView::default(),
            }),
        },
    };
    let terminal = super::transcript::terminal_item_from_shared(&shared);
    let rows = transcript_item_rows(&[terminal], 0, 80, None, TuiDiffViewerConfig::default());
    let rendered = visible_rows_snapshot(&rows);

    assert!(rendered.contains("Tool · example.run · finished"));
    assert!(rendered.contains("semantic result"));
    assert!(rendered.contains("fixture"));
    assert!(!rendered.contains(secret));
    assert!(!rendered.contains("example.unsupported"));
}

#[cfg(test)]
#[test]
fn repeated_malformed_presentations_keep_bounded_semantic_fallback() {
    let secret = "opaque-malformed-presentation-secret";
    for revision in 1..=256 {
        let shared = bcode_session_view_models::TranscriptViewItem {
            output_location: None,
            id: bcode_session_view_models::TranscriptViewItemId::tool("call-malformed"),
            revision,
            sequence: Some(1),
            timestamp_ms: Some(1),
            streaming: true,
            kind: bcode_session_view_models::TranscriptViewItemKind::ToolInvocation {
                tool: Box::new(bcode_session_view_models::ToolInvocationView {
                    tool_call_id: "call-malformed".to_owned(),
                    producer_plugin_id: Some("bcode.shell".to_owned()),
                    tool_name: Some("shell.run".to_owned()),
                    arguments_json: Some(r#"{"command":"cargo test"}"#.to_owned()),
                    working_directory: None,
                    request_draft: None,
                    status: bcode_session_view_models::ToolInvocationViewStatus::Running,
                    result_text: None,
                    is_error: None,
                    result: None,
                    presentation: Some(bcode_session_view_models::ToolPresentationView {
                        producer_id: "bcode.shell".to_owned(),
                        generation: 0,
                        revision,
                        retention: bcode_tool::ToolPresentationRetention::RetainLatest,
                        schema: "bcode.tool.request.shell.run".to_owned(),
                        schema_version: 1,
                        artifact: None,
                        payload: serde_json::json!({"command": {"secret": secret}}),
                    }),
                    timing: bcode_session_view_models::ToolTimingView::default(),
                }),
            },
        };
        let terminal = super::transcript::terminal_item_from_shared(&shared);
        let rows = transcript_item_rows(&[terminal], 0, 80, None, TuiDiffViewerConfig::default());
        let rendered = visible_rows_snapshot(&rows);
        assert!(rendered.contains("shell.run"));
        assert!(rendered.contains("cargo test"));
        assert!(!rendered.contains(secret));
        assert!(rendered.len() < 1_024);
    }
}

#[cfg(test)]
#[test]
fn generic_tool_headers_render_elapsed_and_duration() {
    let now_ms = unix_time_millis(std::time::SystemTime::now());
    let mut request =
        super::transcript::tool_request_item("call-live", None, "example.run", "{}", None);
    request.set_tool_active(true);
    request.set_tool_started_at_ms(Some(now_ms.saturating_sub(2_000)));
    let request_rows =
        transcript_item_rows(&[request], 0, 80, None, TuiDiffViewerConfig::default());
    let request_text = visible_rows_snapshot(&request_rows);

    let mut result = super::transcript::tool_result_item(
        "call-final",
        Some("example.run"),
        Some("{}"),
        "done",
        false,
    );
    result.set_tool_started_at_ms(Some(1_000));
    result.set_tool_finished_at_ms(Some(3_500));
    let result_rows = transcript_item_rows(&[result], 0, 80, None, TuiDiffViewerConfig::default());
    let result_text = visible_rows_snapshot(&result_rows);

    assert!(request_text.contains("Tool · example.run · elapsed 2.0s"));
    assert!(result_text.contains("Tool result · example.run · ok · duration 2.5s"));
}

#[cfg(test)]
#[test]
fn pending_signature_captures_width_state_and_text() {
    let mut pending = PendingSubmission::new("# heading".to_owned());
    let sending = pending_submission_signature(&pending, 30);
    pending.mark_queued(Some(2));

    assert_ne!(sending, pending_submission_signature(&pending, 30));
    assert_ne!(sending, pending_submission_signature(&pending, 20));
    assert_ne!(
        sending,
        pending_submission_signature(&PendingSubmission::new("different".to_owned()), 30)
    );
}

#[cfg(test)]
#[test]
fn pending_and_finalized_user_markdown_share_body_layout() {
    let markdown = "- one\n- two with **wrapped emphasis that spans the available width**\n\n```text\nvalue\n```\n\n| A | B |\n|---|---|\n| 1 | 2 |";
    let pending = PendingSubmission::new(markdown.to_owned());
    let pending_rows = pending_submission_rows(&pending, 30);
    let finalized = TranscriptItem::with_format("You", markdown.to_owned(), TextFormat::Markdown);
    let finalized_rows =
        transcript_item_rows(&[finalized], 0, 30, None, TuiDiffViewerConfig::default());

    assert_eq!(&pending_rows[1..], &finalized_rows[1..]);
}

#[cfg(test)]
#[test]
fn transcript_layout_signature_changes_when_only_format_changes() {
    let plain = TranscriptItem::with_format("You", "* value".to_owned(), TextFormat::PlainText);
    let markdown = TranscriptItem::with_format("You", "* value".to_owned(), TextFormat::Markdown);

    assert_ne!(
        transcript_item_signature(&plain, 30, ()),
        transcript_item_signature(&markdown, 30, ())
    );
}

#[cfg(test)]
#[test]
fn transcript_layout_signature_does_not_copy_message_text() {
    let short = TranscriptItem::with_format("You", "short".to_owned(), TextFormat::Markdown);
    let long = TranscriptItem::with_format("You", "x".repeat(256 * 1024), TextFormat::Markdown);

    let short_signature = transcript_item_signature(&short, 80, ());
    let long_signature = transcript_item_signature(&long, 80, ());

    assert!(short_signature.as_str().len() < 128);
    assert!(long_signature.as_str().len() < 128);
}

#[cfg(test)]
#[test]
fn format_aware_blocks_respect_requested_width() {
    for format in [
        TextFormat::Markdown,
        TextFormat::PlainText,
        TextFormat::Json,
    ] {
        let mut rows = Vec::new();
        push_formatted_block(
            &mut rows,
            "Title",
            "1234567890 * [value] | more",
            format,
            Style::new().fg(Color::Blue),
            true,
            12,
        );
        assert!(rows.iter().all(|line| spans_width(&line.spans) <= 12));
    }
}

#[cfg(test)]
#[test]
fn format_aware_blocks_distinguish_markdown_plain_text_and_json() {
    let source = "* value";
    let render = |format| {
        let mut rows = Vec::new();
        push_formatted_block(
            &mut rows,
            "You",
            source,
            format,
            Style::new().fg(Color::Blue),
            true,
            30,
        );
        rows.into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
    };

    assert_eq!(render(TextFormat::Markdown), ["You", "  •  value", ""]);
    assert_eq!(render(TextFormat::PlainText), ["You", "  * value", ""]);
    assert_eq!(render(TextFormat::Json), ["You", "  * value", ""]);
}

#[cfg(test)]
#[test]
fn format_aware_json_pretty_prints_valid_values() {
    let mut rows = Vec::new();
    push_formatted_block(
        &mut rows,
        "Data",
        r#"{"value":[1,2]}"#,
        TextFormat::Json,
        Style::new().fg(Color::Blue),
        true,
        30,
    );
    let text = rows
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content)
                .collect::<String>()
        })
        .collect::<Vec<_>>();

    assert_eq!(
        text,
        [
            "Data",
            "  {",
            "    \"value\": [",
            "      1,",
            "      2",
            "    ]",
            "  }",
            ""
        ]
    );
}

fn push_reasoning_rows(
    rows: &mut Vec<Line>,
    item: &TranscriptItem,
    width: u16,
    markdown: Option<&bcode_markdown_render::MarkdownRenderResult>,
) {
    let title = if item.streaming() {
        "Reasoning …"
    } else {
        "Reasoning"
    };
    let style = semantic_state_theme().transcript.reasoning_label;
    if let Some(markdown) = markdown {
        push_markdown_projection_block(rows, title, markdown, style, width, false);
    } else if item.text_format() == TextFormat::Markdown {
        push_markdown_block_with_streaming(
            rows,
            title,
            item.text(),
            style,
            width,
            false,
            item.streaming(),
        );
    } else {
        push_formatted_block(
            rows,
            title,
            item.text(),
            item.text_format(),
            style,
            false,
            width,
        );
    }
}

#[derive(Clone, Copy)]
struct ToolRequestRenderContext<'a> {
    tool_name: &'a str,
    status: Option<bcode_session_view_models::ToolInvocationViewStatus>,
}

fn push_tool_request_rows(
    rows: &mut Vec<Line>,
    item: &TranscriptItem,
    context: &ToolRequestRenderContext<'_>,
    width: u16,
) {
    let title = context.status.map_or_else(
        || format!("Tool · {}", context.tool_name),
        |status| {
            format!(
                "Tool · {} · {}",
                context.tool_name,
                tool_status_label(status)
            )
        },
    );
    let status = context.status.or_else(|| {
        item.tool_is_active()
            .then_some(bcode_session_view_models::ToolInvocationViewStatus::Running)
    });
    push_tool_block_header(rows, &title, item.tool_timing(), status, false, width);
    rows.push(Line::default());
}

fn canonical_plugin_visual_available(
    visual: &CanonicalToolVisual,
    plugin_host: Option<&crate::plugin_tui::PluginTuiPresentation>,
) -> bool {
    let CanonicalToolVisual::Plugin(plugin_visual) = visual;
    let Some(presentation) = plugin_host else {
        return false;
    };
    presentation
        .visual_route(
            &plugin_visual.schema,
            plugin_visual.schema_version,
            plugin_visual.producer_plugin_id.as_deref(),
        )
        .is_some()
}

fn resolve_canonical_plugin_visual(
    visual: &CanonicalPluginVisual,
    working_directory: Option<&std::path::Path>,
    width: u16,
    plugin_host: Option<&crate::plugin_tui::PluginTuiPresentation>,
) -> Option<crate::plugin_tui::RoutedTuiVisual> {
    let presentation = plugin_host?;
    presentation.routed_visual(
        visual
            .invocation_id
            .as_deref()
            .unwrap_or("unknown-invocation"),
        visual.revision,
        &visual.schema,
        visual.schema_version,
        visual.producer_plugin_id.as_deref(),
        &visual.payload,
        &plugin_visual_context(width, working_directory),
    )
}

fn push_canonical_tool_visual_rows(
    rows: &mut Vec<Line>,
    visual: &CanonicalToolVisual,
    working_directory: Option<&std::path::Path>,
    width: u16,
    plugin_host: Option<&crate::plugin_tui::PluginTuiPresentation>,
) -> bool {
    let CanonicalToolVisual::Plugin(plugin_visual) = visual;
    push_canonical_plugin_visual_rows(rows, plugin_visual, working_directory, width, plugin_host)
}

fn push_canonical_plugin_visual_rows(
    rows: &mut Vec<Line>,
    visual: &CanonicalPluginVisual,
    working_directory: Option<&std::path::Path>,
    width: u16,
    plugin_host: Option<&crate::plugin_tui::PluginTuiPresentation>,
) -> bool {
    let Some(presentation) = plugin_host else {
        return false;
    };
    let visual_started = Instant::now();
    let routed = resolve_canonical_plugin_visual(visual, working_directory, width, plugin_host);
    if let Some(routed) = &routed {
        presentation.record_visual_timing(
            "render_rows",
            &routed.route.plugin_id,
            &routed.route.schema,
            visual_started,
        );
        rows.extend(routed.rows.clone());
        return true;
    }
    false
}

fn tool_title_style(
    status: Option<bcode_session_view_models::ToolInvocationViewStatus>,
    timing: Option<ToolTiming>,
    is_error: bool,
) -> Style {
    use bcode_session_view_models::ToolInvocationViewStatus;

    let theme = semantic_state_theme();
    if timing.is_some_and(|timing| timing.timed_out == Some(true)) {
        theme.transcript.tool_timed_out_title
    } else if is_error || matches!(status, Some(ToolInvocationViewStatus::Failed)) {
        theme.transcript.tool_failed_title
    } else {
        match status {
            Some(ToolInvocationViewStatus::Running) => theme.transcript.tool_running_title,
            Some(ToolInvocationViewStatus::Waiting) => theme.transcript.tool_waiting_title,
            Some(ToolInvocationViewStatus::Finished) => theme.transcript.tool_succeeded_title,
            Some(ToolInvocationViewStatus::Cancelled) => theme.transcript.tool_cancelled_title,
            Some(ToolInvocationViewStatus::Requested | ToolInvocationViewStatus::Failed) | None => {
                theme.transcript.tool_requested_title
            }
        }
    }
}

fn push_tool_block_header(
    rows: &mut Vec<Line>,
    title: &str,
    timing: Option<ToolTiming>,
    status: Option<bcode_session_view_models::ToolInvocationViewStatus>,
    is_error: bool,
    width: u16,
) {
    let theme = semantic_state_theme();
    let title_style = tool_title_style(status, timing, is_error);
    let title = tool_block_title_with_timing(
        title,
        timing,
        matches!(
            status,
            Some(
                bcode_session_view_models::ToolInvocationViewStatus::Running
                    | bcode_session_view_models::ToolInvocationViewStatus::Waiting
            )
        ),
    );
    push_wrapped_styled_text(
        rows,
        Vec::new(),
        &title,
        width,
        title_style,
        theme.transcript.tool_metadata,
    );
}

fn tool_block_title_with_timing(
    title: &str,
    timing: Option<ToolTiming>,
    streaming: bool,
) -> String {
    let Some(timing) = timing else {
        return title.to_owned();
    };
    let now_ms = unix_time_millis(std::time::SystemTime::now());
    let mut parts = Vec::new();
    if timing.timed_out == Some(true) {
        parts.push("timed out".to_owned());
    }
    if streaming && let Some(started_at_ms) = timing.started_at_ms {
        parts.push(format!(
            "elapsed {}",
            format_millis(now_ms.saturating_sub(started_at_ms))
        ));
    }
    if let Some(timeout_ms) = timing.timeout_ms
        && (streaming || timing.duration_ms.is_none() && timing.finished_at_ms.is_none())
    {
        parts.push(format!("timeout {}", format_millis(timeout_ms)));
    }
    if !streaming {
        if let Some(duration_ms) = timing.duration_ms {
            parts.push(format!("duration {}", format_millis(duration_ms)));
        } else if let (Some(started_at_ms), Some(finished_at_ms)) =
            (timing.started_at_ms, timing.finished_at_ms)
        {
            parts.push(format!(
                "duration {}",
                format_millis(finished_at_ms.saturating_sub(started_at_ms))
            ));
        }
    }
    if parts.is_empty() {
        title.to_owned()
    } else {
        format!("{title} · {}", parts.join(" · "))
    }
}

fn push_tool_invocation_fallback_rows(
    rows: &mut Vec<Line>,
    invocation: Option<&bcode_session_view_models::ToolInvocationView>,
    item: &TranscriptItem,
    width: u16,
) {
    let Some(invocation) = invocation else {
        push_meta_block(rows, item.text(), width);
        return;
    };
    let is_error = invocation.is_error == Some(true)
        || matches!(
            invocation.status,
            bcode_session_view_models::ToolInvocationViewStatus::Failed
        );
    let title = invocation.tool_name.as_deref().map_or_else(
        || format!("Tool · {}", tool_status_label(invocation.status)),
        |name| format!("Tool · {name} · {}", tool_status_label(invocation.status)),
    );
    push_tool_block_header(
        rows,
        &title,
        item.tool_timing(),
        Some(invocation.status),
        is_error,
        width,
    );
    if let Some(arguments) = invocation.arguments_json.as_deref() {
        for (label, value) in bounded_tool_argument_fields(arguments) {
            push_labeled_text_preview(rows, &label, &value, width, 2);
        }
    }
    match invocation.result.as_ref() {
        Some(bcode_session_view_models::ToolResultView::Text { text }) => {
            push_labeled_text_preview(rows, "output", text, width, MAX_INLINE_TOOL_TEXT_ROWS);
        }
        Some(bcode_session_view_models::ToolResultView::Json { value }) => {
            push_labeled_text_preview(rows, "result", value, width, MAX_INLINE_TOOL_TEXT_ROWS);
        }
        Some(bcode_session_view_models::ToolResultView::Artifact { artifact }) => {
            let title = artifact
                .artifact
                .title
                .as_deref()
                .unwrap_or("artifact result");
            push_labeled_text_preview(rows, "result", title, width, MAX_INLINE_TOOL_TEXT_ROWS);
            if let Some(result_text) = invocation.result_text.as_deref()
                && result_text != title
            {
                push_labeled_text_preview(
                    rows,
                    "output",
                    result_text,
                    width,
                    MAX_INLINE_TOOL_TEXT_ROWS,
                );
            }
        }
        None => {
            if let Some(result_text) = invocation.result_text.as_deref() {
                push_labeled_text_preview(
                    rows,
                    "output",
                    result_text,
                    width,
                    MAX_INLINE_TOOL_TEXT_ROWS,
                );
            }
        }
    }
    rows.push(Line::default());
}

const fn tool_status_label(
    status: bcode_session_view_models::ToolInvocationViewStatus,
) -> &'static str {
    match status {
        bcode_session_view_models::ToolInvocationViewStatus::Requested => "requested",
        bcode_session_view_models::ToolInvocationViewStatus::Running => "running",
        bcode_session_view_models::ToolInvocationViewStatus::Waiting => "waiting",
        bcode_session_view_models::ToolInvocationViewStatus::Finished => "finished",
        bcode_session_view_models::ToolInvocationViewStatus::Failed => "failed",
        bcode_session_view_models::ToolInvocationViewStatus::Cancelled => "cancelled",
    }
}

struct ToolResultRenderContext<'a> {
    tool_name: Option<&'a str>,
    result: &'a str,
    artifact: Option<&'a bcode_session_models::ToolArtifact>,
    working_directory: Option<&'a std::path::Path>,
    is_error: bool,
    has_file_preview: bool,
}

#[allow(clippy::too_many_lines)] // Composition branches preserve distinct host-owned tool chrome.
fn push_tool_result_rows(
    rows: &mut Vec<Line>,
    item: &TranscriptItem,
    context: &ToolResultRenderContext<'_>,
    width: u16,
    plugin_host: Option<&crate::plugin_tui::PluginTuiPresentation>,
) {
    if let Some(artifact) = context.artifact {
        let visual = CanonicalToolVisual::from_artifact(artifact);
        let CanonicalToolVisual::Plugin(plugin_visual) = &visual;
        if let Some(routed) = resolve_canonical_plugin_visual(
            plugin_visual,
            context.working_directory,
            width,
            plugin_host,
        ) {
            match routed.render_mode {
                PluginTuiVisualRenderMode::FullBlock => {
                    rows.extend(routed.rows);
                    rows.push(Line::default());
                }
                PluginTuiVisualRenderMode::TranscriptBlock => {
                    let mut timing = item.tool_timing();
                    if let Some(timeout_ms) = routed.header.timeout_ms {
                        timing.get_or_insert_default().timeout_ms = Some(timeout_ms);
                    }
                    push_tool_block_header(
                        rows,
                        routed
                            .header
                            .title
                            .as_deref()
                            .or(artifact.title.as_deref())
                            .unwrap_or("Tool result"),
                        timing,
                        Some(if context.is_error {
                            bcode_session_view_models::ToolInvocationViewStatus::Failed
                        } else {
                            bcode_session_view_models::ToolInvocationViewStatus::Finished
                        }),
                        context.is_error,
                        width,
                    );
                    rows.extend(routed.rows);
                    rows.push(Line::default());
                }
                PluginTuiVisualRenderMode::Inline => {
                    let status = if context.is_error { "failed" } else { "ok" };
                    let title = context.tool_name.map_or_else(
                        || format!("Tool result · {status}"),
                        |name| format!("Tool result · {name} · {status}"),
                    );
                    push_tool_block_header(
                        rows,
                        &title,
                        item.tool_timing(),
                        Some(if context.is_error {
                            bcode_session_view_models::ToolInvocationViewStatus::Failed
                        } else {
                            bcode_session_view_models::ToolInvocationViewStatus::Finished
                        }),
                        context.is_error,
                        width,
                    );
                    rows.extend(routed.rows);
                    rows.push(Line::default());
                }
            }
            return;
        }
    }
    let status = if context.is_error { "failed" } else { "ok" };
    let title = context.tool_name.map_or_else(
        || format!("Tool result · {status}"),
        |name| format!("Tool result · {name} · {status}"),
    );
    push_tool_block_header(
        rows,
        &title,
        item.tool_timing(),
        Some(if context.is_error {
            bcode_session_view_models::ToolInvocationViewStatus::Failed
        } else {
            bcode_session_view_models::ToolInvocationViewStatus::Finished
        }),
        context.is_error,
        width,
    );
    if let Some(artifact) = context.artifact {
        let visual = CanonicalToolVisual::from_artifact(artifact);
        if push_canonical_tool_visual_rows(
            rows,
            &visual,
            context.working_directory,
            width,
            plugin_host,
        ) {
            rows.push(Line::default());
            return;
        }
        let title = artifact.title.as_deref().unwrap_or("artifact result");
        push_labeled_text_preview(rows, "result", title, width, MAX_INLINE_TOOL_TEXT_ROWS);
        let result = context.result.trim();
        if !result.is_empty() && result != title && !result.contains(&artifact.schema) {
            push_labeled_text_preview(rows, "output", result, width, MAX_INLINE_TOOL_TEXT_ROWS);
        }
        rows.push(Line::default());
        return;
    }
    if context.has_file_preview && !context.result.trim().is_empty() {
        let theme = semantic_state_theme();
        push_wrapped_styled_text(
            rows,
            vec![Span::styled("  ", theme.transcript.tool_metadata)],
            &format!("confirmation: {}", context.result.trim()),
            width,
            theme.transcript.tool_output,
            theme.transcript.tool_metadata,
        );
    } else {
        push_labeled_text_preview(
            rows,
            "output",
            item.text(),
            width,
            MAX_INLINE_TOOL_TEXT_ROWS,
        );
    }
    if context.is_error {
        let theme = semantic_state_theme();
        push_wrapped_styled_text(
            rows,
            vec![Span::styled("  ", theme.transcript.tool_metadata)],
            "tool failed",
            width,
            theme.transcript.tool_failed_title,
            theme.transcript.tool_metadata,
        );
    }
    rows.push(Line::default());
}

fn bounded_tool_argument_fields(arguments: &str) -> Vec<(String, String)> {
    const MAX_FIELDS: usize = 6;
    const MAX_VALUE_CHARS: usize = 160;
    let Ok(serde_json::Value::Object(fields)) = serde_json::from_str(arguments) else {
        return Vec::new();
    };
    fields
        .into_iter()
        .take(MAX_FIELDS)
        .filter_map(|(label, value)| {
            let value = match value {
                serde_json::Value::String(value) => value,
                serde_json::Value::Number(value) => value.to_string(),
                serde_json::Value::Bool(value) => value.to_string(),
                serde_json::Value::Null => "none".to_owned(),
                serde_json::Value::Array(values) => format!("{} values", values.len()),
                serde_json::Value::Object(values) => format!("{} fields", values.len()),
            };
            let mut bounded = value.chars().take(MAX_VALUE_CHARS).collect::<String>();
            if value.chars().count() > MAX_VALUE_CHARS {
                bounded.push('…');
            }
            (!bounded.is_empty()).then_some((label, bounded))
        })
        .collect()
}

fn push_labeled_text_preview(
    rows: &mut Vec<Line>,
    label: &str,
    text: &str,
    width: u16,
    max_rows: usize,
) {
    if text.is_empty() {
        return;
    }
    let theme = semantic_state_theme();
    push_wrapped_styled_text(
        rows,
        vec![Span::styled("  ", theme.transcript.tool_metadata)],
        label,
        width,
        theme.transcript.tool_label,
        theme.transcript.tool_metadata,
    );
    let body_style = if matches!(label, "output" | "result") {
        theme.transcript.tool_output
    } else {
        theme.transcript.tool_argument
    };
    let lines = text
        .lines()
        .map(|line| Line::from_spans(vec![Span::styled(line, body_style)]))
        .collect::<Vec<_>>();
    let total = lines.len();
    for line in preview_lines(&lines, max_rows) {
        rows.push(prefix_line(
            line.clone(),
            "    ",
            theme.transcript.tool_metadata,
        ));
    }
    if total > max_rows {
        push_wrapped_styled_text(
            rows,
            vec![Span::styled("    ", theme.transcript.tool_metadata)],
            &format!("… {} {label} rows hidden …", total - max_rows),
            width,
            theme.transcript.tool_truncation,
            theme.transcript.tool_truncation,
        );
    }
}

fn preview_lines(lines: &[Line], max_rows: usize) -> Vec<&Line> {
    lines
        .iter()
        .skip(lines.len().saturating_sub(max_rows))
        .collect()
}

fn prefix_line(mut line: Line, prefix: &str, prefix_style: Style) -> Line {
    let mut spans = vec![Span::styled(prefix.to_owned(), prefix_style)];
    spans.append(&mut line.spans);
    Line::from_spans(spans)
}

fn push_permission_request_rows(
    rows: &mut Vec<Line>,
    item: &TranscriptItem,
    permission_id: &str,
    tool_call_id: &str,
    tool_name: &str,
    width: u16,
) {
    let body = format!(
        "permission {}\ntool call {}\narguments:\n{}",
        permission_id,
        tool_call_id,
        item.text()
    );
    push_detail_block(
        rows,
        &format!("Permission required · {tool_name}"),
        &body,
        semantic_state_theme().error,
        width,
    );
}

fn push_pending_submission_rows(rows: &mut Vec<Line>, pending: &PendingSubmission, width: u16) {
    if matches!(pending.state(), PendingSubmissionState::Sent) {
        return;
    }
    let title = format!("You · {}", pending_label(pending.state()));
    push_formatted_block(
        rows,
        &title,
        pending.text(),
        TextFormat::Markdown,
        semantic_state_theme().transcript.pending_label,
        true,
        width,
    );
}

fn push_detail_block(rows: &mut Vec<Line>, title: &str, body: &str, style: Style, width: u16) {
    push_block_with_body_style(
        rows,
        title,
        body,
        style,
        semantic_state_theme().transcript.detail_body,
        width,
    );
}

fn push_meta_block(rows: &mut Vec<Line>, text: &str, width: u16) {
    push_wrapped_styled_text(
        rows,
        vec![Span::styled("· ", semantic_state_theme().transcript.meta)],
        text,
        width,
        semantic_state_theme().transcript.meta,
        semantic_state_theme().transcript.meta,
    );
}

fn push_block(
    rows: &mut Vec<Line>,
    title: &str,
    body: &str,
    heading_style: Style,
    prominent: bool,
    width: u16,
) {
    let heading_style = if prominent {
        heading_style.add_modifier(Modifier::BOLD)
    } else {
        heading_style
    };
    let body_style = if prominent {
        Style::new()
    } else {
        muted_style()
    };
    push_block_with_body_style(rows, title, body, heading_style, body_style, width);
}

fn push_block_with_body_style(
    rows: &mut Vec<Line>,
    title: &str,
    body: &str,
    heading_style: Style,
    body_style: Style,
    width: u16,
) {
    let continuation_style = semantic_state_theme().transcript.detail_body;
    push_wrapped_styled_text(rows, Vec::new(), title, width, heading_style, heading_style);
    if body.is_empty() {
        rows.push(Line::from_spans(vec![
            Span::styled("  ", continuation_style),
            Span::styled("·", body_style),
        ]));
    } else {
        for line in body.lines() {
            push_wrapped_styled_text(
                rows,
                vec![Span::styled("  ", continuation_style)],
                line,
                width,
                body_style,
                continuation_style,
            );
        }
    }
    rows.push(Line::default());
}

fn push_wrapped_styled_text(
    rows: &mut Vec<Line>,
    prefix: Vec<Span>,
    text: &str,
    width: u16,
    body_style: Style,
    continuation_style: Style,
) {
    let max_width = usize::from(width.max(1));
    let prefix_width = spans_width(&prefix);
    let available_first = max_width.saturating_sub(prefix_width).max(1);
    let available_next = max_width.saturating_sub(2).max(1);
    let continuation_prefix = Span::styled("  ", continuation_style);

    let chunks = wrap_text_with_continuation(text, available_first, available_next);
    for (chunk_index, chunk) in chunks.iter().enumerate() {
        if chunk_index == 0 {
            let mut spans = prefix.clone();
            spans.push(Span::styled(chunk.clone(), body_style));
            rows.push(Line::from_spans(spans));
        } else {
            rows.push(Line::from_spans(vec![
                continuation_prefix.clone(),
                Span::styled(chunk.clone(), body_style),
            ]));
        }
    }

    if chunks.is_empty() {
        rows.push(Line::from_spans(prefix));
    }
}

fn wrap_text_with_continuation(
    text: &str,
    first_width: usize,
    continuation_width: usize,
) -> Vec<String> {
    bmux_tui::text_width::wrap_text_with_continuation(text, first_width, continuation_width)
}

fn spans_width(spans: &[Span]) -> usize {
    spans
        .iter()
        .map(|span| text_display_width(&span.content))
        .sum()
}

fn pending_label(state: PendingSubmissionState) -> String {
    match state {
        PendingSubmissionState::Sending => "sending".to_owned(),
        PendingSubmissionState::Sent => "sent".to_owned(),
        PendingSubmissionState::Queued { queue_position } => queue_position.map_or_else(
            || "queued".to_owned(),
            |position| format!("queued #{position}"),
        ),
    }
}

fn render_status(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>, theme: TuiTheme) {
    if area.is_empty() {
        return;
    }

    let spans = statusline_spans(app, usize::from(area.width), theme);
    frame.write_line(area, &Line::from_spans(spans));
}

use bcode_tui_components::chrome::ChromeLine;

fn statusline_spans(app: &BmuxApp, width: usize, theme: TuiTheme) -> Vec<Span> {
    let muted = theme.muted;
    let mut line = ChromeLine::new(" · ", muted).required(
        activity_label(
            app.activity(),
            app.activity_started_at(),
            app.daemon_connection(),
        ),
        theme.info,
        true,
    );

    let mut token_segments = compact_statusline_token_segments(&app.token_summary()).into_iter();
    if let Some((context_segment, _)) = token_segments.next() {
        line = line.required(context_segment, muted, false);
    }
    for (token_segment, priority) in token_segments {
        line = line.optional(token_segment, muted, priority, false);
    }

    for contribution in app.plugin_status() {
        line = line.optional(
            contribution.text.clone(),
            theme.info,
            u8::try_from(contribution.priority).unwrap_or(u8::MAX - 1),
            true,
        );
    }

    let status_text = statusline_status_text(app);
    if !status_text.is_empty() {
        line = line.optional(status_text, muted, 90, true);
    }
    if app.scroll_offset() > 0 {
        line = line.optional(
            format!("{} rows from bottom", app.scroll_offset()),
            muted,
            100,
            false,
        );
    } else if app.bottom_overscroll() > 0 {
        line = line.optional(
            format!("{} rows below latest", app.bottom_overscroll()),
            muted,
            100,
            false,
        );
    }

    let key_hints = compact_key_hints(app.key_hints());
    if !key_hints.is_empty() {
        line = line.optional(key_hints, muted, 10, false);
    }

    line.spans(width)
}

pub(crate) fn statusline_status_text(app: &BmuxApp) -> String {
    match app.execution_mode_indicator() {
        Some(indicator) if app.status().is_empty() => indicator.to_owned(),
        Some(indicator) => format!("{indicator} · {}", app.status()),
        None => app.status().to_owned(),
    }
}

fn compact_statusline_token_segments(summary: &str) -> Vec<(String, u8)> {
    summary
        .split(" · ")
        .filter_map(|part| match part {
            "reuse on" => Some(("reuse".to_owned(), 50)),
            _ if part.ends_with('%') && part.contains('/') => Some((part.to_owned(), 95)),
            _ => part
                .strip_prefix("spent ")
                .and_then(|value| value.strip_suffix(" tok"))
                .map(|value| (format!("spent {value}"), 30))
                .or_else(|| {
                    part.strip_prefix("cache read ")
                        .and_then(|value| value.strip_suffix(" tok"))
                        .map(|value| (format!("read {value}"), 45))
                })
                .or_else(|| {
                    part.strip_prefix("cache write ")
                        .and_then(|value| value.strip_suffix(" tok"))
                        .map(|value| (format!("write {value}"), 45))
                })
                .or_else(|| {
                    part.strip_prefix("sent ")
                        .and_then(|value| value.strip_suffix(" msgs"))
                        .map(|value| (format!("sent {value}"), 40))
                })
                .or_else(|| {
                    part.strip_prefix("cache points ")
                        .map(|value| (format!("pts {value}"), 40))
                })
                .or_else(|| Some((part.to_owned(), 35))),
        })
        .collect()
}

fn compact_key_hints(hints: &str) -> String {
    hints
        .replace("escape", "esc")
        .replace("ctrl+", "^")
        .replace("palette", "pal")
}

fn activity_label(
    activity: &ActivityState,
    started_at: std::time::Instant,
    daemon_connection: DaemonConnectionState,
) -> String {
    let elapsed = format_activity_elapsed(started_at.elapsed());
    let active = |label: String| format!("{} {label} · {elapsed}", spinner_frame());
    match activity {
        ActivityState::Idle => match daemon_connection {
            DaemonConnectionState::Connecting => format!("{} connecting…", spinner_frame()),
            DaemonConnectionState::Starting => format!("{} starting daemon…", spinner_frame()),
            DaemonConnectionState::Connected | DaemonConnectionState::IdleOffline => {
                "ready".to_owned()
            }
            DaemonConnectionState::Unavailable => "daemon unavailable".to_owned(),
        },
        ActivityState::PreparingModelRequest => active("preparing model request".to_owned()),
        ActivityState::StartingProviderRequest { provider, round } => active(format!(
            "starting {provider} request{}",
            format_round(*round)
        )),
        ActivityState::WaitingForProvider { provider, round } => active(format!(
            "waiting for {provider} response{}",
            format_round(*round)
        )),
        ActivityState::PreparingToolExecution { name } => {
            active(format!("preparing tool execution · {name}"))
        }
        ActivityState::PreparingFollowUpRequest => {
            active("preparing follow-up model request".to_owned())
        }
        ActivityState::FinalizingModelTurn => active("finalizing model turn".to_owned()),
        ActivityState::RuntimeWork { detail } | ActivityState::ProviderStream { detail } => {
            active(detail.clone())
        }
        ActivityState::Compacting { detail } => active(format!("compacting · {detail}")),
        ActivityState::Streaming { chars } => {
            active(format!("receiving model output · {chars} chars"))
        }
        ActivityState::RetryWait {
            message,
            retry_at_unix,
        } => format!(
            "{} {message}; retrying in {} · Esc to cancel",
            spinner_frame(),
            format_retry_remaining(*retry_at_unix)
        ),
        ActivityState::RunningTool { name } => active(tool_activity_label(name)),
        ActivityState::WaitingPermission { name } => active(format!(
            "waiting for permission · {}",
            tool_activity_label(name)
        )),
        ActivityState::Cancelling => active("cancelling".to_owned()),
    }
}

fn format_round(round: Option<u32>) -> String {
    round.map_or_else(String::new, |round| format!(" · round {round}"))
}

fn format_activity_elapsed(elapsed: std::time::Duration) -> String {
    let millis = elapsed.as_millis();
    if millis < 1_000 {
        format!("{millis}ms")
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
}

fn format_retry_remaining(retry_at_unix: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let seconds = retry_at_unix.saturating_sub(now);
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600).div_ceil(60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        "less than 1m".to_owned()
    }
}

fn tool_activity_label(tool_name: &str) -> String {
    format!("tool {tool_name}")
}

fn spinner_frame() -> &'static str {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let index = usize::try_from((elapsed / 100) % SPINNER_FRAMES.len() as u128).unwrap_or(0);
    SPINNER_FRAMES[index]
}

fn render_composer(app: &mut BmuxApp, area: Rect, frame: &mut Frame<'_>, theme: TuiTheme) {
    if area.is_empty() {
        return;
    }
    let panel = composer_panel(theme);
    panel.render(area, frame);
    let inner = panel.inner_area(area);
    app.set_composer_content_area(inner);
    frame.push_hit(
        HitRegion::new("composer", inner)
            .role(HitRole::TextInput)
            .layer(1),
    );
    TextInput::new(app.composer())
        .placeholder("Ask Bcode…")
        .placeholder_style(theme.muted)
        .style(theme.text)
        .selection_style(theme.selection)
        .vertical_scroll(app.composer_scroll_offset_for_render())
        .cursor_visible(app.cursor_visible())
        .render(inner, frame);
    if !app.cursor_visible() {
        frame.set_cursor(bmux_tui::frame::Cursor::hidden(
            bmux_tui::geometry::Point::new(inner.x, inner.y),
        ));
    }
}

fn muted_style() -> Style {
    semantic_theme().muted
}
