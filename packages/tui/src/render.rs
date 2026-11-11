//! Host-shell chrome rendering: header, status row, composer, child monitor and the frame
//! layout the transcript engine paints its region into.

use std::cell::{Cell, RefCell};

#[cfg(test)]
mod chrome_effects_tests;

thread_local! {
    static CHROME_THEME: Cell<Option<super::theme::PresentedTheme>> = const { Cell::new(None) };
    static FOCUS_PHASE: Cell<u16> = const { Cell::new(0) };
    static FOCUS_GRADIENT_PAINTED: Cell<bool> = const { Cell::new(false) };
}

/// Synchronize the presented theme that dialog, component and composer chrome paint with.
pub fn set_chrome_theme(theme: &super::theme::PresentedTheme) {
    CHROME_THEME.with(|state| state.set(Some(*theme)));
}

/// Border glyphs and focus effects from the active theme, for bordered BMUX components.
#[must_use]
pub fn component_chrome() -> bmux_tui_components::theme::ComponentChrome {
    with_focus_phase(semantic_state_theme().component_theme().chrome)
}

/// Border glyphs and focus effects from the active theme, for dialogs and modals.
#[must_use]
pub fn dialog_chrome() -> bmux_tui_components::theme::ComponentChrome {
    with_focus_phase(semantic_state_theme().dialog_chrome)
}

/// Synchronize the focus gradient phase that bordered components paint with.
pub fn set_focus_phase(phase: u16) {
    FOCUS_PHASE.with(|state| state.set(phase));
}

/// Start a frame: synchronize the focus `phase` for every painter (including
/// foregrounds that own the whole canvas) and, on a `full` repaint, forget
/// whether a focus gradient was painted, since everything visible repaints.
/// Partial frames keep the previous answer because their overlays stay on
/// screen unrepainted.
pub fn begin_focus_frame(phase: u16, full: bool) {
    set_focus_phase(phase);
    if full {
        FOCUS_GRADIENT_PAINTED.with(|state| state.set(false));
    }
}

/// Whether chrome carrying a focus gradient was requested since the last
/// full frame, i.e. whether a sweeping focus border can be on screen.
#[must_use]
pub fn focus_gradient_painted() -> bool {
    FOCUS_GRADIENT_PAINTED.with(Cell::get)
}

/// `chrome` with its focus gradient shifted to the synchronized sweep phase.
fn with_focus_phase(
    mut chrome: bmux_tui_components::theme::ComponentChrome,
) -> bmux_tui_components::theme::ComponentChrome {
    if chrome.focus_gradient.is_some() {
        FOCUS_GRADIENT_PAINTED.with(|state| state.set(true));
    }
    let phase = FOCUS_PHASE.with(Cell::get);
    if phase != 0 {
        chrome.focus_gradient = chrome
            .focus_gradient
            .map(|gradient| gradient.with_phase(phase));
    }
    chrome
}

pub fn semantic_state_theme() -> super::theme::PresentedTheme {
    CHROME_THEME.with(|state| {
        state.get().unwrap_or_else(|| {
            let initial = super::theme::resolve_initial_theme();
            initial.presented(initial.accent)
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
            heading: syntax_color(syntax.heading),
            link: syntax_color(syntax.link),
            raw: syntax_color(syntax.raw),
        },
        component: components,
    }
}

use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::composition::TextBlock;
#[cfg(test)]
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};
#[cfg(test)]
use bmux_tui::style::Color;
use bmux_tui::style::Modifier;

use super::activity::ActivityState;
use super::app::{BmuxApp, DaemonConnectionState, composer_policy};

static SPINNER: std::sync::LazyLock<bmux_tui_components::spinner::Spinner> =
    std::sync::LazyLock::new(bmux_tui_components::spinner::Spinner::braille);
const MAX_COMPOSER_ROWS: u16 = 6;
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
    /// The theme's `borders.composer` chrome, which the composer panel frames itself with.
    pub composer_border: Option<bcode_tui_theme::chrome::BorderStyle>,
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
        Self::from_presented(&app.presented_theme())
    }

    /// Adapt semantic theme roles for pre-session screens.
    #[must_use]
    pub const fn from_presented(theme: &super::theme::PresentedTheme) -> Self {
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
            composer_border: theme.unit_chrome.composer_border,
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
            composer_border: theme.presented(theme.accent).unit_chrome.composer_border,
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
            composer_border: theme.unit_chrome.composer_border,
        }
    }
}

/// Prepared geometry for one TUI frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout {
    area: Rect,
    header: Rect,
    pub(crate) body: Rect,
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
    let status_height = queue_status_height(app, area.width)
        .min(composer_y.saturating_sub(area.y.saturating_add(1)));
    let body_height =
        composer_y.saturating_sub(area.y.saturating_add(1).saturating_add(status_height));
    let body = Rect::new(area.x, area.y.saturating_add(1), area.width, body_height);
    transcript_area_for_body(app, body)
}

/// Prepare derived frame layout before rendering.
#[cfg(test)]
pub fn prepare_frame(app: &mut BmuxApp, area: Rect) -> Option<FrameLayout> {
    prepare_chrome_with_bottom_dock(app, area, 0).map(|(layout, _dock)| layout)
}

/// Prepare the shared chat chrome layout (header, body, status, composer and an optional
/// bottom dock). The transcript engine lays out and paints the transcript region itself.
pub fn prepare_chrome_with_bottom_dock(
    app: &mut BmuxApp,
    area: Rect,
    dock_height: u16,
) -> Option<(FrameLayout, Rect)> {
    if area.is_empty() {
        app.queue_recovery.settings_visible = false;
    }
    let layout = frame_layout(app, area)?;
    app.set_composer_content_area(layout.composer_content);
    Some(layout.with_bottom_dock(dock_height))
}

/// Render one frame of shell chrome (the transcript region is the transcript engine's).
#[cfg(test)]
pub fn render(app: &mut BmuxApp, frame: &mut PaintCx<'_, '_>) {
    if let Some(layout) = prepare_frame(
        app,
        Rect::new(0, 0, frame.area().width, frame.area().height),
    ) {
        render_prepared_damage_with(app, frame, layout, FrameChrome::default(), |_| true);
    }
}

/// Paint child monitor using the public terminal acceptance interface.
fn paint_child_monitor(
    app: &BmuxApp,
    frame: &mut PaintCx<'_, '_>,
    layout: FrameLayout,
    intersects: impl Fn(Rect) -> bool,
) {
    use super::monitor_presentation::{MonitorAction, MonitorStyles};
    let area = Rect::new(
        layout.area.x,
        layout.header.bottom(),
        layout.area.width,
        layout.body.y.saturating_sub(layout.header.bottom()),
    );
    if area.is_empty() || !intersects(area) {
        return;
    }
    let theme = TuiTheme::for_app(app);
    let styles = MonitorStyles {
        text: theme.text,
        muted: theme.muted,
        border: theme.border,
        focused: theme.focused,
        raised: super::monitor_presentation::raised_surface(
            theme.raised,
            app.presented_theme().canvas,
            theme.text,
        ),
        selection: theme.selection,
        info: theme.info,
        warning: theme.warning,
        error: theme.error,
    };
    for hit in super::monitor_presentation::paint_with_context(
        &app.monitor_presentation(),
        &app.monitor_context(),
        area,
        app.child_panel_focused(),
        styles,
        frame,
    ) {
        let layer = if matches!(hit.action, MonitorAction::ToggleExpansion(_)) {
            3
        } else {
            2
        };
        frame.push_hit(
            HitRegion::new(hit.key(), hit.rect)
                .role(HitRole::Action)
                .layer(layer),
        );
    }
}

/// What the shell chrome reports about the transcript engine that paints the body. The host
/// frame path passes it explicitly, so chrome never reads a transcript engine's state itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameChrome {
    /// Reading position the status row reports (C2).
    pub transcript_position: TranscriptStatusPosition,
}

/// Transcript reading position the status row reports (C2): how far the reader is above the
/// content bottom ("N rows from bottom"), or how many blank tail-space rows are below the latest
/// content ("N rows below latest"). At most one is non-zero; both are zero while following.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct TranscriptStatusPosition {
    /// Rows between the viewport bottom and the content bottom.
    pub rows_from_bottom: u64,
    /// Blank tail-space rows below the latest content row.
    pub rows_below_latest: u64,
}

impl TranscriptStatusPosition {
    /// The optional status-row label for the position, if any.
    #[must_use]
    pub fn label(self) -> Option<String> {
        if self.rows_from_bottom > 0 {
            Some(format!("{} rows from bottom", self.rows_from_bottom))
        } else if self.rows_below_latest > 0 {
            Some(format!("{} rows below latest", self.rows_below_latest))
        } else {
            None
        }
    }
}

/// Render the shell chrome regions selected by terminal-space damage, with the chrome inputs of
/// the transcript engine that paints the body.
pub fn render_prepared_damage_with(
    app: &mut BmuxApp,
    frame: &mut PaintCx<'_, '_>,
    layout: FrameLayout,
    chrome: FrameChrome,
    intersects: impl Fn(Rect) -> bool,
) {
    if layout.area.is_empty() {
        app.queue_recovery.settings_visible = false;
        return;
    }

    set_focus_phase(app.focus_phase());
    let theme = TuiTheme::for_app(app);
    paint_child_monitor(app, frame, layout, &intersects);
    if intersects(layout.header) {
        render_header_damage(app, layout.header, frame, theme);
    }
    if intersects(layout.composer) {
        frame.fill(
            LocalRect::terminal(layout.composer),
            " ",
            app.presented_theme().canvas,
        );
        render_composer(app, layout.composer, frame, theme);
    }
    if intersects(layout.status) {
        frame.fill(
            LocalRect::terminal(layout.status),
            " ",
            app.presented_theme().canvas,
        );
        render_status(app, layout.status, frame, theme, chrome.transcript_position);
    }
    if let Some(confirmation) = &app.queue_recovery.confirmation {
        let text = format!(
            "{}\n\nType /queue-confirm to explicitly confirm these settings.\nType /queue-dismiss to return without resuming.",
            bcode_session_view_models::submission_resume_settings(confirmation)
        );
        let component = bcode_tui_components::composer::composer_panel(
            bcode_tui_components::composer::ComposerStyle {
                border: theme.warning,
                surface: theme.raised,
            },
            TextBlock::new(text),
        );
        let area = layout.body;
        let natural = component.layout(
            Constraints::loose(bmux_tui::geometry::Size::new(area.width, u16::MAX)),
            &mut LayoutCx::new(),
        );
        app.queue_recovery.settings_visible = area.width > 0
            && natural.size.width <= u64::from(area.width)
            && natural.size.height <= u64::from(area.height);
        let measured = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
        frame.with_child(
            i32::from(area.x),
            i64::from(area.y),
            LocalRect::new(0, 0, area.width, area.height),
            |cx| component.paint(&measured, cx),
        );
    }
    let enabled = !app.composer().text().trim().is_empty();
    super::composer_submit::paint_menu(
        &mut app.composer_submit,
        layout.area,
        enabled,
        frame,
        theme,
    );
}

fn render_header_damage(app: &BmuxApp, area: Rect, frame: &mut PaintCx<'_, '_>, theme: TuiTheme) {
    frame.fill(LocalRect::terminal(area), " ", app.presented_theme().canvas);
    render_header(app, area, frame, theme);
}

impl FrameLayout {
    fn with_bottom_dock(self, requested_height: u16) -> (Self, Rect) {
        let full_body = self.body;
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
        let transcript_height = full_body.height.saturating_sub(dock_height);
        let body = Rect::new(full_body.x, full_body.y, full_body.width, transcript_height);
        (Self { body, ..self }, dock)
    }
}

fn frame_layout(app: &BmuxApp, area: Rect) -> Option<FrameLayout> {
    if area.is_empty() {
        return None;
    }

    let header = Rect::new(area.x, area.y, area.width, 1);
    let composer_height = composer_height(app, area);
    let composer = composer_area(area, composer_height);
    let status_height = queue_status_height(app, area.width)
        .min(composer.y.saturating_sub(area.y.saturating_add(1)));
    let body_height = composer
        .y
        .saturating_sub(area.y.saturating_add(1).saturating_add(status_height));
    let body = Rect::new(area.x, area.y.saturating_add(1), area.width, body_height);
    let panel_height = if app.child_panel_open() {
        let presentation = app.monitor_presentation();
        u16::try_from(
            presentation
                .rows
                .len()
                .saturating_add(8)
                .saturating_add(usize::from(presentation.load_more)),
        )
        .unwrap_or(u16::MAX)
        .min(body.height / 2)
    } else {
        0
    };
    let body = Rect::new(
        body.x,
        body.y.saturating_add(panel_height),
        body.width,
        body.height.saturating_sub(panel_height),
    );
    let status = Rect::new(
        area.x,
        composer.y.saturating_sub(status_height),
        area.width,
        status_height,
    );
    Some(FrameLayout {
        area,
        header,
        body,
        status,
        composer,
        composer_content: composer_content_area(composer, TuiTheme::for_app(app)),
    })
}

#[cfg(test)]
#[test]
fn queue_resume_settings_require_complete_presentation_and_survive_resize() {
    use bcode_session_models::*;
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    app.queue_recovery.confirmation = Some(SubmissionResumeConfirmation {
        version: 1,
        submission_id: SubmissionId("pending".to_owned()),
        expected_accepted_sequence: 1,
        descriptor: SubmissionExecutionDescriptor {
            version: 2,
            provider: "provider-界".to_owned(),
            model: "model-👩‍💻-e\u{301}".to_owned(),
            model_profile: Some("selected profile".to_owned()),
            auth_profile: Some("account".to_owned()),
            reasoning: None,
            config_identity: "digest".to_owned(),
            recovery_recipe: None,
        },
    });
    for (width, height, visible) in [
        (120, 40, true),
        (8, 6, false),
        (0, 0, false),
        (80, 30, true),
    ] {
        app.queue_recovery.settings_visible = false;
        let area = Rect::new(0, 0, width, height);
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        render(&mut app, &mut PaintCx::new(&mut frame));
        assert_eq!(app.queue_recovery.settings_visible, visible);
    }
    drop(app);
}

#[cfg(test)]
#[test]
fn bottom_dock_never_overlaps_transcript_and_preserves_one_row() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    let terminal = Rect::new(0, 0, 80, 20);
    let normal = prepare_frame(&mut app, terminal).expect("normal frame layout");
    let normal_body_height = normal.body.height;
    let (layout, dock) =
        prepare_chrome_with_bottom_dock(&mut app, terminal, 100).expect("frame layout");
    let transcript = layout.body;
    drop(app);

    assert_eq!(dock.height, normal_body_height.saturating_sub(1));
    assert_eq!(transcript.height, 1);
    assert!(transcript.bottom() <= dock.y);
    assert_eq!(transcript.width, dock.width);
}

#[cfg(test)]
#[test]
fn resolved_canvas_fills_the_normal_frame_without_opaque_terminal_native_fallback() {
    let area = Rect::new(0, 0, 48, 14);

    let mut opaque = BmuxApp::new_with_history(None, &[], &[]);
    assert!(opaque.apply_theme("bcode-dark"));
    let mut opaque_buffer = bmux_tui::buffer::Buffer::empty(area);
    let mut opaque_frame = Frame::new(&mut opaque_buffer);
    render(
        &mut opaque,
        &mut bmux_tui::paint::PaintCx::new(&mut opaque_frame),
    );
    drop(opaque);
    assert_eq!(
        opaque_buffer
            .get(bmux_tui::geometry::Point::new(47, 0))
            .map(|cell| cell.style.bg),
        Some(Some(Color::Rgb(11, 16, 32)))
    );

    let mut native = BmuxApp::new_with_history(None, &[], &[]);
    assert!(native.apply_theme("terminal-native"));
    let mut native_buffer = bmux_tui::buffer::Buffer::empty(area);
    let mut native_frame = Frame::new(&mut native_buffer);
    render(
        &mut native,
        &mut bmux_tui::paint::PaintCx::new(&mut native_frame),
    );
    drop(native);
    assert!(
        native_buffer
            .get(bmux_tui::geometry::Point::new(47, 0))
            .is_some_and(|cell| cell.style.bg.is_none_or(|color| color == Color::Default))
    );
}

#[cfg(test)]
#[test]
fn bottom_dock_handles_short_narrow_and_resized_terminals() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    for terminal in [
        Rect::new(0, 0, 8, 6),
        Rect::new(0, 0, 24, 8),
        Rect::new(0, 0, 120, 40),
        Rect::new(0, 0, 18, 7),
    ] {
        let (layout, dock) = prepare_chrome_with_bottom_dock(&mut app, terminal, 100)
            .expect("constrained frame layout");
        let transcript = layout.body;
        assert!(transcript.bottom() <= dock.y);
        assert!(dock.right() <= terminal.right());
        assert!(dock.bottom() <= terminal.bottom());
        assert!(transcript.height >= 1 || layout.body.height == 0);
    }
    drop(app);
}

#[cfg(test)]
#[test]
fn zero_height_bottom_dock_preserves_normal_layout() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    let terminal = Rect::new(0, 0, 80, 20);
    let normal = prepare_frame(&mut app, terminal).expect("normal frame layout");
    let (docked, dock) =
        prepare_chrome_with_bottom_dock(&mut app, terminal, 0).expect("docked frame layout");
    drop(app);

    assert_eq!(normal, docked);
    assert_eq!(dock.height, 0);
}

fn composer_height(app: &BmuxApp, area: Rect) -> u16 {
    if area.height == 0 {
        return 0;
    }
    let content_width = area.width.saturating_sub(4).max(1);
    let rows = app.composer_state().rows_with_trailing_exclusion(
        content_width,
        super::composer_submit::trailing_exclusion(area.size()),
        MAX_COMPOSER_ROWS,
    );
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

fn composer_content_area(area: Rect, theme: TuiTheme) -> Rect {
    let state = RefCell::new(bmux_tui_components::text_input::TextInputState::default());
    let policy = composer_policy();
    let submit = super::composer_submit::ComposerSubmit::default();
    let component = super::composer_submit::panel(
        &state,
        &policy,
        &submit,
        super::composer_submit::ComposerPresentation {
            theme,
            focused: false,
            enabled: false,
            size: area.size(),
        },
        super::composer_submit::DEFAULT_PLACEHOLDER,
    );
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    let child = layout
        .find_logical_rect(&bmux_tui::component::LayoutId::new("composer.editor"))
        .expect("composer owns its editor");
    Rect::new(
        area.x
            .saturating_add(u16::try_from(child.x).unwrap_or(u16::MAX)),
        area.y
            .saturating_add(u16::try_from(child.y).unwrap_or(u16::MAX)),
        u16::try_from(child.width).unwrap_or(u16::MAX),
        u16::try_from(child.height).unwrap_or(u16::MAX),
    )
}

fn render_header(app: &BmuxApp, area: Rect, frame: &mut PaintCx<'_, '_>, theme: TuiTheme) {
    if area.is_empty() {
        return;
    }

    let line = Line::from_spans(header_spans(app, usize::from(area.width), theme));
    frame.write_line(LocalRect::terminal(area), &line);
}

fn header_spans(app: &BmuxApp, width: usize, theme: TuiTheme) -> Vec<Span> {
    let muted = theme.muted;
    let accent = theme.focused;
    let session_title = app
        .display_session_title()
        .unwrap_or_else(|| "Untitled session".to_owned());
    let mut line = ChromeLine::new(" · ", muted)
        .required(
            "bcode".to_owned(),
            theme.focused.add_modifier(Modifier::BOLD),
            false,
        )
        .required(
            app.managed_label()
                .unwrap_or_else(|| app.display_agent_id())
                .to_owned(),
            accent,
            false,
        )
        .required(app.model_header_label(), Style::new(), false)
        .required(session_title, Style::new(), true);
    // A managed conversation runs on its native executor; Bcode provider routing is unused.
    if app.managed_label().is_none() {
        line = line.optional(
            format!(
                "provider {}",
                app.selected_provider_plugin_id().unwrap_or("auto")
            ),
            accent,
            50,
            false,
        );
    }

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

/// The transcript region of a frame body.
pub const fn transcript_area_for_body(_app: &BmuxApp, area: Rect) -> Rect {
    area
}

fn render_status(
    app: &BmuxApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
    position: TranscriptStatusPosition,
) {
    if area.is_empty() {
        return;
    }

    if let Some(text) = queue_status_text(app) {
        let component = TextBlock::new(text).style(theme.warning);
        let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
        frame.with_child(
            i32::from(area.x),
            i64::from(area.y),
            LocalRect::new(0, 0, area.width, area.height),
            |cx| component.paint(&layout, cx),
        );
        return;
    }
    let spans = statusline_spans(app, usize::from(area.width), theme, position);
    frame.write_line(LocalRect::terminal(area), &Line::from_spans(spans));
}

fn queue_status_height(app: &BmuxApp, width: u16) -> u16 {
    queue_status_text(app).map_or(1, |text| {
        let node = TextBlock::new(text).layout(
            Constraints::loose(bmux_tui::geometry::Size::new(width, u16::MAX)),
            &mut LayoutCx::new(),
        );
        u16::try_from(node.size.height).unwrap_or(u16::MAX)
    })
}

fn queue_status_text(app: &BmuxApp) -> Option<String> {
    if app
        .session_view_snapshot()
        .runtime
        .submission_queue_unavailable
    {
        return Some("Queued message status unavailable. Retrying bounded status reads; reconnect to retry if it remains unavailable.".to_owned());
    }
    let recovery = app.session_view_snapshot().runtime.queue_recovery()?;
    let controls = if recovery.interrupted_turn_id.is_some() {
        "\n/queue-end-interrupted: end this turn without replaying tools (prior effects remain)"
    } else if recovery.can_resume {
        "\n/queue-resume: review settings · /queue-cancel: cancel this item"
    } else if recovery.can_cancel {
        "\n/queue-cancel: cancel this item"
    } else {
        ""
    };
    Some(format!(
        "Queued message needs attention: {}{}",
        recovery.reason, controls
    ))
}

use bcode_tui_components::chrome::ChromeLine;

fn statusline_spans(
    app: &BmuxApp,
    width: usize,
    theme: TuiTheme,
    position: TranscriptStatusPosition,
) -> Vec<Span> {
    let muted = theme.muted;
    let separator = app.presented_theme().effects.status_separator();
    if let Some(recovery) = app.session_view_snapshot().runtime.queue_recovery() {
        let controls = if recovery.interrupted_turn_id.is_some() {
            " /queue-end-interrupted"
        } else if recovery.can_resume {
            " /queue-resume · /queue-cancel"
        } else if recovery.can_cancel {
            " /queue-cancel"
        } else {
            ""
        };
        return ChromeLine::new(separator, muted)
            .required(
                format!("{}{}", recovery.reason, controls),
                theme.error,
                true,
            )
            .spans(width);
    }
    let mut line = ChromeLine::new(separator, muted).required(
        activity_label(
            app.activity(),
            app.activity_elapsed(),
            app.daemon_connection(),
            app.spinner_step(),
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

    let child_activity = app.child_activity_text();
    if !child_activity.is_empty() {
        line = line.optional(child_activity, theme.info, 20, true);
    }
    let persistent_activity =
        bcode_session_view::presentation::persistent_activity_text(app.session_view_snapshot());
    if let Some(text) = &persistent_activity {
        line = line.required(text.clone(), theme.info, true);
    }
    for contribution in app.plugin_status() {
        if persistent_activity.is_some() && contribution.metadata.contains_key("run_id") {
            continue;
        }
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
    if let Some(label) = position.label() {
        line = line.optional(label, muted, 100, false);
    }

    let key_hints = compact_key_hints(app.key_hints());
    if !key_hints.is_empty() {
        line = line.optional(key_hints, muted, 10, false);
    }

    line.spans(width)
}

fn statusline_status_text(app: &BmuxApp) -> String {
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
    elapsed: std::time::Duration,
    daemon_connection: DaemonConnectionState,
    activity_frame: usize,
) -> String {
    let elapsed = format_activity_elapsed(elapsed);
    let active = |label: String| format!("{} {label} · {elapsed}", spinner_frame(activity_frame));
    match activity {
        ActivityState::Idle => match daemon_connection {
            DaemonConnectionState::Connecting => {
                format!("{} connecting…", spinner_frame(activity_frame))
            }
            DaemonConnectionState::Starting => {
                format!("{} starting daemon…", spinner_frame(activity_frame))
            }
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
            "{} {message}; retrying in {} · cancel to exit",
            spinner_frame(activity_frame),
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

/// The spinner frame for the `activity_frame`th activity tick.
fn spinner_frame(activity_frame: usize) -> &'static str {
    // Frames repeat, so only the position within a cycle matters.
    let step = u32::try_from(activity_frame % 1_000).unwrap_or_default();
    SPINNER.frame(SPINNER.interval() * step)
}

fn render_composer(app: &mut BmuxApp, area: Rect, frame: &mut PaintCx<'_, '_>, theme: TuiTheme) {
    if area.is_empty() {
        return;
    }
    let state = RefCell::new(app.composer_state().clone());
    let policy = composer_policy();
    let placeholder = app.managed_label().map_or_else(
        || super::composer_submit::DEFAULT_PLACEHOLDER.to_owned(),
        |label| format!("Ask {label}…"),
    );
    let panel = super::composer_submit::panel(
        &state,
        &policy,
        &app.composer_submit,
        super::composer_submit::ComposerPresentation {
            theme,
            focused: app.cursor_visible(),
            enabled: !app.composer().text().trim().is_empty(),
            size: area.size(),
        },
        &placeholder,
    );
    let layout = panel.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    app.composer_submit.record_layout(&layout, area);
    frame.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| panel.paint(&layout, cx),
    );
    drop(panel);
    *app.composer_state_mut() = state.into_inner();
    let inner = composer_content_area(area, theme);
    app.set_composer_content_area(inner);
    let exclusion = super::composer_submit::trailing_exclusion(area.size());
    for region in [
        Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(1),
        ),
        Rect::new(
            inner.x,
            inner.bottom().saturating_sub(1),
            inner.width.saturating_sub(exclusion),
            inner.height.min(1),
        ),
    ] {
        frame.push_hit(
            HitRegion::new("composer", region)
                .role(HitRole::TextInput)
                .layer(1),
        );
    }
    if !app.cursor_visible() {
        frame.set_cursor(bmux_tui::geometry::Point::new(inner.x, inner.y), false);
    }
}

#[cfg(test)]
mod header_tests {
    use super::*;

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|span| span.content.as_str()).collect()
    }

    #[test]
    fn full_frame_contains_build_version_in_header() {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 160, 24));
        render(
            &mut app,
            &mut bmux_tui::paint::PaintCx::new(&mut Frame::new(&mut buffer)),
        );
        let header = buffer.row_symbols(0).expect("header row");
        assert!(header.contains(super::super::build_info().display_version()));
    }

    #[test]
    fn header_shows_build_version_when_space_is_available() {
        let app = BmuxApp::new_with_history(None, &[], &[]);
        let rendered = text(&header_spans(&app, 200, TuiTheme::for_app(&app)));
        drop(app);
        assert!(rendered.contains("bcode"));
        assert!(rendered.contains(super::super::build_info().display_version()));
    }

    #[test]
    fn header_omits_build_version_before_required_context() {
        let app = BmuxApp::new_with_history(None, &[], &[]);
        let rendered = text(&header_spans(&app, 50, TuiTheme::for_app(&app)));
        assert!(rendered.contains("bcode"));
        assert!(rendered.contains(app.display_agent_id()));
        drop(app);
        assert!(!rendered.contains(super::super::build_info().display_version()));
        assert!(!rendered.ends_with(" · "));
    }
}
