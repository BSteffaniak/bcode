//! Workflow graph/status TUI surface.

use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiAction, PluginTuiHost, PluginTuiRegistry, PluginTuiSurface,
    PluginTuiSurfaceFactory, PluginTuiSurfaceFuture, PluginTuiSurfaceOpenRequest,
    PluginTuiSurfaceUpdate, PluginTuiSurfaceUpdateReceiver,
};
use bmux_keyboard::KeyCode;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text::{Line, Span};
use bmux_tui_components::action_row::{ActionButton, ActionRow, ActionRowStyles};
use bmux_tui_components::key_hint_bar::{KeyHint, KeyHintBar, KeyHintBarStyles};
use bmux_tui_components::pane::{Pane, PaneState, PaneStyles};
use bmux_tui_components::tab_bar::{TabBar, TabBarState, TabBarStyles, TabItem};
use bmux_tui_components::table::{
    Table, TableAlign, TableColumn, TableRow, TableState, TableStyles,
};
use bmux_tui_components::text_input::{TextInputPolicy, TextInputState};
use bmux_tui_components::text_input_box::{
    TextInputBox, TextInputBoxOutcome, TextInputBoxPolicy, TextInputBoxStyles,
};
use bmux_tui_components::text_view::{TextView, TextViewPolicy, TextViewState, TextViewStyles};
use std::collections::BTreeSet;
use std::fmt::Write as _;

pub const WORKFLOW_STATUS_SURFACE_KIND: &str = "workflow.status";
pub const WORKFLOW_AUTHOR_SURFACE_KIND: &str = "workflow.author";

#[must_use]
pub fn tui_registry() -> PluginTuiRegistry {
    let mut registry = PluginTuiRegistry::default();
    registry.register_factory(Box::new(WorkflowStatusFactory));
    registry.register_factory(Box::new(WorkflowAuthorFactory));
    registry
}

#[derive(Debug)]
struct WorkflowAuthorFactory;

impl PluginTuiSurfaceFactory for WorkflowAuthorFactory {
    fn surface_kind(&self) -> &'static str {
        WORKFLOW_AUTHOR_SURFACE_KIND
    }

    fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        crate::authoring_tui::open(request)
    }
}

#[cfg(test)]
fn template_start_command(options: &serde_json::Value) -> Option<String> {
    let template = options.get("template")?;
    let owner = template.get("owner_plugin_id")?.as_str()?;
    let contribution = template.get("template")?;
    let template_id = contribution.get("template_id")?.as_str()?;
    let version = contribution.get("template_version")?.as_u64()?;
    let configuration = options.get("configuration")?;
    let configuration = serde_json::to_string(configuration).ok()?;
    let session_id = options.get("session_id")?.as_str()?;
    Some(format!(
        "/workflow template-start owner_plugin_id={owner} template_id={template_id} template_version={version} session_id={session_id} configuration={configuration}"
    ))
}

#[cfg(test)]
fn author_lines(options: &serde_json::Value) -> Vec<String> {
    let mut lines = vec!["Workflow template authoring".to_string(), String::new()];
    let Some(description) = options.get("template") else {
        lines.push("No template selected".to_string());
        lines.push("Use /workflow template-describe with an exact template identity".to_string());
        return lines;
    };
    let template = description
        .get("template")
        .unwrap_or(&serde_json::Value::Null);
    lines.push(format!(
        "{} · {} v{} · owner {}",
        text(template, "title"),
        text(template, "template_id"),
        number(template, "template_version"),
        text(description, "owner_plugin_id")
    ));
    lines.push(text(template, "description").to_string());
    append_named_rows(
        &mut lines,
        description,
        "diagnostics",
        "Diagnostics",
        |value| {
            format!(
                "  {} · {} · {}",
                text(value, "code"),
                text(value, "requirement"),
                text(value, "message")
            )
        },
    );
    append_named_rows(
        &mut lines,
        template,
        "required_plugins",
        "Required plugins",
        |value| format!("  {}", value.as_str().unwrap_or("-")),
    );
    append_named_rows(
        &mut lines,
        template,
        "required_capabilities",
        "Required capabilities",
        |value| format!("  {}", value.as_str().unwrap_or("-")),
    );
    if let Some(configuration) = options.get("configuration") {
        lines.push("Validated configuration preview".to_string());
        lines.push(format!("  {configuration}"));
    }
    lines.extend(effect_preview(template));
    lines.push(String::new());
    if description
        .get("diagnostics")
        .and_then(serde_json::Value::as_array)
        .is_some_and(Vec::is_empty)
        && options.get("configuration").is_some()
    {
        lines.push("s start exact template · Esc/q close".to_string());
    } else {
        lines.push("Resolve diagnostics and provide valid configuration before start".to_string());
        lines.push("Esc/q close".to_string());
    }
    lines
}

#[cfg(test)]
fn effect_preview(template: &serde_json::Value) -> Vec<String> {
    let blocks = template
        .get("definition")
        .and_then(|definition| definition.get("nodes"))
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flat_map(serde_json::Map::values)
        .filter_map(|node| node.get("configuration"))
        .filter(|configuration| configuration.get("effect").is_some())
        .collect::<Vec<_>>();
    let mut lines = vec!["External effects and reconciliation".to_string()];
    if blocks.is_empty() {
        lines.push("  No manifest-declared external effects in this definition".to_string());
    } else {
        for block in blocks {
            lines.push(format!(
                "  {} / {} · effect={} · reconciliation={}",
                text(block, "plugin_id"),
                text(block, "block_id"),
                text(block, "effect"),
                text(block, "reconciliation")
            ));
        }
    }
    lines
}

#[derive(Debug)]
struct WorkflowStatusFactory;

impl PluginTuiSurfaceFactory for WorkflowStatusFactory {
    fn surface_kind(&self) -> &'static str {
        WORKFLOW_STATUS_SURFACE_KIND
    }

    fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        Box::pin(async move {
            Ok(Box::new(WorkflowStatusSurface {
                options: request.options,
                selected_approval: 0,
                text_view: TextViewState::new(),
                selected_run_id: None,
                selected_node_id: None,
                selected_wait_id: None,
                selected_approval_id: None,
                selected_attempt_id: None,
                selected_output_id: None,
                selected_child_session_id: None,
                detail_loading_run_id: None,
                catalog_loading: true,
                catalog_stale: false,
                catalog_error: None,
                detail_errors: std::collections::BTreeMap::new(),
                workspace_focus: WorkflowWorkspaceFocus::Catalog,
                narrow_page: WorkflowNarrowPage::Runs,
                descendants_expanded: false,
                active_detail_tab: 0,
                input_form: None,
                pending_confirmation: None,
                pending_action_target: None,
                inline_error: None,
                catalog_search_buffer: None,
                updates: None,
                catalog: None,
                runs: std::collections::BTreeMap::new(),
                live_status: "loading live workflow state".to_string(),
                subscription_requested: false,
            }) as BoxedPluginTuiSurface)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowWorkspaceFocus {
    Catalog,
    Graph,
    Inspector,
    Actions,
}

impl WorkflowWorkspaceFocus {
    const fn next(self) -> Self {
        match self {
            Self::Catalog => Self::Graph,
            Self::Graph => Self::Inspector,
            Self::Inspector => Self::Actions,
            Self::Actions => Self::Catalog,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowNarrowPage {
    Runs,
    Graph,
    Inspector,
    Actions,
}

impl WorkflowNarrowPage {
    const fn index(self) -> usize {
        match self {
            Self::Runs => 0,
            Self::Graph => 1,
            Self::Inspector => 2,
            Self::Actions => 3,
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Runs => Self::Graph,
            Self::Graph => Self::Inspector,
            Self::Inspector => Self::Actions,
            Self::Actions => Self::Runs,
        }
    }

    const fn previous(self) -> Self {
        match self {
            Self::Runs => Self::Actions,
            Self::Graph => Self::Runs,
            Self::Inspector => Self::Graph,
            Self::Actions => Self::Inspector,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowInputFieldKind {
    String,
    Boolean,
    Integer,
    Number,
}

#[derive(Debug)]
struct WorkflowInputField {
    name: String,
    kind: WorkflowInputFieldKind,
    required: bool,
    editor: TextInputState,
}

#[derive(Debug)]
struct PendingWorkflowConfirmation {
    title: String,
    detail: String,
    action: PluginTuiAction,
    kind: bcode_workflow_view_models::WorkflowActionKind,
    target: bcode_workflow_view_models::WorkflowActionTarget,
}

#[derive(Debug)]
struct WorkflowInputForm {
    run_id: String,
    node_id: String,
    activation_id: String,
    prompt: String,
    expected_schema: Option<serde_json::Value>,
    fields: Vec<WorkflowInputField>,
    focused_field: usize,
    editor: TextInputState,
    error: Option<String>,
}

impl WorkflowInputForm {
    fn new(run_id: String, wait: &bcode_workflow_view_models::WorkflowWaitView) -> Self {
        let initial = wait.input.as_ref().map_or_else(
            || default_input_value(wait.expected_schema.as_ref()).to_string(),
            serde_json::Value::to_string,
        );
        let fields = simple_object_fields(wait.expected_schema.as_ref(), wait.input.as_ref());
        Self {
            run_id,
            node_id: wait.node_id.clone(),
            activation_id: wait.activation_id.clone(),
            prompt: wait.prompt.clone(),
            expected_schema: wait.expected_schema.clone(),
            fields,
            focused_field: 0,
            editor: TextInputState::new(TextEditBuffer::from_text(initial)),
            error: None,
        }
    }

    fn validate(&self) -> Result<serde_json::Value, String> {
        let value = if self.fields.is_empty() {
            serde_json::from_str::<serde_json::Value>(self.editor.buffer().text())
                .map_err(|error| format!("Invalid JSON: {error}"))?
        } else {
            let mut object = serde_json::Map::new();
            for field in &self.fields {
                let text = field.editor.buffer().text().trim();
                if text.is_empty() {
                    if field.required {
                        return Err(format!("{} is required", field.name));
                    }
                    continue;
                }
                let value = match field.kind {
                    WorkflowInputFieldKind::String => serde_json::Value::String(text.to_string()),
                    WorkflowInputFieldKind::Boolean => text
                        .parse::<bool>()
                        .map(serde_json::Value::Bool)
                        .map_err(|_| format!("{} must be true or false", field.name))?,
                    WorkflowInputFieldKind::Integer => text
                        .parse::<i64>()
                        .map(serde_json::Value::from)
                        .map_err(|_| format!("{} must be an integer", field.name))?,
                    WorkflowInputFieldKind::Number => text
                        .parse::<f64>()
                        .ok()
                        .and_then(serde_json::Number::from_f64)
                        .map(serde_json::Value::Number)
                        .ok_or_else(|| format!("{} must be a finite number", field.name))?,
                };
                object.insert(field.name.clone(), value);
            }
            serde_json::Value::Object(object)
        };
        if let Some(schema) = &self.expected_schema {
            let validator = jsonschema::validator_for(schema)
                .map_err(|error| format!("Invalid expected schema: {error}"))?;
            if let Err(error) = validator.validate(&value) {
                return Err(format!("Input does not match expected schema: {error}"));
            }
        }
        Ok(value)
    }
}

fn simple_object_fields(
    schema: Option<&serde_json::Value>,
    input: Option<&serde_json::Value>,
) -> Vec<WorkflowInputField> {
    let Some(schema) = schema
        .filter(|schema| schema.get("type").and_then(serde_json::Value::as_str) == Some("object"))
    else {
        return Vec::new();
    };
    let Some(properties) = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
    else {
        return Vec::new();
    };
    if properties.is_empty() || properties.len() > 8 {
        return Vec::new();
    }
    let required = schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect::<BTreeSet<_>>();
    let mut fields = Vec::new();
    for (name, property) in properties {
        let kind = match property.get("type").and_then(serde_json::Value::as_str) {
            Some("string") => WorkflowInputFieldKind::String,
            Some("boolean") => WorkflowInputFieldKind::Boolean,
            Some("integer") => WorkflowInputFieldKind::Integer,
            Some("number") => WorkflowInputFieldKind::Number,
            _ => return Vec::new(),
        };
        let initial = input
            .and_then(|input| input.get(name))
            .map_or_else(String::new, |value| match value {
                serde_json::Value::String(value) => value.clone(),
                _ => value.to_string(),
            });
        fields.push(WorkflowInputField {
            name: name.clone(),
            kind,
            required: required.contains(name.as_str()),
            editor: TextInputState::new(TextEditBuffer::from_text(initial)),
        });
    }
    fields
}

fn default_input_value(schema: Option<&serde_json::Value>) -> serde_json::Value {
    let Some(schema) = schema else {
        return serde_json::Value::Null;
    };
    match schema.get("type").and_then(serde_json::Value::as_str) {
        Some("object") => serde_json::Value::Object(serde_json::Map::new()),
        Some("array") => serde_json::Value::Array(Vec::new()),
        Some("string") => serde_json::Value::String(String::new()),
        Some("boolean") => serde_json::Value::Bool(false),
        Some("integer" | "number") => serde_json::json!(0),
        _ => serde_json::Value::Null,
    }
}

#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
struct WorkflowStatusSurface {
    options: serde_json::Value,
    selected_approval: usize,
    text_view: TextViewState,
    selected_run_id: Option<String>,
    selected_node_id: Option<String>,
    selected_wait_id: Option<(String, String)>,
    selected_approval_id: Option<String>,
    selected_attempt_id: Option<(String, String, u32)>,
    selected_output_id: Option<String>,
    selected_child_session_id: Option<String>,
    detail_loading_run_id: Option<String>,
    catalog_loading: bool,
    catalog_stale: bool,
    catalog_error: Option<String>,
    detail_errors: std::collections::BTreeMap<String, String>,
    workspace_focus: WorkflowWorkspaceFocus,
    narrow_page: WorkflowNarrowPage,
    descendants_expanded: bool,
    active_detail_tab: usize,
    input_form: Option<WorkflowInputForm>,
    pending_confirmation: Option<PendingWorkflowConfirmation>,
    pending_action_target: Option<(
        bcode_workflow_view_models::WorkflowActionKind,
        bcode_workflow_view_models::WorkflowActionTarget,
    )>,
    inline_error: Option<String>,
    catalog_search_buffer: Option<String>,
    updates: Option<PluginTuiSurfaceUpdateReceiver>,
    catalog: Option<bcode_workflow_view_models::WorkflowCatalogView>,
    runs: std::collections::BTreeMap<String, bcode_workflow_view_models::WorkflowRunView>,
    live_status: String,
    subscription_requested: bool,
}

#[derive(Clone, Copy)]
struct WorkflowSurfaceTheme {
    canvas: Style,
    text: Style,
    muted: Style,
    focused: Style,
    selected: Style,
    info: Style,
    success: Style,
    warning: Style,
    error: Style,
    component: bmux_tui_components::theme::ComponentTheme,
}

impl WorkflowSurfaceTheme {
    fn resolve(theme: Option<bcode_plugin_sdk::tui::PluginTuiTheme>) -> Self {
        theme.map_or_else(
            || {
                let component = bmux_tui_components::theme::ComponentTheme::default();
                Self {
                    canvas: component.canvas,
                    text: component.text,
                    muted: component.muted,
                    focused: component.focused,
                    selected: component.selected,
                    info: component.info,
                    success: component.success,
                    warning: component.warning,
                    error: component.error,
                    component,
                }
            },
            |theme| {
                let component = theme.component_theme().unwrap_or_default();
                Self {
                    canvas: theme.canvas,
                    text: theme.text,
                    muted: theme.muted,
                    focused: theme.focused,
                    selected: theme.selection,
                    info: component.info,
                    success: component.success,
                    warning: component.warning,
                    error: component.error,
                    component,
                }
            },
        )
    }
}

const fn workflow_run_status_is_terminal(
    status: bcode_workflow_view_models::WorkflowRunStatus,
) -> bool {
    matches!(
        status,
        bcode_workflow_view_models::WorkflowRunStatus::Completed
            | bcode_workflow_view_models::WorkflowRunStatus::Failed
            | bcode_workflow_view_models::WorkflowRunStatus::Cancelled
            | bcode_workflow_view_models::WorkflowRunStatus::RepairRequired
    )
}

impl WorkflowStatusSurface {
    fn action_is_current(
        &self,
        kind: bcode_workflow_view_models::WorkflowActionKind,
        target: &bcode_workflow_view_models::WorkflowActionTarget,
    ) -> bool {
        self.selected_run_view().is_some_and(|run| {
            run.actions
                .iter()
                .any(|action| action.kind == kind && action.enabled && &action.target == target)
        })
    }

    fn dispatch_exact_action(
        &mut self,
        kind: bcode_workflow_view_models::WorkflowActionKind,
        target: bcode_workflow_view_models::WorkflowActionTarget,
        action: PluginTuiAction,
    ) -> PluginTuiAction {
        if !self.action_is_current(kind, &target) {
            self.inline_error = Some(
                "Action target is stale or unavailable; wait for authoritative refresh".to_string(),
            );
            return PluginTuiAction::Redraw;
        }
        self.inline_error = None;
        self.pending_action_target = Some((kind, target));
        self.live_status = "action submitted; waiting for authoritative refresh".to_string();
        action
    }

    fn confirm_action(
        &mut self,
        title: impl Into<String>,
        detail: impl Into<String>,
        kind: bcode_workflow_view_models::WorkflowActionKind,
        target: bcode_workflow_view_models::WorkflowActionTarget,
        action: PluginTuiAction,
    ) -> PluginTuiAction {
        if !self.action_is_current(kind, &target) {
            self.inline_error = Some(
                "Action target is stale or unavailable; confirmation was not opened".to_string(),
            );
            return PluginTuiAction::Redraw;
        }
        self.pending_confirmation = Some(PendingWorkflowConfirmation {
            title: title.into(),
            detail: detail.into(),
            action,
            kind,
            target,
        });
        PluginTuiAction::Redraw
    }

    fn selected_run_view(&self) -> Option<&bcode_workflow_view_models::WorkflowRunView> {
        self.runs.get(self.selected_run_id.as_deref()?)
    }

    fn selected_wait(
        &self,
        kind: bcode_workflow_view_models::WorkflowWaitKind,
    ) -> Option<&bcode_workflow_view_models::WorkflowWaitView> {
        let run = self.selected_run_view()?;
        self.selected_wait_id
            .as_ref()
            .and_then(|(node_id, activation_id)| {
                run.waits.iter().find(|wait| {
                    wait.kind == kind
                        && &wait.node_id == node_id
                        && &wait.activation_id == activation_id
                })
            })
    }

    fn selected_mutation_approval(
        &self,
    ) -> Option<&bcode_workflow_view_models::WorkflowMutationApprovalView> {
        let approval_id = self.selected_approval_id.as_deref()?;
        self.selected_run_view()?
            .mutation_approvals
            .iter()
            .find(|approval| approval.approval_id == approval_id)
    }

    fn selected_attempt(&self) -> Option<&bcode_workflow_view_models::WorkflowAttemptView> {
        let (node_id, activation_id, attempt) = self.selected_attempt_id.as_ref()?;
        self.selected_run_view()?.attempts.iter().find(|candidate| {
            &candidate.node_id == node_id
                && &candidate.activation_id == activation_id
                && candidate.attempt == *attempt
        })
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn selected_node(&self) -> Option<&bcode_workflow_view_models::WorkflowNodeView> {
        let node_id = self.selected_node_id.as_deref()?;
        self.selected_run_view()?
            .nodes
            .iter()
            .find(|node| node.node_id == node_id)
    }

    fn selected_projection_notice(&self) -> Option<(String, bool)> {
        let run = self.selected_run_view()?;
        match &run.health {
            bcode_workflow_view_models::WorkflowProjectionHealth::Current => (run.run.status
                == bcode_workflow_view_models::WorkflowRunStatus::RepairRequired)
                .then(|| ("Selected run requires explicit repair".to_string(), true)),
            bcode_workflow_view_models::WorkflowProjectionHealth::Degraded { reason } => Some((
                format!("Selected run projection is degraded · {reason}"),
                false,
            )),
            bcode_workflow_view_models::WorkflowProjectionHealth::RepairRequired { reason } => {
                Some((format!("Selected run requires repair · {reason}"), true))
            }
            bcode_workflow_view_models::WorkflowProjectionHealth::UnsupportedVersion {
                version,
            } => Some((
                format!("Selected run uses unsupported projection version {version}"),
                true,
            )),
        }
    }

    fn has_workspace_notice(&self) -> bool {
        self.catalog_loading
            || self.catalog_error.is_some()
            || self.inline_error.is_some()
            || self.live_status != "live"
            || self.selected_projection_notice().is_some()
    }

    fn render_focused_pane(
        area: Rect,
        frame: &mut Frame<'_>,
        theme: WorkflowSurfaceTheme,
        title: &'static str,
        focused: bool,
    ) -> Rect {
        if area.is_empty() {
            return area;
        }
        let pane = Pane::new()
            .title(Line::from(title))
            .padding(Insets::new(1, 1, 1, 1))
            .styles(PaneStyles {
                background: Some(theme.canvas),
                border: theme.component.border,
                focused_border: theme.focused,
            });
        let mut state = PaneState::new(area);
        state.interaction = state.interaction.focused(focused);
        pane.render(&state, frame);
        pane.inner_area(&state)
    }

    fn render_catalog_pane(
        &self,
        area: Rect,
        frame: &mut Frame<'_>,
        theme: WorkflowSurfaceTheme,
        catalog: &bcode_workflow_view_models::WorkflowCatalogView,
        focused: bool,
    ) {
        let inner = Self::render_focused_pane(area, frame, theme, "Runs", focused);
        self.render_catalog(inner, frame, theme, catalog);
    }

    fn render_graph_pane(
        &self,
        area: Rect,
        frame: &mut Frame<'_>,
        theme: WorkflowSurfaceTheme,
        focused: bool,
    ) {
        let inner = Self::render_focused_pane(area, frame, theme, "Execution graph", focused);
        self.render_graph(inner, frame, theme);
    }

    fn render_inspector_pane(
        &self,
        area: Rect,
        frame: &mut Frame<'_>,
        theme: WorkflowSurfaceTheme,
        tabbed: bool,
        focused: bool,
    ) {
        let inner = Self::render_focused_pane(area, frame, theme, "Inspector", focused);
        self.render_inspector(inner, frame, theme, tabbed);
    }

    fn render_action_pane(
        &self,
        area: Rect,
        frame: &mut Frame<'_>,
        theme: WorkflowSurfaceTheme,
        focused: bool,
    ) {
        let inner = Self::render_focused_pane(area, frame, theme, "Actions", focused);
        self.render_action_panel(inner, frame, theme);
    }

    #[allow(clippy::too_many_lines)]
    fn render_workspace(&self, area: Rect, frame: &mut Frame<'_>, theme: WorkflowSurfaceTheme) {
        const GUTTER: u16 = 1;
        if area.height < 4 || area.width < 24 {
            return;
        }
        let Some(catalog) = self.catalog.as_ref() else {
            let message = self
                .catalog_error
                .as_ref()
                .map_or("Loading workflow catalog…", |error| error.as_str());
            frame.write_line(
                Rect::new(area.x, area.y, area.width, 1),
                &Line::from_spans(vec![Span::styled(
                    message,
                    if self.catalog_error.is_some() {
                        theme.error
                    } else {
                        theme.muted
                    },
                )]),
            );
            return;
        };
        let header_height = u16::from(self.has_workspace_notice())
            .saturating_add(2)
            .min(area.height);
        let header = Rect::new(area.x, area.y, area.width, header_height);
        let footer_height = 1.min(area.height.saturating_sub(header.height));
        let body = Rect::new(
            area.x,
            area.y.saturating_add(header.height),
            area.width,
            area.height
                .saturating_sub(header.height)
                .saturating_sub(footer_height),
        );
        let footer = Rect::new(
            area.x,
            area.bottom().saturating_sub(footer_height),
            area.width,
            footer_height,
        );
        self.render_workspace_header(header, frame, theme, catalog);
        if area.width >= 112 {
            let available = body.width.saturating_sub(GUTTER.saturating_mul(2));
            let catalog_width = available.saturating_mul(28) / 100;
            let inspector_width = available.saturating_mul(30) / 100;
            let graph_width = available
                .saturating_sub(catalog_width)
                .saturating_sub(inspector_width);
            let catalog_area = Rect::new(body.x, body.y, catalog_width, body.height);
            let graph_area = Rect::new(
                catalog_area.right().saturating_add(GUTTER),
                body.y,
                graph_width,
                body.height,
            );
            let inspector_area = Rect::new(
                graph_area.right().saturating_add(GUTTER),
                body.y,
                inspector_width,
                body.height,
            );
            self.render_catalog_pane(
                catalog_area,
                frame,
                theme,
                catalog,
                self.workspace_focus == WorkflowWorkspaceFocus::Catalog,
            );
            self.render_graph_pane(
                graph_area,
                frame,
                theme,
                self.workspace_focus == WorkflowWorkspaceFocus::Graph,
            );
            self.render_inspector_pane(
                inspector_area,
                frame,
                theme,
                false,
                self.workspace_focus == WorkflowWorkspaceFocus::Inspector,
            );
        } else if area.width >= 72 {
            let available = body.width.saturating_sub(GUTTER);
            let catalog_width = available.saturating_mul(38) / 100;
            let catalog_area = Rect::new(body.x, body.y, catalog_width, body.height);
            let detail_area = Rect::new(
                catalog_area.right().saturating_add(GUTTER),
                body.y,
                available.saturating_sub(catalog_width),
                body.height,
            );
            self.render_catalog_pane(
                catalog_area,
                frame,
                theme,
                catalog,
                self.workspace_focus == WorkflowWorkspaceFocus::Catalog,
            );
            self.render_inspector_pane(
                detail_area,
                frame,
                theme,
                true,
                self.workspace_focus != WorkflowWorkspaceFocus::Catalog,
            );
        } else {
            self.render_narrow_page(body, frame, theme, catalog);
        }
        if let Some(search) = &self.catalog_search_buffer {
            frame.write_line(
                footer,
                &Line::from_spans(vec![
                    Span::styled("Search › ", theme.focused),
                    Span::styled(search, theme.text),
                    Span::styled("  Enter apply · Esc cancel", theme.muted),
                ]),
            );
            return;
        }
        let hints = [
            KeyHint::new("←/→", "run"),
            KeyHint::new("↑/↓", "node"),
            KeyHint::new("Tab", "focus/page"),
            KeyHint::new("1-4", "page"),
            KeyHint::new("[/]", "section"),
            KeyHint::new("/", "search"),
            KeyHint::new("f/s/g", "filter/sort/group"),
            KeyHint::new("m", "more"),
        ];
        KeyHintBar::new(&hints)
            .styles(KeyHintBarStyles {
                key: theme.focused,
                label: theme.text,
                separator: theme.muted,
                disabled: theme.muted,
                background: theme.canvas,
            })
            .render(footer, frame);
    }

    fn render_workspace_header(
        &self,
        area: Rect,
        frame: &mut Frame<'_>,
        theme: WorkflowSurfaceTheme,
        catalog: &bcode_workflow_view_models::WorkflowCatalogView,
    ) {
        let active = catalog
            .runs
            .iter()
            .filter(|run| {
                matches!(
                    run.status,
                    bcode_workflow_view_models::WorkflowRunStatus::Running
                        | bcode_workflow_view_models::WorkflowRunStatus::Paused
                )
            })
            .count();
        let attention = catalog
            .runs
            .iter()
            .filter(|run| run.attention.needs_attention())
            .count();
        let failed = catalog
            .runs
            .iter()
            .filter(|run| {
                matches!(
                    run.status,
                    bcode_workflow_view_models::WorkflowRunStatus::Failed
                        | bcode_workflow_view_models::WorkflowRunStatus::RepairRequired
                )
            })
            .count();
        let completed = catalog
            .runs
            .iter()
            .filter(|run| run.status == bcode_workflow_view_models::WorkflowRunStatus::Completed)
            .count();
        frame.write_line(
            Rect::new(area.x, area.y, area.width, 1),
            &Line::from_spans(vec![
                Span::styled(" Workflows  ", theme.focused.add_modifier(Modifier::BOLD)),
                Span::styled(format!("{}  ", self.live_status), theme.info),
                Span::styled(format!("active {active}  "), theme.info),
                Span::styled(format!("attention {attention}  "), theme.warning),
                Span::styled(format!("failed {failed}  "), theme.error),
                Span::styled(format!("completed {completed}"), theme.success),
            ]),
        );
        if area.height > 1 {
            frame.write_line(
                Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
                &Line::from_spans(vec![Span::styled(
                    format!(
                        " Filter: {:?}  Sort: {:?}  Group: {:?}  Search: {}  Showing {}{}",
                        catalog.filter,
                        catalog.sort,
                        catalog.group,
                        catalog.search.as_deref().unwrap_or("all"),
                        catalog.runs.len(),
                        if catalog.has_more { "+" } else { "" }
                    ),
                    theme.muted,
                )]),
            );
        }
        if area.height > 2 {
            let (message, style) = self.inline_error.as_ref().map_or_else(
                || {
                    self.catalog_error.as_ref().map_or_else(
                        || {
                            if self.catalog_loading && self.catalog_stale {
                                (
                                    "Refreshing catalog · showing stale results".to_string(),
                                    theme.warning,
                                )
                            } else if self.catalog_loading {
                                ("Loading workflow catalog…".to_string(), theme.info)
                            } else if self.live_status != "live" {
                                (self.live_status.clone(), theme.warning)
                            } else if let Some((message, error)) = self.selected_projection_notice()
                            {
                                (message, if error { theme.error } else { theme.warning })
                            } else {
                                ("Workflow state is current".to_string(), theme.success)
                            }
                        },
                        |error| (format!("Catalog refresh failed · {error}"), theme.error),
                    )
                },
                |error| (error.clone(), theme.error),
            );
            frame.write_line(
                Rect::new(area.x, area.y.saturating_add(2), area.width, 1),
                &Line::from_spans(vec![Span::styled(message, style)]),
            );
        }
    }

    fn render_catalog(
        &self,
        area: Rect,
        frame: &mut Frame<'_>,
        theme: WorkflowSurfaceTheme,
        catalog: &bcode_workflow_view_models::WorkflowCatalogView,
    ) {
        if area.is_empty() {
            return;
        }
        if catalog.runs.is_empty() {
            let message = if catalog.search.is_some()
                || catalog.filter != bcode_workflow_view_models::WorkflowCatalogFilter::All
            {
                "No workflow runs match the current query"
            } else {
                "No workflow runs yet"
            };
            frame.write_line(
                Rect::new(area.x, area.y, area.width, 1),
                &Line::from_spans(vec![Span::styled(message, theme.muted)]),
            );
            return;
        }
        let columns = [
            TableColumn::new("Run").flex(3).align(TableAlign::Left),
            TableColumn::new("Status").flex(2).align(TableAlign::Left),
        ];
        let mut current_group = String::new();
        let rows = catalog
            .runs
            .iter()
            .map(|run| {
                let group = match catalog.group {
                    bcode_workflow_view_models::WorkflowCatalogGroup::None => String::new(),
                    bcode_workflow_view_models::WorkflowCatalogGroup::AuthoredWorkflow => {
                        run.authored_source.as_ref().map_or_else(
                            || "Compiled definitions".to_string(),
                            |source| source.workflow_id.clone(),
                        )
                    }
                    bcode_workflow_view_models::WorkflowCatalogGroup::Definition => {
                        format!("{} v{}", run.definition_id, run.definition_version)
                    }
                };
                let group_prefix = if group.is_empty() || group == current_group {
                    String::new()
                } else {
                    current_group.clone_from(&group);
                    format!("[{group}] ")
                };
                let attention = if run.attention.needs_attention() {
                    " !"
                } else {
                    ""
                };
                let status_style = workflow_run_style(run.status, theme);
                TableRow::rich(vec![
                    Line::from_spans(vec![
                        Span::styled(group_prefix, theme.muted),
                        Span::styled(run.display_title.clone(), theme.text),
                    ]),
                    Line::from_spans(vec![
                        Span::styled(format!("{:?}", run.status), status_style),
                        Span::styled(attention, theme.warning),
                    ]),
                ])
            })
            .collect::<Vec<_>>();
        let selected = self
            .selected_run_id
            .as_deref()
            .and_then(|run_id| catalog.runs.iter().position(|run| run.run_id == run_id));
        Table::new(&columns, &rows)
            .styles(TableStyles {
                header: theme.focused,
                row: theme.text,
                selected: theme.selected,
                selected_column: theme.selected,
                selected_cell: theme.warning,
                hovered: theme.focused,
                disabled: theme.muted,
                separator: theme.component.border,
                empty: theme.muted,
            })
            .render(area, &TableState::new(selected), frame);
    }

    fn render_graph(&self, area: Rect, frame: &mut Frame<'_>, theme: WorkflowSurfaceTheme) {
        let lines = if let Some(run_id) = self.selected_run_id.as_deref()
            && let Some(error) = self.detail_errors.get(run_id)
        {
            vec![Line::from_spans(vec![Span::styled(
                format!("Run detail unavailable · {error}"),
                theme.error,
            )])]
        } else if self.detail_loading_run_id.as_deref() == self.selected_run_id.as_deref() {
            vec![Line::from_spans(vec![Span::styled(
                "Loading selected run…",
                theme.muted,
            )])]
        } else if self.selected_run_id.is_some() && self.selected_run_view().is_none() {
            vec![Line::from_spans(vec![Span::styled(
                "Selected run detail is not loaded",
                theme.warning,
            )])]
        } else {
            self.selected_run_view().map_or_else(
                || vec![Line::from("Select a workflow run")],
                |run| {
                    workflow_graph_lines(
                        run,
                        self.selected_node_id.as_deref(),
                        area.width,
                        area.height,
                        self.descendants_expanded,
                        theme,
                    )
                },
            )
        };
        TextView::new(&lines)
            .policy(TextViewPolicy::bare())
            .styles(TextViewStyles {
                text: theme.text,
                empty: theme.muted,
                background: theme.canvas,
            })
            .render(area, &self.text_view, frame);
    }

    fn render_inspector(
        &self,
        area: Rect,
        frame: &mut Frame<'_>,
        theme: WorkflowSurfaceTheme,
        tabbed: bool,
    ) {
        if area.is_empty() {
            return;
        }
        let tabs = [
            TabItem::new("overview", "Overview"),
            TabItem::new("inputs", "Inputs"),
            TabItem::new("outputs", "Outputs"),
            TabItem::new("attempts", "Attempts"),
            TabItem::new("approvals", "Approvals"),
            TabItem::new("sessions", "Sessions"),
            TabItem::new("definition", "Definition"),
        ];
        let tab_height = u16::from(tabbed || area.width < 44);
        if tab_height > 0 {
            TabBar::new(&tabs)
                .styles(TabBarStyles {
                    normal: theme.muted,
                    selected: theme.selected,
                    focused: theme.focused,
                    hovered: theme.focused,
                    pressed: theme.selected,
                    disabled: theme.muted,
                    separator: theme.component.border,
                })
                .render(
                    Rect::new(area.x, area.y, area.width, tab_height),
                    &TabBarState::new(Some(self.active_detail_tab)),
                    frame,
                );
        }
        let content = Rect::new(
            area.x,
            area.y.saturating_add(tab_height),
            area.width,
            area.height.saturating_sub(tab_height),
        );
        let lines = if self.selected_run_id.is_some() && self.selected_run_view().is_none() {
            vec![Line::from_spans(vec![Span::styled(
                if self.detail_loading_run_id.as_deref() == self.selected_run_id.as_deref() {
                    "Loading selected run…"
                } else {
                    "Selected run detail is not loaded"
                },
                if self.detail_loading_run_id.as_deref() == self.selected_run_id.as_deref() {
                    theme.muted
                } else {
                    theme.warning
                },
            )])]
        } else {
            inspector_lines(self.selected_run_view(), self.active_detail_tab, theme)
        };
        TextView::new(&lines)
            .policy(TextViewPolicy::bare())
            .styles(TextViewStyles {
                text: theme.text,
                empty: theme.muted,
                background: theme.canvas,
            })
            .render(content, &self.text_view, frame);
    }

    fn render_action_panel(&self, area: Rect, frame: &mut Frame<'_>, theme: WorkflowSurfaceTheme) {
        let Some(run) = self.selected_run_view() else {
            frame.write_line(
                area,
                &Line::from_spans(vec![Span::styled(
                    if self.detail_loading_run_id.as_deref() == self.selected_run_id.as_deref() {
                        "Loading selected run…"
                    } else {
                        "Selected run detail is not loaded"
                    },
                    theme.muted,
                )]),
            );
            return;
        };
        let actions = run
            .actions
            .iter()
            .map(|action| {
                ActionButton::new(format!("{:?}", action.kind), format!("{:?}", action.kind))
            })
            .collect::<Vec<_>>();
        let row_area = Rect::new(area.x, area.y, area.width, 1.min(area.height));
        ActionRow::new(&actions)
            .styles(ActionRowStyles {
                normal: theme.text,
                focused: theme.focused,
                hovered: theme.focused,
                pressed: theme.selected,
                disabled: theme.muted,
            })
            .render_with_fallback_style(row_area, frame, theme.canvas);
        for (index, action) in run.actions.iter().enumerate() {
            if let Some(reason) = &action.unavailable_reason {
                let row = u16::try_from(index).unwrap_or(u16::MAX).saturating_add(2);
                if row < area.height {
                    frame.write_line(
                        Rect::new(area.x, area.y.saturating_add(row), area.width, 1),
                        &Line::from_spans(vec![Span::styled(
                            format!("{:?}: {reason}", action.kind),
                            theme.muted,
                        )]),
                    );
                }
            }
        }
    }

    fn render_narrow_page(
        &self,
        area: Rect,
        frame: &mut Frame<'_>,
        theme: WorkflowSurfaceTheme,
        catalog: &bcode_workflow_view_models::WorkflowCatalogView,
    ) {
        if area.is_empty() {
            return;
        }
        let pages = [
            TabItem::new("runs", "Runs"),
            TabItem::new("graph", "Graph"),
            TabItem::new("inspector", "Inspector"),
            TabItem::new("actions", "Actions"),
        ];
        TabBar::new(&pages)
            .styles(TabBarStyles {
                normal: theme.muted,
                selected: theme.selected,
                focused: theme.focused,
                hovered: theme.focused,
                pressed: theme.selected,
                disabled: theme.muted,
                separator: theme.component.border,
            })
            .render(
                Rect::new(area.x, area.y, area.width, 1),
                &TabBarState::new(Some(self.narrow_page.index())),
                frame,
            );
        let page_area = Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            area.height.saturating_sub(1),
        );
        match self.narrow_page {
            WorkflowNarrowPage::Runs => {
                self.render_catalog_pane(page_area, frame, theme, catalog, true);
            }
            WorkflowNarrowPage::Graph => self.render_graph_pane(page_area, frame, theme, true),
            WorkflowNarrowPage::Inspector => {
                self.render_inspector_pane(page_area, frame, theme, true, true);
            }
            WorkflowNarrowPage::Actions => {
                self.render_action_pane(page_area, frame, theme, true);
            }
        }
    }

    fn select_adjacent_run(&mut self, offset: isize) -> PluginTuiAction {
        let Some(catalog) = &self.catalog else {
            return PluginTuiAction::None;
        };
        if catalog.runs.is_empty() {
            self.selected_run_id = None;
            self.selected_node_id = None;
            return PluginTuiAction::Redraw;
        }
        let current = self
            .selected_run_id
            .as_deref()
            .and_then(|run_id| catalog.runs.iter().position(|run| run.run_id == run_id))
            .unwrap_or(0);
        let next = current
            .saturating_add_signed(offset)
            .min(catalog.runs.len().saturating_sub(1));
        let run_id = catalog.runs[next].run_id.clone();
        if self.selected_run_id.as_deref() == Some(run_id.as_str()) {
            return PluginTuiAction::Redraw;
        }
        self.selected_run_id = Some(run_id.clone());
        self.selected_node_id = None;
        self.selected_wait_id = None;
        self.selected_approval_id = None;
        self.selected_attempt_id = None;
        self.selected_output_id = None;
        self.selected_child_session_id = None;
        self.detail_errors.remove(&run_id);
        self.detail_loading_run_id = Some(run_id.clone());
        PluginTuiAction::SelectWorkflowRun { run_id }
    }

    fn definition_navigation_action(&self) -> PluginTuiAction {
        let Some(run) = self.selected_run_view() else {
            return PluginTuiAction::None;
        };
        match &run.run.definition_disposition {
            bcode_workflow_view_models::WorkflowDefinitionDisposition::Published {
                workflow_id,
                revision,
                editable_draft_id,
            } => PluginTuiAction::OpenSurface {
                plugin_id: "bcode.workflow".to_string(),
                surface_id: WORKFLOW_AUTHOR_SURFACE_KIND.to_string(),
                options: editable_draft_id.as_ref().map_or_else(
                    || {
                        let draft_id = format!("revision-{revision}-fork");
                        serde_json::json!({
                            "workflow_id": workflow_id,
                            "draft": {
                                "identity": {
                                    "workflow_id": workflow_id,
                                    "draft_id": draft_id,
                                },
                                "base_revision": revision,
                            },
                            "fork_revision": revision,
                        })
                    },
                    |draft_id| {
                        serde_json::json!({
                            "workflow_id": workflow_id,
                            "draft_id": draft_id,
                        })
                    },
                ),
            },
            bcode_workflow_view_models::WorkflowDefinitionDisposition::CompiledOnly => {
                PluginTuiAction::None
            }
        }
    }

    fn select_adjacent_node(&mut self, offset: isize) -> PluginTuiAction {
        let Some(run) = self.selected_run_view() else {
            return PluginTuiAction::None;
        };
        if run.nodes.is_empty() {
            self.selected_node_id = None;
            return PluginTuiAction::Redraw;
        }
        let current = self
            .selected_node_id
            .as_deref()
            .and_then(|node_id| run.nodes.iter().position(|node| node.node_id == node_id))
            .unwrap_or(0);
        let next = current
            .saturating_add_signed(offset)
            .min(run.nodes.len().saturating_sub(1));
        self.selected_node_id = Some(run.nodes[next].node_id.clone());
        PluginTuiAction::Redraw
    }

    #[allow(clippy::too_many_lines)]
    fn handle_control_center_event(&mut self, event: &Event) -> PluginTuiAction {
        let Event::Key(key) = event else {
            return PluginTuiAction::None;
        };
        if self.pending_confirmation.is_some() {
            match key.key {
                KeyCode::Escape | KeyCode::Char('n') => {
                    self.pending_confirmation = None;
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Enter | KeyCode::Char('y') => {
                    let Some(confirmation) = self.pending_confirmation.take() else {
                        return PluginTuiAction::None;
                    };
                    return self.dispatch_exact_action(
                        confirmation.kind,
                        confirmation.target,
                        confirmation.action,
                    );
                }
                _ => return PluginTuiAction::None,
            }
        }
        if self.input_form.is_some() {
            if matches!(key.key, KeyCode::Escape) {
                self.input_form = None;
                return PluginTuiAction::Redraw;
            }
            let submit = key.key == KeyCode::Enter && key.modifiers.ctrl;
            if submit {
                let Some(form) = self.input_form.as_mut() else {
                    return PluginTuiAction::None;
                };
                match form.validate() {
                    Ok(value) => {
                        return plugin_command(
                            "workflow.provide-input",
                            format!(
                                "run_id={} node_id={} activation_id={} value={value}",
                                form.run_id, form.node_id, form.activation_id
                            ),
                        );
                    }
                    Err(error) => {
                        form.error = Some(error);
                        return PluginTuiAction::Redraw;
                    }
                }
            }
            let Some(form) = self.input_form.as_mut() else {
                return PluginTuiAction::None;
            };
            if !form.fields.is_empty() {
                if key.key == KeyCode::Tab {
                    if key.modifiers.shift {
                        form.focused_field = form.focused_field.saturating_sub(1);
                    } else {
                        form.focused_field = (form.focused_field + 1) % form.fields.len();
                    }
                    return PluginTuiAction::Redraw;
                }
                let field = &mut form.fields[form.focused_field];
                let area = if field.editor.content_area().is_empty() {
                    Rect::new(0, 0, 80, 1)
                } else {
                    field.editor.content_area()
                };
                return match TextInputBox::new(TextInputPolicy::chat_composer())
                    .policy(TextInputBoxPolicy::bare().focused(true).rows(1, Some(1)))
                    .handle_event(area, &mut field.editor, event)
                {
                    TextInputBoxOutcome::Ignored => PluginTuiAction::None,
                    TextInputBoxOutcome::Edited
                    | TextInputBoxOutcome::Redraw
                    | TextInputBoxOutcome::Submitted
                    | TextInputBoxOutcome::EdgeUp
                    | TextInputBoxOutcome::EdgeDown => {
                        form.error = None;
                        PluginTuiAction::Redraw
                    }
                };
            }
            let area = if form.editor.content_area().is_empty() {
                Rect::new(0, 0, 80, 8)
            } else {
                form.editor.content_area()
            };
            return match TextInputBox::new(TextInputPolicy::chat_composer())
                .policy(TextInputBoxPolicy::bare().focused(true).rows(3, Some(8)))
                .handle_event(area, &mut form.editor, event)
            {
                TextInputBoxOutcome::Ignored => PluginTuiAction::None,
                TextInputBoxOutcome::Edited
                | TextInputBoxOutcome::Redraw
                | TextInputBoxOutcome::Submitted
                | TextInputBoxOutcome::EdgeUp
                | TextInputBoxOutcome::EdgeDown => {
                    form.error = None;
                    PluginTuiAction::Redraw
                }
            };
        }
        if let Some(buffer) = self.catalog_search_buffer.as_mut() {
            match key.key {
                KeyCode::Escape => {
                    self.catalog_search_buffer = None;
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Backspace => {
                    buffer.pop();
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Char(character) => {
                    buffer.push(character);
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Enter => {
                    let search = std::mem::take(buffer);
                    self.catalog_search_buffer = None;
                    let Some(catalog) = self.catalog.as_ref() else {
                        return PluginTuiAction::None;
                    };
                    return PluginTuiAction::UpdateWorkflowCatalogQuery {
                        filter: catalog.filter,
                        sort: catalog.sort,
                        group: catalog.group,
                        search: (!search.trim().is_empty()).then_some(search),
                    };
                }
                _ => return PluginTuiAction::None,
            }
        }
        match key.key {
            KeyCode::Escape | KeyCode::Char('q') => PluginTuiAction::Close { outcome: None },
            KeyCode::Left | KeyCode::Char('h') => self.select_adjacent_run(-1),
            KeyCode::Right | KeyCode::Char('l') => self.select_adjacent_run(1),
            KeyCode::Up | KeyCode::Char('k') => self.select_adjacent_node(-1),
            KeyCode::Down | KeyCode::Char('j') => self.select_adjacent_node(1),
            KeyCode::Char('/') => {
                self.catalog_search_buffer = Some(
                    self.catalog
                        .as_ref()
                        .and_then(|catalog| catalog.search.clone())
                        .unwrap_or_default(),
                );
                PluginTuiAction::Redraw
            }
            KeyCode::Char('f') => {
                let Some(catalog) = self.catalog.as_ref() else {
                    return PluginTuiAction::None;
                };
                PluginTuiAction::UpdateWorkflowCatalogQuery {
                    filter: next_catalog_filter(catalog.filter),
                    sort: catalog.sort,
                    group: catalog.group,
                    search: catalog.search.clone(),
                }
            }
            KeyCode::Char('s') => {
                let Some(catalog) = self.catalog.as_ref() else {
                    return PluginTuiAction::None;
                };
                PluginTuiAction::UpdateWorkflowCatalogQuery {
                    filter: catalog.filter,
                    sort: next_catalog_sort(catalog.sort),
                    group: catalog.group,
                    search: catalog.search.clone(),
                }
            }
            KeyCode::Char('g') => {
                let Some(catalog) = self.catalog.as_ref() else {
                    return PluginTuiAction::None;
                };
                PluginTuiAction::UpdateWorkflowCatalogQuery {
                    filter: catalog.filter,
                    sort: catalog.sort,
                    group: next_catalog_group(catalog.group),
                    search: catalog.search.clone(),
                }
            }
            KeyCode::Char('m') => self
                .catalog
                .as_ref()
                .and_then(|catalog| catalog.next_cursor.clone())
                .map_or(PluginTuiAction::None, |cursor| {
                    PluginTuiAction::LoadMoreWorkflowRuns { cursor }
                }),
            KeyCode::Tab => {
                if key.modifiers.shift {
                    self.narrow_page = self.narrow_page.previous();
                    self.workspace_focus = match self.narrow_page {
                        WorkflowNarrowPage::Runs => WorkflowWorkspaceFocus::Catalog,
                        WorkflowNarrowPage::Graph => WorkflowWorkspaceFocus::Graph,
                        WorkflowNarrowPage::Inspector => WorkflowWorkspaceFocus::Inspector,
                        WorkflowNarrowPage::Actions => WorkflowWorkspaceFocus::Actions,
                    };
                } else {
                    self.workspace_focus = self.workspace_focus.next();
                    self.narrow_page = self.narrow_page.next();
                }
                PluginTuiAction::Redraw
            }
            KeyCode::Char('1') => {
                self.narrow_page = WorkflowNarrowPage::Runs;
                self.workspace_focus = WorkflowWorkspaceFocus::Catalog;
                PluginTuiAction::Redraw
            }
            KeyCode::Char('2') => {
                self.narrow_page = WorkflowNarrowPage::Graph;
                self.workspace_focus = WorkflowWorkspaceFocus::Graph;
                PluginTuiAction::Redraw
            }
            KeyCode::Char('3') => {
                self.narrow_page = WorkflowNarrowPage::Inspector;
                self.workspace_focus = WorkflowWorkspaceFocus::Inspector;
                PluginTuiAction::Redraw
            }
            KeyCode::Char('4') => {
                self.narrow_page = WorkflowNarrowPage::Actions;
                self.workspace_focus = WorkflowWorkspaceFocus::Actions;
                PluginTuiAction::Redraw
            }
            KeyCode::Char('n') => {
                if self.selected_run_view().is_some_and(|run| {
                    !run.descendant_runs.is_empty() || !run.child_sessions.is_empty()
                }) {
                    self.descendants_expanded = !self.descendants_expanded;
                    PluginTuiAction::Redraw
                } else {
                    PluginTuiAction::None
                }
            }
            KeyCode::Char('[') => {
                self.active_detail_tab = self.active_detail_tab.saturating_sub(1);
                PluginTuiAction::Redraw
            }
            KeyCode::Char(']') => {
                self.active_detail_tab = (self.active_detail_tab + 1) % 7;
                PluginTuiAction::Redraw
            }
            KeyCode::Char('p') => {
                let Some(run) = self.selected_run_view() else {
                    return PluginTuiAction::None;
                };
                let kind =
                    if run.run.status == bcode_workflow_view_models::WorkflowRunStatus::Paused {
                        bcode_workflow_view_models::WorkflowActionKind::Resume
                    } else {
                        bcode_workflow_view_models::WorkflowActionKind::Pause
                    };
                let command = if kind == bcode_workflow_view_models::WorkflowActionKind::Resume {
                    "workflow.resume"
                } else {
                    "workflow.pause"
                };
                let run_id = run.run.run_id.clone();
                self.dispatch_exact_action(
                    kind,
                    bcode_workflow_view_models::WorkflowActionTarget::Run {
                        run_id: run_id.clone(),
                    },
                    plugin_command(command, format!("run_id={run_id}")),
                )
            }
            KeyCode::Char('c') => {
                let Some(run) = self.selected_run_view() else {
                    return PluginTuiAction::None;
                };
                let run_id = run.run.run_id.clone();
                let target = bcode_workflow_view_models::WorkflowActionTarget::Run {
                    run_id: run_id.clone(),
                };
                self.confirm_action(
                    "Cancel workflow run",
                    format!("Cancel exact run {run_id}? This cannot be undone."),
                    bcode_workflow_view_models::WorkflowActionKind::Cancel,
                    target,
                    plugin_command("workflow.cancel", format!("run_id={run_id}")),
                )
            }
            KeyCode::Char('a' | 'd') => {
                let approve = key.key == KeyCode::Char('a');
                let Some(run) = self.selected_run_view() else {
                    return PluginTuiAction::None;
                };
                if let Some(approval) = self.selected_mutation_approval().cloned() {
                    let kind = if approve {
                        bcode_workflow_view_models::WorkflowActionKind::ApproveMutation
                    } else {
                        bcode_workflow_view_models::WorkflowActionKind::DenyMutation
                    };
                    let target =
                        bcode_workflow_view_models::WorkflowActionTarget::MutationApproval {
                            approval_id: approval.approval_id.clone(),
                        };
                    let action = plugin_command(
                        if approve {
                            "workflow.approve-mutation"
                        } else {
                            "workflow.deny-mutation"
                        },
                        format!("approval_id={}", approval.approval_id),
                    );
                    if approve {
                        return self.dispatch_exact_action(kind, target, action);
                    }
                    return self.confirm_action(
                        "Deny mutation approval",
                        format!(
                            "Deny {} / {} operation {} on snapshot {}?",
                            approval.plugin_id,
                            approval.block_id,
                            approval.operation,
                            approval.workspace_snapshot
                        ),
                        kind,
                        target,
                        action,
                    );
                }
                let Some(wait) = self
                    .selected_wait(bcode_workflow_view_models::WorkflowWaitKind::Approval)
                    .cloned()
                else {
                    return PluginTuiAction::None;
                };
                let kind = if approve {
                    bcode_workflow_view_models::WorkflowActionKind::Approve
                } else {
                    bcode_workflow_view_models::WorkflowActionKind::Deny
                };
                let target = bcode_workflow_view_models::WorkflowActionTarget::Activation {
                    run_id: run.run.run_id.clone(),
                    node_id: wait.node_id.clone(),
                    activation_id: wait.activation_id.clone(),
                };
                let action = plugin_command(
                    if approve {
                        "workflow.approve"
                    } else {
                        "workflow.deny"
                    },
                    format!(
                        "run_id={} node_id={} activation_id={}",
                        run.run.run_id, wait.node_id, wait.activation_id
                    ),
                );
                self.dispatch_exact_action(kind, target, action)
            }
            KeyCode::Char('i') => {
                let Some(run) = self.selected_run_view() else {
                    return PluginTuiAction::None;
                };
                let Some(wait) = self
                    .selected_wait(bcode_workflow_view_models::WorkflowWaitKind::Input)
                    .cloned()
                else {
                    return PluginTuiAction::None;
                };
                self.input_form = Some(WorkflowInputForm::new(run.run.run_id.clone(), &wait));
                PluginTuiAction::Redraw
            }
            KeyCode::Char('r') => {
                let Some(run) = self.selected_run_view() else {
                    return PluginTuiAction::None;
                };
                let Some(attempt) = self
                    .selected_attempt()
                    .filter(|attempt| attempt.status == "failed")
                else {
                    return PluginTuiAction::None;
                };
                let run_id = run.run.run_id.clone();
                let target = bcode_workflow_view_models::WorkflowActionTarget::Attempt {
                    run_id: run_id.clone(),
                    node_id: attempt.node_id.clone(),
                    activation_id: attempt.activation_id.clone(),
                    attempt: attempt.attempt,
                };
                self.dispatch_exact_action(
                    bcode_workflow_view_models::WorkflowActionKind::RetryNode,
                    target,
                    plugin_command(
                        "workflow.retry-node",
                        format!(
                            "run_id={run_id} node_id={} activation_id={} failed_attempt={}",
                            attempt.node_id, attempt.activation_id, attempt.attempt
                        ),
                    ),
                )
            }
            KeyCode::Char('o') => self
                .selected_child_session_id
                .as_deref()
                .and_then(|session_id| session_id.parse().ok())
                .map_or(PluginTuiAction::None, |session_id| {
                    PluginTuiAction::OpenSession { session_id }
                }),
            KeyCode::Char('e') => self.definition_navigation_action(),
            _ => PluginTuiAction::None,
        }
    }

    fn render_confirmation(
        area: Rect,
        frame: &mut Frame<'_>,
        theme: WorkflowSurfaceTheme,
        confirmation: &PendingWorkflowConfirmation,
    ) {
        let pane = Pane::new()
            .title(Line::from(confirmation.title.clone()))
            .padding(Insets::new(1, 1, 1, 1))
            .styles(PaneStyles {
                background: Some(theme.canvas),
                border: theme.warning,
                focused_border: theme.error,
            });
        let mut state = PaneState::new(area);
        state.interaction = state.interaction.focused(true);
        pane.render(&state, frame);
        let inner = pane.inner_area(&state);
        frame.write_line(
            Rect::new(inner.x, inner.y, inner.width, 1),
            &Line::from_spans(vec![Span::styled(&confirmation.detail, theme.warning)]),
        );
        if inner.height > 2 {
            frame.write_line(
                Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1),
                &Line::from_spans(vec![
                    Span::styled("y/Enter", theme.focused),
                    Span::styled(" confirm  ", theme.text),
                    Span::styled("n/Esc", theme.focused),
                    Span::styled(" cancel", theme.text),
                ]),
            );
        }
    }

    #[allow(clippy::too_many_lines)]
    fn render_input_form(
        area: Rect,
        frame: &mut Frame<'_>,
        theme: WorkflowSurfaceTheme,
        form: &WorkflowInputForm,
    ) {
        if area.is_empty() {
            return;
        }
        let pane = Pane::new()
            .title(Line::from("Provide workflow input"))
            .padding(Insets::new(1, 1, 1, 1))
            .styles(PaneStyles {
                background: Some(theme.canvas),
                border: theme.component.border,
                focused_border: theme.focused,
            });
        let mut state = PaneState::new(area);
        state.interaction = state.interaction.focused(true);
        pane.render(&state, frame);
        let inner = pane.inner_area(&state);
        if inner.height == 0 {
            return;
        }
        frame.write_line(
            Rect::new(inner.x, inner.y, inner.width, 1),
            &Line::from_spans(vec![Span::styled(&form.prompt, theme.text)]),
        );
        if inner.height > 1 {
            frame.write_line(
                Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 1),
                &Line::from_spans(vec![Span::styled(
                    format!(
                        "Target · {} / {} / {}",
                        form.run_id, form.node_id, form.activation_id
                    ),
                    theme.muted,
                )]),
            );
        }
        if inner.height > 2 {
            frame.write_line(
                Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1),
                &Line::from_spans(vec![Span::styled(
                    format!(
                        "Expected · {}",
                        form.expected_schema.as_ref().map_or_else(
                            || "any JSON value".to_string(),
                            serde_json::Value::to_string
                        )
                    ),
                    theme.muted,
                )]),
            );
        }
        let editor_y = inner.y.saturating_add(3);
        let footer_rows = u16::from(form.error.is_some()).saturating_add(1);
        let editor_height = inner
            .bottom()
            .saturating_sub(editor_y)
            .saturating_sub(footer_rows)
            .max(1);
        let editor_area = Rect::new(inner.x, editor_y, inner.width, editor_height);
        let styles = TextInputBoxStyles {
            text: theme.text,
            focused_text: theme.focused,
            disabled_text: theme.muted,
            placeholder: theme.muted,
            selection: theme.selected,
            border: theme.component.border,
            focused_border: theme.focused,
            background: theme.canvas,
            focused_background: theme.canvas,
            disabled_background: theme.canvas,
        };
        if form.fields.is_empty() {
            let mut editor = form.editor.clone();
            TextInputBox::new(TextInputPolicy::chat_composer())
                .label("JSON input")
                .required(true)
                .help("Complex schema JSON fallback")
                .policy(TextInputBoxPolicy::field().focused(true).rows(3, Some(8)))
                .styles(styles)
                .error(form.error.as_deref().unwrap_or_default())
                .render(editor_area, &mut editor, frame);
        } else {
            let row_height = 3_u16;
            for (index, field) in form.fields.iter().enumerate() {
                let row = u16::try_from(index).unwrap_or(u16::MAX);
                let y = editor_y.saturating_add(row.saturating_mul(row_height));
                if y >= inner.bottom().saturating_sub(footer_rows) {
                    break;
                }
                let mut editor = field.editor.clone();
                let kind = match field.kind {
                    WorkflowInputFieldKind::String => "text",
                    WorkflowInputFieldKind::Boolean => "true/false",
                    WorkflowInputFieldKind::Integer => "integer",
                    WorkflowInputFieldKind::Number => "number",
                };
                TextInputBox::new(TextInputPolicy::chat_composer())
                    .label(&field.name)
                    .required(field.required)
                    .help(kind)
                    .policy(
                        TextInputBoxPolicy::field()
                            .focused(index == form.focused_field)
                            .rows(1, Some(1)),
                    )
                    .styles(styles)
                    .render(
                        Rect::new(
                            inner.x,
                            y,
                            inner.width,
                            row_height.min(inner.bottom().saturating_sub(y)),
                        ),
                        &mut editor,
                        frame,
                    );
            }
        }
        let mut footer_y = inner.bottom().saturating_sub(1);
        if let Some(error) = &form.error {
            footer_y = footer_y.saturating_sub(1);
            frame.write_line(
                Rect::new(inner.x, footer_y, inner.width, 1),
                &Line::from_spans(vec![Span::styled(error, theme.error)]),
            );
        }
        frame.write_line(
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
            &Line::from_spans(vec![
                Span::styled("Ctrl+Enter", theme.focused),
                Span::styled(" submit  ", theme.text),
                Span::styled("Esc", theme.focused),
                Span::styled(" cancel", theme.text),
            ]),
        );
    }

    fn render_themed(
        &self,
        area: Rect,
        frame: &mut Frame<'_>,
        theme: Option<bcode_plugin_sdk::tui::PluginTuiTheme>,
    ) {
        let theme = WorkflowSurfaceTheme::resolve(theme);
        frame.fill(area, " ", theme.canvas);
        let pane = Pane::new()
            .title(Line::from_spans(vec![Span::styled(
                "Workflow Status",
                theme.focused.add_modifier(Modifier::BOLD),
            )]))
            .padding(Insets::new(1, 1, 1, 1))
            .styles(PaneStyles {
                background: Some(theme.canvas),
                border: theme.component.border,
                focused_border: theme.focused,
            });
        let pane_state = PaneState::new(area);
        pane.render(&pane_state, frame);
        let content = pane.inner_area(&pane_state);
        if let Some(confirmation) = &self.pending_confirmation {
            Self::render_confirmation(content, frame, theme, confirmation);
            return;
        }
        if let Some(form) = &self.input_form {
            Self::render_input_form(content, frame, theme, form);
            return;
        }
        if self.subscription_requested || self.catalog.is_some() {
            self.render_workspace(content, frame, theme);
            return;
        }
        let rows = surface_lines(&self.options, self.selected_approval)
            .into_iter()
            .map(|row| Line::from_spans(vec![Span::styled(row, theme.text)]))
            .collect::<Vec<_>>();
        TextView::new(&rows)
            .policy(TextViewPolicy::bare())
            .styles(TextViewStyles {
                text: theme.text,
                empty: theme.component.muted,
                background: theme.canvas,
            })
            .empty("Workflow status unavailable")
            .render(content, &self.text_view, frame);
    }
}

impl PluginTuiSurface for WorkflowStatusSurface {
    fn id(&self) -> &'static str {
        "bcode.workflow-status"
    }

    fn title(&self) -> &'static str {
        "Workflow Status"
    }

    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        self.render_themed(area, frame, None);
    }

    fn render_with_theme(
        &mut self,
        area: Rect,
        frame: &mut Frame<'_>,
        theme: Option<bcode_plugin_sdk::tui::PluginTuiTheme>,
    ) {
        self.render_themed(area, frame, theme);
    }

    fn attach_updates(&mut self, updates: PluginTuiSurfaceUpdateReceiver) {
        self.updates = Some(updates);
        self.live_status = "subscribed".to_string();
    }

    #[allow(clippy::too_many_lines)]
    fn poll(&mut self, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        if !self.subscription_requested {
            self.subscription_requested = true;
            return PluginTuiAction::SubscribeWorkflowRuns;
        }
        let Some(updates) = self.updates.as_mut() else {
            return PluginTuiAction::None;
        };
        let mut changed = false;
        while let Ok(update) = updates.try_recv() {
            changed = true;
            match update {
                PluginTuiSurfaceUpdate::WorkflowCatalogLoading { stale } => {
                    self.catalog_loading = true;
                    self.catalog_stale = stale;
                    self.catalog_error = None;
                }
                PluginTuiSurfaceUpdate::WorkflowCatalog(catalog) => {
                    if let Err(error) = catalog.validate_version() {
                        self.live_status = error.to_string();
                        continue;
                    }
                    let previous_run_id = self.selected_run_id.clone();
                    self.selected_run_id = previous_run_id
                        .as_ref()
                        .filter(|run_id| catalog.runs.iter().any(|run| &run.run_id == *run_id))
                        .cloned()
                        .or_else(|| catalog.runs.first().map(|run| run.run_id.clone()));
                    if self.selected_run_id != previous_run_id {
                        self.selected_node_id = None;
                        self.selected_wait_id = None;
                        self.selected_approval_id = None;
                        self.selected_attempt_id = None;
                        self.selected_output_id = None;
                        self.selected_child_session_id = None;
                        self.detail_loading_run_id = self.selected_run_id.clone();
                    }
                    self.catalog_loading = false;
                    self.catalog_stale = false;
                    self.catalog_error = None;
                    self.catalog = Some(catalog);
                    self.live_status = "live".to_string();
                }
                PluginTuiSurfaceUpdate::WorkflowCatalogPage(page) => {
                    if let Err(error) = page.validate_version() {
                        self.live_status = error.to_string();
                        continue;
                    }
                    if let Some(catalog) = self.catalog.as_mut()
                        && catalog.filter == page.filter
                        && catalog.sort == page.sort
                        && catalog.search == page.search
                        && catalog.group == page.group
                    {
                        let known = catalog
                            .runs
                            .iter()
                            .map(|run| run.run_id.clone())
                            .collect::<BTreeSet<_>>();
                        let additions = page
                            .runs
                            .into_iter()
                            .filter(|run| !known.contains(&run.run_id))
                            .collect::<Vec<_>>();
                        catalog.runs.extend(additions);
                        catalog.next_cursor = page.next_cursor;
                        catalog.has_more = page.has_more;
                    }
                    self.catalog_loading = false;
                    self.catalog_stale = false;
                    self.catalog_error = None;
                    self.live_status = "live".to_string();
                }
                PluginTuiSurfaceUpdate::WorkflowCatalogError { message } => {
                    self.catalog_loading = false;
                    self.catalog_stale = self.catalog.is_some();
                    self.catalog_error = Some(message);
                }
                PluginTuiSurfaceUpdate::WorkflowRun(view) => {
                    if let Err(error) = view.validate_version() {
                        self.live_status = error.to_string();
                        continue;
                    }
                    let run_id = view.run.run_id.clone();
                    if self.runs.get(&run_id).is_some_and(|current| {
                        workflow_run_status_is_terminal(current.run.status)
                            && !workflow_run_status_is_terminal(view.run.status)
                    }) {
                        self.live_status =
                            "ignored stale workflow update after terminal state".to_string();
                        continue;
                    }
                    if self.selected_run_id.is_none() {
                        self.selected_run_id = Some(run_id.clone());
                    }
                    if self.selected_run_id.as_deref() == Some(run_id.as_str()) {
                        self.detail_loading_run_id = None;
                        self.detail_errors.remove(&run_id);
                        self.pending_action_target = None;
                        self.inline_error = None;
                        self.input_form = None;
                        self.selected_node_id = self
                            .selected_node_id
                            .take()
                            .filter(|node_id| {
                                view.nodes.iter().any(|node| &node.node_id == node_id)
                            })
                            .or_else(|| view.nodes.first().map(|node| node.node_id.clone()));
                        self.selected_wait_id = self
                            .selected_wait_id
                            .take()
                            .filter(|(node_id, activation_id)| {
                                view.waits.iter().any(|wait| {
                                    &wait.node_id == node_id && &wait.activation_id == activation_id
                                })
                            })
                            .or_else(|| {
                                view.waits
                                    .first()
                                    .map(|wait| (wait.node_id.clone(), wait.activation_id.clone()))
                            });
                        self.selected_approval_id = self
                            .selected_approval_id
                            .take()
                            .filter(|approval_id| {
                                view.mutation_approvals
                                    .iter()
                                    .any(|approval| &approval.approval_id == approval_id)
                            })
                            .or_else(|| {
                                view.mutation_approvals
                                    .first()
                                    .map(|approval| approval.approval_id.clone())
                            });
                        self.selected_attempt_id = self
                            .selected_attempt_id
                            .take()
                            .filter(|(node_id, activation_id, attempt)| {
                                view.attempts.iter().any(|candidate| {
                                    &candidate.node_id == node_id
                                        && &candidate.activation_id == activation_id
                                        && candidate.attempt == *attempt
                                })
                            })
                            .or_else(|| {
                                view.attempts.first().map(|attempt| {
                                    (
                                        attempt.node_id.clone(),
                                        attempt.activation_id.clone(),
                                        attempt.attempt,
                                    )
                                })
                            });
                        self.selected_output_id = self
                            .selected_output_id
                            .take()
                            .filter(|output_id| {
                                view.outputs
                                    .iter()
                                    .any(|output| &output.output_id == output_id)
                            })
                            .or_else(|| {
                                view.outputs.first().map(|output| output.output_id.clone())
                            });
                        self.selected_child_session_id = self
                            .selected_child_session_id
                            .take()
                            .filter(|session_id| {
                                view.child_sessions
                                    .iter()
                                    .any(|session| &session.session_id == session_id)
                            })
                            .or_else(|| {
                                view.child_sessions
                                    .first()
                                    .map(|session| session.session_id.clone())
                            });
                    }
                    self.runs.clear();
                    self.runs.insert(run_id, *view);
                    self.live_status = "live".to_string();
                }
                PluginTuiSurfaceUpdate::WorkflowRunLoading { run_id } => {
                    self.detail_errors.remove(&run_id);
                    self.detail_loading_run_id = Some(run_id);
                }
                PluginTuiSurfaceUpdate::WorkflowRunError { run_id, message } => {
                    if self.detail_loading_run_id.as_deref() == Some(run_id.as_str()) {
                        self.detail_loading_run_id = None;
                    }
                    self.inline_error = Some(format!("Run refresh failed: {message}"));
                    self.detail_errors.insert(run_id, message);
                }
                PluginTuiSurfaceUpdate::SelectWorkflowRun { .. } => {}
                PluginTuiSurfaceUpdate::ResyncRequired => {
                    self.live_status = "resync required; reopen /workflow".to_string();
                }
                PluginTuiSurfaceUpdate::Disconnected { message } => {
                    self.live_status = format!("live updates unavailable: {message}");
                }
            }
        }
        if changed {
            PluginTuiAction::Redraw
        } else {
            PluginTuiAction::None
        }
    }

    fn handle_event(&mut self, event: &Event, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        if self.subscription_requested || self.catalog.is_some() {
            return self.handle_control_center_event(event);
        }
        let approval_count =
            mutation_approvals(&self.options).map_or(0, <[serde_json::Value]>::len);
        match event {
            Event::Key(key) if matches!(key.key, KeyCode::Escape | KeyCode::Char('q')) => {
                PluginTuiAction::Close { outcome: None }
            }
            Event::Key(key) if matches!(key.key, KeyCode::Down | KeyCode::Char('j')) => {
                if approval_count > 0 {
                    self.selected_approval = (self.selected_approval + 1).min(approval_count - 1);
                    PluginTuiAction::Redraw
                } else {
                    PluginTuiAction::None
                }
            }
            Event::Key(key) if matches!(key.key, KeyCode::Up | KeyCode::Char('k')) => {
                self.selected_approval = self.selected_approval.saturating_sub(1);
                PluginTuiAction::Redraw
            }
            Event::Key(key) if matches!(key.key, KeyCode::Char('a' | 'd')) => {
                let approve = key.key == KeyCode::Char('a');
                mutation_approval_command(&self.options, self.selected_approval, approve).map_or(
                    PluginTuiAction::None,
                    |command| {
                        let mut parts = command.splitn(3, ' ');
                        let _prefix = parts.next();
                        let action = parts.next().unwrap_or_default();
                        let arguments = parts.next().map(str::to_string);
                        PluginTuiAction::InvokePluginCommand {
                            plugin_id: "bcode.workflow".to_string(),
                            command_id: format!("workflow.{action}"),
                            arguments,
                        }
                    },
                )
            }
            _ => PluginTuiAction::None,
        }
    }
}

const fn next_catalog_filter(
    filter: bcode_workflow_view_models::WorkflowCatalogFilter,
) -> bcode_workflow_view_models::WorkflowCatalogFilter {
    use bcode_workflow_view_models::WorkflowCatalogFilter as Filter;
    match filter {
        Filter::All => Filter::Active,
        Filter::Active => Filter::NeedsAttention,
        Filter::NeedsAttention => Filter::Failed,
        Filter::Failed => Filter::Completed,
        Filter::Completed => Filter::All,
    }
}

const fn next_catalog_sort(
    sort: bcode_workflow_view_models::WorkflowCatalogSort,
) -> bcode_workflow_view_models::WorkflowCatalogSort {
    use bcode_workflow_view_models::WorkflowCatalogSort as Sort;
    match sort {
        Sort::UpdatedAt => Sort::CreatedAt,
        Sort::CreatedAt => Sort::Status,
        Sort::Status => Sort::UpdatedAt,
    }
}

const fn next_catalog_group(
    group: bcode_workflow_view_models::WorkflowCatalogGroup,
) -> bcode_workflow_view_models::WorkflowCatalogGroup {
    use bcode_workflow_view_models::WorkflowCatalogGroup as Group;
    match group {
        Group::None => Group::AuthoredWorkflow,
        Group::AuthoredWorkflow => Group::Definition,
        Group::Definition => Group::None,
    }
}

const fn workflow_node_glyph(
    status: &bcode_workflow_view_models::WorkflowNodeStatus,
) -> &'static str {
    match status {
        bcode_workflow_view_models::WorkflowNodeStatus::NotStarted => "○",
        bcode_workflow_view_models::WorkflowNodeStatus::Pending => "◌",
        bcode_workflow_view_models::WorkflowNodeStatus::Running => "●",
        bcode_workflow_view_models::WorkflowNodeStatus::WaitingInput
        | bcode_workflow_view_models::WorkflowNodeStatus::WaitingApproval
        | bcode_workflow_view_models::WorkflowNodeStatus::WaitingMutationApproval => "◆",
        bcode_workflow_view_models::WorkflowNodeStatus::Completed => "✓",
        bcode_workflow_view_models::WorkflowNodeStatus::Failed => "✕",
        bcode_workflow_view_models::WorkflowNodeStatus::Cancelled => "⊘",
        bcode_workflow_view_models::WorkflowNodeStatus::Skipped => "–",
        bcode_workflow_view_models::WorkflowNodeStatus::RepairRequired => "!",
        bcode_workflow_view_models::WorkflowNodeStatus::Unknown(_) => "?",
    }
}

const fn workflow_node_kind_label(
    kind: bcode_workflow_view_models::WorkflowNodeKind,
) -> &'static str {
    match kind {
        bcode_workflow_view_models::WorkflowNodeKind::Task => "task",
        bcode_workflow_view_models::WorkflowNodeKind::Agent => "agent",
        bcode_workflow_view_models::WorkflowNodeKind::Branch => "branch",
        bcode_workflow_view_models::WorkflowNodeKind::Repeat => "repeat",
        bcode_workflow_view_models::WorkflowNodeKind::Retry => "retry",
        bcode_workflow_view_models::WorkflowNodeKind::Parallel => "parallel",
        bcode_workflow_view_models::WorkflowNodeKind::FanOut => "fan-out",
        bcode_workflow_view_models::WorkflowNodeKind::PluginBlock => "plugin",
        bcode_workflow_view_models::WorkflowNodeKind::Input => "input",
        bcode_workflow_view_models::WorkflowNodeKind::Approval => "approval",
        bcode_workflow_view_models::WorkflowNodeKind::WorkflowCall => "workflow",
    }
}

fn workflow_graph_generations(
    run: &bcode_workflow_view_models::WorkflowRunView,
) -> Option<Vec<Vec<usize>>> {
    let node_indexes = run
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.node_id.as_str(), index))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut incoming = vec![0_usize; run.nodes.len()];
    let mut outgoing = vec![Vec::<usize>::new(); run.nodes.len()];
    for edge in &run.edges {
        if edge.kind == "back" || edge.kind == "retry" {
            continue;
        }
        let (&from, &to) = (
            node_indexes.get(edge.from.as_str())?,
            node_indexes.get(edge.to.as_str())?,
        );
        outgoing[from].push(to);
        incoming[to] = incoming[to].saturating_add(1);
    }
    let mut frontier = incoming
        .iter()
        .enumerate()
        .filter_map(|(index, count)| (*count == 0).then_some(index))
        .collect::<Vec<_>>();
    let mut generations = Vec::new();
    let mut visited = 0_usize;
    while !frontier.is_empty() {
        frontier.sort_unstable();
        visited = visited.saturating_add(frontier.len());
        generations.push(frontier.clone());
        let mut next = Vec::new();
        for index in frontier {
            for target in &outgoing[index] {
                incoming[*target] = incoming[*target].saturating_sub(1);
                if incoming[*target] == 0 {
                    next.push(*target);
                }
            }
        }
        frontier = next;
    }
    (visited == run.nodes.len()).then_some(generations)
}

#[allow(clippy::too_many_lines)]
fn workflow_graph_lines(
    run: &bcode_workflow_view_models::WorkflowRunView,
    selected_node_id: Option<&str>,
    width: u16,
    height: u16,
    descendants_expanded: bool,
    theme: WorkflowSurfaceTheme,
) -> Vec<Line> {
    let selected_index = selected_node_id
        .and_then(|node_id| run.nodes.iter().position(|node| node.node_id == node_id));
    let generations = workflow_graph_generations(run);
    let linear_fallback = width < 42 || generations.is_none();
    let ordered =
        generations.unwrap_or_else(|| (0..run.nodes.len()).map(|index| vec![index]).collect());
    let mut lines = Vec::new();
    if linear_fallback && !run.edges.is_empty() {
        lines.push(Line::from_spans(vec![Span::styled(
            "Linear graph fallback",
            theme.muted,
        )]));
    }
    for (generation, indexes) in ordered.iter().enumerate() {
        if !linear_fallback {
            lines.push(Line::from_spans(vec![Span::styled(
                format!("Stage {}", generation.saturating_add(1)),
                theme.muted,
            )]));
        }
        for index in indexes {
            let node = &run.nodes[*index];
            let selected = selected_index == Some(*index);
            lines.push(Line::from_spans(vec![
                Span::styled(if selected { "▶ " } else { "  " }, theme.focused),
                Span::styled(
                    format!("{} {}", workflow_node_glyph(&node.status), node.name),
                    workflow_node_style(&node.status, theme),
                ),
                Span::styled(
                    format!(
                        "  {} · {:?}",
                        workflow_node_kind_label(node.kind),
                        node.status
                    ),
                    theme.muted,
                ),
            ]));
            for edge in run.edges.iter().filter(|edge| edge.from == node.node_id) {
                let connector = match edge.kind.as_str() {
                    "conditional" => "├?→",
                    "back" => "↶",
                    "retry" => "↻",
                    _ => "└→",
                };
                lines.push(Line::from_spans(vec![Span::styled(
                    format!("    {connector} {} ({})", edge.to, edge.kind),
                    if matches!(edge.kind.as_str(), "back" | "retry") {
                        theme.warning
                    } else {
                        theme.muted
                    },
                )]));
            }
        }
    }
    if !run.descendant_runs.is_empty() || !run.child_sessions.is_empty() {
        lines.push(Line::from_spans(vec![Span::styled(
            format!(
                "{} Nested · {} descendant runs · {} child sessions",
                if descendants_expanded { "▾" } else { "▸" },
                run.descendant_runs.len(),
                run.child_sessions.len()
            ),
            theme.info,
        )]));
        if descendants_expanded {
            for descendant in &run.descendant_runs {
                lines.push(Line::from_spans(vec![
                    Span::styled(
                        format!("  {}{} ", "  ".repeat(descendant.depth as usize), "↳"),
                        theme.muted,
                    ),
                    Span::styled(descendant.run.display_title.clone(), theme.text),
                    Span::styled(
                        format!(
                            " · {:?} · parent node {}",
                            descendant.run.status, descendant.parent_node_id
                        ),
                        workflow_run_style(descendant.run.status, theme),
                    ),
                ]));
            }
            for session in &run.child_sessions {
                lines.push(Line::from_spans(vec![
                    Span::styled("  ◇ session ", theme.info),
                    Span::styled(session.session_id.clone(), theme.text),
                    Span::styled(
                        format!(" · {} attempt {}", session.node_id, session.attempt),
                        theme.muted,
                    ),
                ]));
            }
        }
    }
    if let Some(selected) = selected_index
        && lines.len() > usize::from(height)
    {
        let selected_line = lines.iter().position(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains(&run.nodes[selected].name))
        });
        if let Some(selected_line) = selected_line {
            let visible = usize::from(height.max(1));
            let start = selected_line.saturating_sub(visible / 2);
            lines = lines.into_iter().skip(start).take(visible).collect();
        }
    }
    lines
}

const fn workflow_run_style(
    status: bcode_workflow_view_models::WorkflowRunStatus,
    theme: WorkflowSurfaceTheme,
) -> Style {
    match status {
        bcode_workflow_view_models::WorkflowRunStatus::Running => theme.info,
        bcode_workflow_view_models::WorkflowRunStatus::Paused
        | bcode_workflow_view_models::WorkflowRunStatus::Cancelled => theme.muted,
        bcode_workflow_view_models::WorkflowRunStatus::Completed => theme.success,
        bcode_workflow_view_models::WorkflowRunStatus::Failed
        | bcode_workflow_view_models::WorkflowRunStatus::RepairRequired => theme.error,
    }
}

const fn workflow_node_style(
    status: &bcode_workflow_view_models::WorkflowNodeStatus,
    theme: WorkflowSurfaceTheme,
) -> Style {
    match status {
        bcode_workflow_view_models::WorkflowNodeStatus::Running
        | bcode_workflow_view_models::WorkflowNodeStatus::Pending => theme.info,
        bcode_workflow_view_models::WorkflowNodeStatus::Completed => theme.success,
        bcode_workflow_view_models::WorkflowNodeStatus::WaitingInput
        | bcode_workflow_view_models::WorkflowNodeStatus::WaitingApproval
        | bcode_workflow_view_models::WorkflowNodeStatus::WaitingMutationApproval => theme.warning,
        bcode_workflow_view_models::WorkflowNodeStatus::Failed
        | bcode_workflow_view_models::WorkflowNodeStatus::RepairRequired => theme.error,
        bcode_workflow_view_models::WorkflowNodeStatus::NotStarted
        | bcode_workflow_view_models::WorkflowNodeStatus::Cancelled
        | bcode_workflow_view_models::WorkflowNodeStatus::Skipped
        | bcode_workflow_view_models::WorkflowNodeStatus::Unknown(_) => theme.muted,
    }
}

fn inspector_lines(
    run: Option<&bcode_workflow_view_models::WorkflowRunView>,
    tab: usize,
    theme: WorkflowSurfaceTheme,
) -> Vec<Line> {
    let Some(run) = run else {
        return vec![Line::from("Loading selected run…")];
    };
    match tab {
        0 => inspector_overview_lines(run),
        1 => inspector_input_lines(run),
        2 => inspector_output_lines(run, theme),
        3 => inspector_attempt_lines(run),
        4 => inspector_approval_lines(run),
        5 => inspector_session_lines(run),
        _ => inspector_definition_lines(run),
    }
}

fn inspector_overview_lines(run: &bcode_workflow_view_models::WorkflowRunView) -> Vec<Line> {
    let mut lines = vec![
        Line::from(run.run.display_title.clone()),
        Line::from(format!("Run: {}", run.run.run_id)),
        Line::from(format!("Status: {:?}", run.run.status)),
        Line::from(format!(
            "Created: {} · Updated: {}",
            run.run.created_at_ms, run.run.updated_at_ms
        )),
        Line::from(format!(
            "Progress: {}/{} completed · {} active · {} blocked · {} failed",
            run.run.progress.completed,
            run.run.progress.total_nodes,
            run.run.progress.active,
            run.run.progress.blocked,
            run.run.progress.failed
        )),
        Line::from(format!(
            "Definition: {} v{}",
            run.run.definition_id, run.run.definition_version
        )),
        Line::from(format!(
            "Parent: {} · descendants: {}",
            run.run.parent_run_id.as_deref().unwrap_or("root"),
            run.run.descendant_count
        )),
        Line::from(format!("Health: {:?}", run.health)),
    ];
    if let Some(terminal) = &run.terminal {
        lines.push(Line::from(format!("Terminal: {terminal:?}")));
    }
    lines
}

fn inspector_input_lines(run: &bcode_workflow_view_models::WorkflowRunView) -> Vec<Line> {
    if run.waits.is_empty() {
        return vec![Line::from("No pending workflow inputs or approvals")];
    }
    run.waits
        .iter()
        .flat_map(|wait| {
            vec![
                Line::from(format!(
                    "{:?} · {} · {}",
                    wait.kind, wait.node_id, wait.activation_id
                )),
                Line::from(format!("Prompt: {}", wait.prompt)),
                Line::from(format!(
                    "Expected schema: {}",
                    wait.expected_schema
                        .as_ref()
                        .map_or_else(|| "none".to_string(), serde_json::Value::to_string)
                )),
                Line::from(format!(
                    "Current input: {}",
                    wait.input
                        .as_ref()
                        .map_or_else(|| "none".to_string(), serde_json::Value::to_string)
                )),
            ]
        })
        .collect()
}

fn inspector_output_lines(
    run: &bcode_workflow_view_models::WorkflowRunView,
    theme: WorkflowSurfaceTheme,
) -> Vec<Line> {
    if run.outputs.is_empty() {
        return vec![Line::from("No outputs")];
    }
    run.outputs
        .iter()
        .flat_map(|output| {
            let value = match &output.value {
                bcode_workflow_view_models::WorkflowOutputValue::Resolved { value } => value,
                bcode_workflow_view_models::WorkflowOutputValue::Unresolved => {
                    return vec![Line::from(format!(
                        "{} · {} v{} · unresolved",
                        output.node_id, output.schema_id, output.schema_version
                    ))];
                }
            };
            let mut lines = vec![
                Line::from(format!(
                    "{} · {} v{}",
                    output.node_id, output.schema_id, output.schema_version
                )),
                Line::from(format!("Output id: {}", output.output_id)),
            ];
            if let Some(verdict) = value.get("verdict") {
                let verdict_text = verdict
                    .as_str()
                    .map_or_else(|| verdict.to_string(), str::to_string);
                let style = match verdict_text.to_ascii_lowercase().as_str() {
                    "pass" | "approved" | "success" => theme.success,
                    "fail" | "failed" | "rejected" => theme.error,
                    _ => theme.warning,
                };
                lines.push(Line::from_spans(vec![
                    Span::styled("Verdict: ", theme.focused),
                    Span::styled(verdict_text, style.add_modifier(Modifier::BOLD)),
                ]));
            }
            if let Some(findings) = value.get("findings").and_then(serde_json::Value::as_array) {
                lines.push(Line::from(format!("Findings ({})", findings.len())));
                lines.extend(
                    findings
                        .iter()
                        .map(|finding| Line::from(format!("  • {finding}"))),
                );
            } else {
                lines.push(Line::from(value.to_string()));
            }
            if let Some(reference) = &output.artifact_reference {
                lines.push(Line::from(format!("Artifact: {reference}")));
            }
            lines
        })
        .collect()
}

fn inspector_attempt_lines(run: &bcode_workflow_view_models::WorkflowRunView) -> Vec<Line> {
    if run.attempts.is_empty() {
        return vec![Line::from("No dispatch attempts")];
    }
    run.attempts
        .iter()
        .flat_map(|attempt| {
            vec![
                Line::from(format!(
                    "#{} {} · {}",
                    attempt.attempt, attempt.node_id, attempt.status
                )),
                Line::from(format!("Dispatch: {}", attempt.dispatch_identity)),
                Line::from(format!(
                    "Prepared: {} · terminal: {} · receipt: {}",
                    attempt.prepared_at_ms,
                    attempt
                        .terminal_at_ms
                        .map_or_else(|| "pending".to_string(), |value| value.to_string()),
                    if attempt.has_receipt { "yes" } else { "no" }
                )),
            ]
        })
        .collect()
}

fn inspector_approval_lines(run: &bcode_workflow_view_models::WorkflowRunView) -> Vec<Line> {
    let mut lines = run
        .waits
        .iter()
        .filter(|wait| wait.kind == bcode_workflow_view_models::WorkflowWaitKind::Approval)
        .map(|wait| {
            Line::from(format!(
                "Approval · {} · {} · {}",
                wait.node_id, wait.activation_id, wait.prompt
            ))
        })
        .collect::<Vec<_>>();
    for approval in &run.mutation_approvals {
        lines.extend([
            Line::from(format!(
                "Mutation · {} · {} / {} v{}",
                approval.approval_id, approval.plugin_id, approval.block_id, approval.block_version
            )),
            Line::from(format!(
                "Operation: {} · effect: {:?}",
                approval.operation, approval.effect
            )),
            Line::from(format!("Input: {}", approval.input_summary)),
            Line::from(format!(
                "Resources: {}",
                approval
                    .resource_claims
                    .iter()
                    .map(|claim| format!("{}:{}", claim.resource, claim.access))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            Line::from(format!("Snapshot: {}", approval.workspace_snapshot)),
        ]);
        if let Some(warning) = &approval.reconciliation_warning {
            lines.push(Line::from(format!("Warning: {warning}")));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from("No pending approvals"));
    }
    lines
}

fn inspector_session_lines(run: &bcode_workflow_view_models::WorkflowRunView) -> Vec<Line> {
    if run.child_sessions.is_empty() {
        return vec![Line::from("No child sessions")];
    }
    run.child_sessions
        .iter()
        .map(|session| {
            Line::from(format!(
                "{} · node {} · attempt {} · activation {}",
                session.session_id, session.node_id, session.attempt, session.activation_id
            ))
        })
        .collect()
}

fn inspector_definition_lines(run: &bcode_workflow_view_models::WorkflowRunView) -> Vec<Line> {
    let mut lines = vec![Line::from(format!(
        "Definition: {} v{}",
        run.run.definition_id, run.run.definition_version
    ))];
    match &run.run.definition_disposition {
        bcode_workflow_view_models::WorkflowDefinitionDisposition::Published {
            workflow_id,
            revision,
            editable_draft_id,
        } => {
            lines.push(Line::from(format!(
                "Published source: {workflow_id} revision {revision}"
            )));
            lines.push(Line::from(editable_draft_id.as_ref().map_or_else(
                || "Immutable revision · fork to edit".to_string(),
                |draft_id| format!("Editable draft: {draft_id}"),
            )));
        }
        bcode_workflow_view_models::WorkflowDefinitionDisposition::CompiledOnly => {
            lines.push(Line::from(
                "Externally managed compiled definition · view only",
            ));
        }
    }
    lines
}

#[cfg(test)]
#[allow(clippy::too_many_lines)]
fn workflow_view_lines(
    catalog: Option<&bcode_workflow_view_models::WorkflowCatalogView>,
    runs: &std::collections::BTreeMap<String, bcode_workflow_view_models::WorkflowRunView>,
    live_status: &str,
    selected_run_id: Option<&str>,
    selected_node_id: Option<&str>,
    input_buffer: Option<&str>,
) -> Vec<String> {
    let mut lines = vec![
        "Workflow control center".to_string(),
        format!("Live status · {live_status}"),
        String::new(),
    ];
    let Some(catalog) = catalog else {
        lines.push("Loading workflow catalog…".to_string());
        return lines;
    };
    lines.push(format!("Runs ({})", catalog.runs.len()));
    for item in &catalog.runs {
        let selected = selected_run_id == Some(item.run_id.as_str());
        let run_marker = if selected { ">" } else { " " };
        let progress = &item.progress;
        lines.push(format!(
            "{run_marker} {} · {:?} · {}/{} nodes{}",
            item.display_title,
            item.status,
            progress.completed,
            progress.total_nodes,
            if item.attention.needs_attention() {
                " · needs attention"
            } else {
                ""
            }
        ));
        lines.push(format!(
            "    {} · {} v{}",
            item.run_id, item.definition_id, item.definition_version
        ));
        if !selected {
            continue;
        }
        if let Some(run) = runs.get(&item.run_id) {
            lines.push(format!("    Nodes ({})", run.nodes.len()));
            for node in &run.nodes {
                let node_marker = if selected_node_id == Some(node.node_id.as_str()) {
                    "▶"
                } else {
                    " "
                };
                lines.push(format!(
                    "    {node_marker} {} · {:?} · {:?}",
                    node.name, node.kind, node.status
                ));
            }
            lines.push(format!("    Waits ({})", run.waits.len()));
            for wait in &run.waits {
                lines.push(format!("      {:?} · {}", wait.kind, wait.prompt));
            }
            lines.push(format!(
                "    Mutation approvals ({})",
                run.mutation_approvals.len()
            ));
            for approval in &run.mutation_approvals {
                lines.push(format!(
                    "      {} / {} · {} · {}",
                    approval.plugin_id,
                    approval.block_id,
                    approval.operation,
                    approval.workspace_snapshot
                ));
            }
            lines.push(format!("    Attempts ({})", run.attempts.len()));
            lines.push(format!("    Outputs ({})", run.outputs.len()));
            for output in &run.outputs {
                lines.push(format!(
                    "      {} · {} v{} · {}",
                    output.node_id,
                    output.schema_id,
                    output.schema_version,
                    match &output.value {
                        bcode_workflow_view_models::WorkflowOutputValue::Resolved { value } => {
                            value.to_string()
                        }
                        bcode_workflow_view_models::WorkflowOutputValue::Unresolved => {
                            "unresolved".to_string()
                        }
                    }
                ));
            }
            if let Some(terminal) = &run.terminal {
                lines.push(format!("    Terminal · {terminal:?}"));
            }
            if !matches!(
                run.health,
                bcode_workflow_view_models::WorkflowProjectionHealth::Current
            ) {
                lines.push(format!("    Health · {:?}", run.health));
            }
        } else {
            lines.push("    Loading bounded run projection…".to_string());
        }
    }
    lines.push(String::new());
    if let Some(input) = input_buffer {
        lines.push(format!("Input JSON › {input}"));
        lines.push("Enter submit · Esc cancel".to_string());
    } else {
        lines.push(
            "←/→ run · ↑/↓ node · m more · p pause/resume · c cancel · r retry · a/d approval · i input · o session"
                .to_string(),
        );
    }
    lines
}

fn plugin_command(command_id: &str, arguments: String) -> PluginTuiAction {
    PluginTuiAction::InvokePluginCommand {
        plugin_id: "bcode.workflow".to_string(),
        command_id: command_id.to_string(),
        arguments: Some(arguments),
    }
}

fn next_action(activation: &serde_json::Value) -> &'static str {
    match text(activation, "status") {
        "pending" => "dispatch",
        "running" => "await owner",
        "waiting_input" => "provide input",
        "waiting_approval" | "waiting_mutation_approval" => "approve or deny",
        "failed" => "inspect or explicit retry",
        "repair_required" => "doctor or repair",
        "completed" | "cancelled" | "skipped" => "none",
        _ => "inspect",
    }
}

fn mutation_approval_command(
    options: &serde_json::Value,
    selected_approval: usize,
    approve: bool,
) -> Option<String> {
    let approval_id = mutation_approvals(options)?
        .get(selected_approval)?
        .get("approval_id")?
        .as_str()?;
    let action = if approve {
        "approve-mutation"
    } else {
        "deny-mutation"
    };
    Some(format!("/workflow {action} approval_id={approval_id}"))
}

fn mutation_approvals(options: &serde_json::Value) -> Option<&[serde_json::Value]> {
    options
        .get("mutation_approvals")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
}

#[allow(clippy::too_many_lines)]
fn surface_lines(options: &serde_json::Value, selected_approval: usize) -> Vec<String> {
    let command = options
        .get("command_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("workflow.status");
    let mut lines = vec![format!("Workflow · {command}"), String::new()];
    if let Some(definitions) = options
        .get("definitions")
        .and_then(serde_json::Value::as_array)
    {
        lines.push(format!("Definitions ({})", definitions.len()));
        for definition in definitions {
            lines.push(format!(
                "  {} v{}  {}",
                text(definition, "definition_id"),
                number(definition, "version"),
                short_checksum(text(definition, "checksum_sha256"))
            ));
        }
    }
    if let Some(blocks) = options.get("blocks").and_then(serde_json::Value::as_array) {
        lines.push(format!("Available plugin blocks ({})", blocks.len()));
        for block in blocks {
            lines.push(format!(
                "  {} v{} · {}",
                text(block, "block_id"),
                number(block, "block_version"),
                text(block, "plugin_id")
            ));
        }
    }
    if let Some(runs) = options.get("runs").and_then(serde_json::Value::as_array) {
        append_runs(&mut lines, runs);
    }
    if let Some(run) = options.get("run") {
        lines.push(format!(
            "Run {} · {} · {} v{}",
            text(run, "run_id"),
            text(run, "status"),
            text(run, "definition_id"),
            number(run, "definition_version")
        ));
    }
    if let Some(definition) = options.get("definition") {
        append_graph(&mut lines, definition);
    }
    append_named_rows(&mut lines, options, "activations", "Activations", |value| {
        format!(
            "  {} · generation {} · {} · next={}",
            text(value, "node_id"),
            number(value, "dependency_generation"),
            text(value, "status"),
            next_action(value)
        )
    });
    append_named_rows(&mut lines, options, "waits", "Waits", |value| {
        format!(
            "  {} · {} · {}",
            text(value, "node_id"),
            text(value, "kind"),
            text(value, "activation_id")
        )
    });
    append_mutation_approvals(&mut lines, options, selected_approval);
    append_named_rows(&mut lines, options, "attempts", "Attempts", |value| {
        format!(
            "  {} #{} · {} · {} · receipt={} · dispatch={}",
            text(value, "node_id"),
            number(value, "attempt"),
            text(value, "status"),
            text(value, "side_effect"),
            value
                .get("has_receipt")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            text(value, "dispatch_identity")
        )
    });
    append_named_rows(&mut lines, options, "decisions", "Decisions", |value| {
        format!(
            "  {} · {} · {}",
            text(value, "node_id"),
            text(value, "decision_type"),
            compact_json(value.get("value"))
        )
    });
    append_named_rows(&mut lines, options, "grants", "Grants", |value| {
        format!(
            "  {} · {} · scope={}",
            text(value, "grant_id"),
            text(value, "node_id"),
            value
                .get("scope")
                .map_or_else(|| "-".to_string(), serde_json::Value::to_string)
        )
    });
    append_named_rows(
        &mut lines,
        options,
        "resource_leases",
        "Resource leases",
        |value| {
            format!(
                "  {} · {} · {}",
                text(value, "node_id"),
                text(value, "mode"),
                text(value, "resource_key")
            )
        },
    );
    append_git_head_diagnostics(&mut lines, options);
    append_named_rows(
        &mut lines,
        options,
        "outputs",
        "Outputs/artifacts",
        |value| {
            format!(
                "  {} · {} · {} · checksum {}",
                text(value, "node_id"),
                text(value, "schema_id"),
                text(value, "artifact_reference"),
                short_checksum(text(value, "checksum_sha256"))
            )
        },
    );
    append_named_rows(
        &mut lines,
        options,
        "descendant_runs",
        "Composed workflow levels",
        |value| {
            let link = value.get("link").unwrap_or(&serde_json::Value::Null);
            let run = value.get("run").unwrap_or(&serde_json::Value::Null);
            format!(
                "  depth {} · {} · {} · parent={} · {} v{}",
                number(link, "depth"),
                text(run, "run_id"),
                text(run, "status"),
                text(link, "parent_run_id"),
                text(run, "definition_id"),
                number(run, "definition_version")
            )
        },
    );
    append_named_rows(
        &mut lines,
        options,
        "repeat_outcomes",
        "Runtime-owned counters",
        |value| {
            format!(
                "  {} · {}/{} · effective={} · {}",
                text(value, "node_id"),
                number(value, "iterations_completed"),
                number(value, "max_iterations"),
                number(value, "effective_iteration_bound"),
                text(value, "outcome")
            )
        },
    );
    append_named_rows(
        &mut lines,
        options,
        "child_sessions",
        "Child sessions",
        |value| format!("  {} · {}", text(value, "id"), text(value, "name")),
    );
    append_named_rows(&mut lines, options, "events", "Events", |value| {
        format!(
            "  {} · {}",
            number(value, "event_seq"),
            text(value, "event_type")
        )
    });
    if let Some(doctor) = options.get("doctor") {
        lines.push(format!(
            "Doctor · truncated={} · {} issue(s)",
            doctor
                .get("truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            doctor
                .get("issues")
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len)
        ));
        append_named_rows(&mut lines, doctor, "issues", "Doctor issues", |value| {
            format!("  {} · {}", text(value, "issue"), compact_json(Some(value)))
        });
    }
    if let Some(repair) = options.get("repair") {
        lines.push(format!(
            "Repair · {} · attempt={} · run={}",
            text(repair, "dispatch_identity"),
            text(repair, "attempt_status"),
            text(repair, "run_status")
        ));
    }
    if options.get("retry").is_some() {
        lines.push("Exact failed node retry admitted".to_string());
    }
    if options.get("resolution").is_some() {
        lines.push("Input resolution committed".to_string());
    }
    lines.extend([
        String::new(),
        "Graph/history data is bounded and daemon-backed".to_string(),
        "Esc/q close".to_string(),
    ]);
    lines
}

fn append_graph(lines: &mut Vec<String>, stored: &serde_json::Value) {
    let Some(definition_json) = stored
        .get("definition_json")
        .and_then(serde_json::Value::as_str)
    else {
        return;
    };
    let Ok(definition) = serde_json::from_str::<serde_json::Value>(definition_json) else {
        lines.push("Graph: invalid stored definition JSON".to_string());
        return;
    };
    let nodes = definition
        .get("nodes")
        .and_then(serde_json::Value::as_object);
    let edges = definition
        .get("edges")
        .and_then(serde_json::Value::as_array);
    lines.push(format!(
        "Graph ({} nodes, {} edges)",
        nodes.map_or(0, serde_json::Map::len),
        edges.map_or(0, Vec::len)
    ));
    if let Some(nodes) = nodes {
        for (id, node) in nodes {
            let mut line = format!("  [{id}] {}", text(node, "kind"));
            if text(node, "kind") == "agent"
                && let Some(configuration) = node.get("configuration")
            {
                write!(
                    line,
                    " · target={} · profile={} · provider={} · model={}",
                    text(configuration, "execution_target"),
                    text(configuration, "profile"),
                    text(configuration, "provider"),
                    text(configuration, "model")
                )
                .expect("writing to String cannot fail");
            }
            lines.push(line);
        }
    }
    if let Some(edges) = edges {
        for edge in edges {
            let kind = edge
                .get("kind")
                .map_or_else(|| "direct".to_string(), edge_kind_label);
            lines.push(format!(
                "    {} -> {} · {kind}",
                text(edge, "from"),
                text(edge, "to")
            ));
        }
    }
}

fn edge_kind_label(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), ToString::to_string)
}

fn append_runs(lines: &mut Vec<String>, runs: &[serde_json::Value]) {
    lines.push(format!("Runs ({})", runs.len()));
    for run in runs {
        lines.push(format!(
            "  {} · {} · {} v{}",
            text(run, "run_id"),
            text(run, "status"),
            text(run, "definition_id"),
            number(run, "definition_version")
        ));
    }
}

fn append_git_head_diagnostics(lines: &mut Vec<String>, options: &serde_json::Value) {
    let mut seen = BTreeSet::new();
    let mut diagnostics = Vec::new();
    let mut collect = |value: &serde_json::Value| {
        let Some(expected_head) = value
            .get("expected_head")
            .and_then(serde_json::Value::as_str)
        else {
            return;
        };
        let Some(actual_head) = value.get("actual_head").and_then(serde_json::Value::as_str) else {
            return;
        };
        let outcome = value
            .get("outcome")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let guidance = value
            .get("guidance")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("-");
        let diagnostic = format!(
            "  expected {} · actual {} · {} · {}",
            short_checksum(expected_head),
            short_checksum(actual_head),
            outcome,
            guidance
        );
        if seen.insert(diagnostic.clone()) {
            diagnostics.push(diagnostic);
        }
    };

    if let Some(values) = options
        .get("git_head_diagnostics")
        .and_then(serde_json::Value::as_array)
    {
        for value in values {
            collect(value);
        }
    }
    if let Some(values) = options
        .get("mutation_approvals")
        .and_then(serde_json::Value::as_array)
    {
        for value in values {
            let scope = value.get("scope").unwrap_or(value);
            if let Some(summary) = scope.get("input_summary") {
                collect(summary);
            }
        }
    }
    if let Some(doctor) = options.get("doctor")
        && let Some(issues) = doctor.get("issues").and_then(serde_json::Value::as_array)
    {
        for issue in issues {
            collect(issue);
            if let Some(evidence) = issue.get("evidence") {
                collect(evidence);
            }
        }
    }

    if !diagnostics.is_empty() {
        lines.push(format!("Git HEAD diagnostics ({})", diagnostics.len()));
        lines.extend(diagnostics);
    }
}

fn append_mutation_approvals(
    lines: &mut Vec<String>,
    options: &serde_json::Value,
    selected_approval: usize,
) {
    match mutation_approvals(options) {
        Some(approvals) => {
            lines.push(format!("Mutation approvals ({})", approvals.len()));
            for (index, value) in approvals.iter().enumerate() {
                let scope = value.get("scope").unwrap_or(&serde_json::Value::Null);
                let marker = if index == selected_approval { ">" } else { " " };
                lines.push(format!(
                    "{marker} {} · {} / {} v{} · {} · workspace {} · mutating",
                    text(value, "approval_id"),
                    text(scope, "plugin_id"),
                    text(scope, "block_id"),
                    number(scope, "block_version"),
                    text(scope, "operation"),
                    text(scope, "workspace_snapshot")
                ));
                lines.push(format!(
                    "    immutable input {} · checksum {}",
                    compact_json(scope.get("input_summary")),
                    short_checksum(text(scope, "input_checksum_sha256"))
                ));
                lines.push(format!(
                    "    resources {} · reconciliation {}{}",
                    compact_json(scope.get("resource_claims")),
                    text(scope, "reconciliation"),
                    if text(scope, "reconciliation") == "repair_required" {
                        " (ambiguous accepted execution requires explicit repair)"
                    } else {
                        ""
                    }
                ));
            }
            if !approvals.is_empty() {
                lines.push(
                    "  ↑/↓ select · a approve exact request · d deny exact request".to_string(),
                );
            }
        }
        None => lines.push("Mutation approvals unavailable".to_string()),
    }
}

fn compact_json(value: Option<&serde_json::Value>) -> String {
    value.map_or_else(|| "-".to_string(), serde_json::Value::to_string)
}

fn append_named_rows(
    lines: &mut Vec<String>,
    options: &serde_json::Value,
    key: &str,
    title: &str,
    row: impl Fn(&serde_json::Value) -> String,
) {
    if let Some(values) = options.get(key).and_then(serde_json::Value::as_array) {
        lines.push(format!("{title} ({})", values.len()));
        lines.extend(values.iter().map(row));
    }
}

fn text<'a>(value: &'a serde_json::Value, key: &str) -> &'a str {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("-")
}

fn number(value: &serde_json::Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default()
}

fn short_checksum(checksum: &str) -> &str {
    checksum.get(..12).unwrap_or(checksum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::buffer::Buffer;

    fn render_workspace_text(surface: &WorkflowStatusSurface, width: u16, height: u16) -> String {
        render_workspace_buffer(surface, width, height)
            .cells()
            .chunks(usize::from(width))
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    fn render_workspace_buffer_with_theme(
        surface: &WorkflowStatusSurface,
        width: u16,
        height: u16,
        theme: Option<bcode_plugin_sdk::tui::PluginTuiTheme>,
    ) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        surface.render_themed(area, &mut Frame::new(&mut buffer), theme);
        buffer
    }

    fn render_workspace_buffer(surface: &WorkflowStatusSurface, width: u16, height: u16) -> Buffer {
        render_workspace_buffer_with_theme(surface, width, height, None)
    }

    fn test_plugin_theme(
        foreground: bmux_tui::style::Color,
        accent: bmux_tui::style::Color,
    ) -> bcode_plugin_sdk::tui::PluginTuiTheme {
        use bcode_plugin_sdk::tui::{
            PLUGIN_TUI_COMPONENT_THEME_VERSION, PluginTuiDiffTheme, PluginTuiSourceTheme,
            PluginTuiSyntaxColor, PluginTuiSyntaxTheme,
        };
        let canvas = Style::new();
        let text = Style::new().fg(foreground);
        let muted = Style::new().fg(bmux_tui::style::Color::BrightBlack);
        let focused = Style::new().fg(accent);
        let syntax_color = PluginTuiSyntaxColor::from_tui(foreground);
        bcode_plugin_sdk::tui::PluginTuiTheme {
            component_theme_version: PLUGIN_TUI_COMPONENT_THEME_VERSION,
            canvas,
            text,
            muted,
            border: muted,
            focused,
            selection: focused.add_modifier(Modifier::REVERSED),
            source: PluginTuiSourceTheme {
                source: text,
                border: muted,
                gutter: muted,
                truncated: muted,
            },
            diff: PluginTuiDiffTheme {
                text,
                muted,
                title: focused,
                label: focused,
                added: focused,
                removed: focused,
                hunk: focused,
                added_row: canvas,
                removed_row: canvas,
                added_emphasis: focused,
                removed_emphasis: focused,
            },
            syntax: PluginTuiSyntaxTheme {
                text: syntax_color,
                comment: syntax_color,
                keyword: syntax_color,
                function: syntax_color,
                variable: syntax_color,
                string: syntax_color,
                number: syntax_color,
                type_name: syntax_color,
                operator: syntax_color,
                punctuation: syntax_color,
            },
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn status_lines_render_daemon_backed_sections() {
        let definition = serde_json::json!({
            "nodes": {
                "review": {
                    "kind": "agent",
                    "configuration": {
                        "execution_target": "fresh_isolated",
                        "profile": "review",
                        "provider": "bcode.fake-provider",
                        "model": "fake-review",
                    }
                },
                "approve": {"kind": "approval"}
            },
            "edges": [{"from": "review", "to": "approve", "kind": "direct"}],
        });
        let lines = surface_lines(
            &serde_json::json!({
                "command_id": "workflow.inspect",
                "definition": {"definition_json": definition.to_string()},
                "blocks": [{"block_id": "code_review.bundle", "block_version": 1, "plugin_id": "bcode.code_review"}],
                "run": {"run_id": "run-1", "status": "running", "definition_id": "review", "definition_version": 2},
                "waits": [{"node_id": "approve", "kind": "approval", "activation_id": "a1"}],
                "mutation_approvals": [{
                    "approval_id": "mutation-1",
                    "scope": {
                        "plugin_id": "bcode.git",
                        "block_id": "git.commit",
                        "block_version": 1,
                        "operation": "git.commit",
                        "workspace_snapshot": "/repo",
                        "input_checksum_sha256": "0123456789abcdef",
                        "input_summary": {"expected_head": "abc", "paths": ["src/lib.rs"]},
                        "resource_claims": [{"resource": "repository", "access": "write"}],
                        "reconciliation": "repair_required"
                    }
                }],
                "git_head_diagnostics": [{
                    "expected_head": "aaaaaaaaaaaaaaaa",
                    "actual_head": "bbbbbbbbbbbbbbbb",
                    "outcome": "diverged",
                    "guidance": "explicit repair is required"
                }],
                "attempts": [{"node_id": "review", "attempt": 1, "status": "succeeded"}],
                "grants": [{"grant_id": "grant-1", "node_id": "review", "scope": {"capability": "read_only"}}],
                "resource_leases": [{"node_id": "review", "mode": "read", "resource_key": "repo"}],
                "outputs": [{"node_id": "review", "schema_id": "review/v1", "artifact_reference": "bcode-artifact://result"}],
                "descendant_runs": [{
                    "link": {"depth": 2, "parent_run_id": "run-1"},
                    "run": {"run_id": "tranche-1", "status": "running", "definition_id": "delivery-tranche", "definition_version": 1}
                }],
                "repeat_outcomes": [{
                    "node_id": "batch_repeat", "iterations_completed": 3,
                    "max_iterations": 5, "effective_iteration_bound": 5,
                    "outcome": "condition_cleared"
                }],
                "child_sessions": [{"id": "session-1", "name": "reviewer"}],
                "events": [{"event_seq": 3, "event_type": "attempt_succeeded"}],
            }),
            0,
        );
        let rendered = lines.join("\n");
        assert!(rendered.contains("Available plugin blocks (1)"));
        assert!(rendered.contains("code_review.bundle v1 · bcode.code_review"));
        assert!(rendered.contains("Run run-1 · running · review v2"));
        assert!(rendered.contains("Graph (2 nodes, 1 edges)"));
        assert!(rendered.contains("target=fresh_isolated · profile=review"));
        assert!(rendered.contains("provider=bcode.fake-provider · model=fake-review"));
        assert!(rendered.contains("review -> approve · direct"));
        assert!(rendered.contains("Waits (1)"));
        assert!(rendered.contains("Mutation approvals (1)"));
        assert!(rendered.contains("bcode.git / git.commit v1 · git.commit · workspace /repo"));
        assert!(
            rendered
                .contains("immutable input {\"expected_head\":\"abc\",\"paths\":[\"src/lib.rs\"]}")
        );
        assert!(rendered.contains("checksum 0123456789ab"));
        assert!(
            rendered.contains("resources [{\"access\":\"write\",\"resource\":\"repository\"}]")
        );
        assert!(rendered.contains("reconciliation repair_required"));
        assert!(rendered.contains("ambiguous accepted execution requires explicit repair"));
        assert!(rendered.contains("Git HEAD diagnostics (1)"));
        assert!(rendered.contains("expected aaaaaaaaaaaa · actual bbbbbbbbbbbb · diverged"));
        assert!(rendered.contains("Composed workflow levels (1)"));
        assert!(rendered.contains("depth 2 · tranche-1 · running · parent=run-1"));
        assert!(rendered.contains("Runtime-owned counters (1)"));
        assert!(rendered.contains("batch_repeat · 3/5 · effective=5 · condition_cleared"));
        assert!(rendered.contains("explicit repair is required"));
        assert!(rendered.contains("Attempts (1)"));
        assert!(rendered.contains("Grants (1)"));
        assert!(rendered.contains("Resource leases (1)"));
        assert!(rendered.contains("Outputs/artifacts (1)"));
        assert!(rendered.contains("Child sessions (1)"));
        assert!(rendered.contains("Events (1)"));
        let options = serde_json::json!({
            "mutation_approvals": [{"approval_id": "mutation-1"}]
        });
        assert_eq!(
            mutation_approval_command(&options, 0, true).as_deref(),
            Some("/workflow approve-mutation approval_id=mutation-1")
        );
        assert_eq!(
            mutation_approval_command(&options, 0, false).as_deref(),
            Some("/workflow deny-mutation approval_id=mutation-1")
        );
    }

    #[test]
    fn status_lines_distinguish_empty_approvals_from_unavailable_data() {
        let loaded = surface_lines(
            &serde_json::json!({"command_id": "workflow.status", "mutation_approvals": []}),
            0,
        )
        .join("\n");
        assert!(loaded.contains("Mutation approvals (0)"));
        assert!(!loaded.contains("Mutation approvals unavailable"));

        let unavailable =
            surface_lines(&serde_json::json!({"command_id": "workflow.status"}), 0).join("\n");
        assert!(unavailable.contains("Mutation approvals unavailable"));
    }

    #[test]
    fn author_surface_previews_requirements_effects_and_exact_start() {
        let options = serde_json::json!({
            "session_id": "00000000-0000-0000-0000-000000000001",
            "configuration": {"prompt": "implement safely", "max_iterations": 3},
            "template": {
                "owner_plugin_id": "bcode.workflow",
                "diagnostics": [],
                "template": {
                    "template_id": "implementation-verification-commit",
                    "template_version": 1,
                    "title": "Implementation verification commit",
                    "description": "Implement, verify, and optionally commit.",
                    "required_plugins": ["bcode.shell", "bcode.git"],
                    "required_capabilities": ["workflow-production/v1"],
                    "definition": {"nodes": {"commit": {"configuration": {
                        "plugin_id": "bcode.git", "block_id": "git.commit",
                        "effect": "mutating", "reconciliation": "repair_required"
                    }}}}
                }
            }
        });
        let rendered = author_lines(&options).join("\n");
        assert!(rendered.contains("Implementation verification commit"));
        assert!(rendered.contains("Required plugins (2)"));
        assert!(rendered.contains("bcode.git / git.commit · effect=mutating"));
        assert!(rendered.contains("reconciliation=repair_required"));
        assert!(rendered.contains("Validated configuration preview"));
        let command = template_start_command(&options).expect("start command");
        assert!(command.contains("template_id=implementation-verification-commit"));
        assert!(command.contains("session_id=00000000-0000-0000-0000-000000000001"));
        assert!(command.contains("max_iterations"));
    }

    #[test]
    fn author_surface_blocks_start_when_requirements_are_unavailable() {
        let options = serde_json::json!({
            "template": {
                "owner_plugin_id": "bcode.workflow",
                "diagnostics": [{
                    "code": "missing_plugin", "requirement": "bcode.git",
                    "message": "required plugin is not loaded"
                }],
                "template": {
                    "template_id": "implementation-verification-commit",
                    "template_version": 1,
                    "title": "Implementation verification commit",
                    "description": "Implement and verify."
                }
            }
        });
        assert!(template_start_command(&options).is_none());
        assert!(
            author_lines(&options)
                .join("\n")
                .contains("Resolve diagnostics and provide valid configuration before start")
        );
    }
    #[allow(clippy::too_many_lines)]
    fn projected_surface() -> WorkflowStatusSurface {
        let run = bcode_workflow_view_models::WorkflowRunView {
            version: bcode_workflow_view_models::WORKFLOW_VIEW_VERSION,
            run: bcode_workflow_view_models::WorkflowRunListItem {
                run_id: "run-1".to_string(),
                display_title: "Review".to_string(),
                binding_label: None,
                definition_id: "review".to_string(),
                definition_version: 1,
                authored_source: None,
                definition_disposition:
                    bcode_workflow_view_models::WorkflowDefinitionDisposition::CompiledOnly,
                parent_run_id: None,
                descendant_count: 0,
                progress: bcode_workflow_view_models::WorkflowRunProgress {
                    total_nodes: 1,
                    failed: 1,
                    ..bcode_workflow_view_models::WorkflowRunProgress::default()
                },
                attention: bcode_workflow_view_models::WorkflowAttentionSummary {
                    pending_approvals: 1,
                    retryable_failures: 1,
                    ..bcode_workflow_view_models::WorkflowAttentionSummary::default()
                },
                status: bcode_workflow_view_models::WorkflowRunStatus::Running,
                created_at_ms: 1,
                updated_at_ms: 2,
            },
            nodes: vec![bcode_workflow_view_models::WorkflowNodeView {
                node_id: "reviewer".to_string(),
                name: "Reviewer".to_string(),
                kind: bcode_workflow_view_models::WorkflowNodeKind::Agent,
                activation_id: Some("activation-1".to_string()),
                status: bcode_workflow_view_models::WorkflowNodeStatus::Failed,
            }],
            edges: Vec::new(),
            waits: vec![bcode_workflow_view_models::WorkflowWaitView {
                node_id: "approval".to_string(),
                activation_id: "approval-activation".to_string(),
                kind: bcode_workflow_view_models::WorkflowWaitKind::Approval,
                prompt: "Approve review".to_string(),
                expected_schema: None,
                input: None,
                requested_at_ms: 2,
            }],
            mutation_approvals: Vec::new(),
            attempts: vec![bcode_workflow_view_models::WorkflowAttemptView {
                node_id: "reviewer".to_string(),
                activation_id: "activation-1".to_string(),
                attempt: 2,
                dispatch_identity: "dispatch-2".to_string(),
                status: "failed".to_string(),
                has_receipt: true,
                prepared_at_ms: 2,
                terminal_at_ms: Some(3),
            }],
            outputs: vec![bcode_workflow_view_models::WorkflowOutputView {
                output_id: "output-1".to_string(),
                node_id: "reviewer".to_string(),
                activation_id: "activation-1".to_string(),
                schema_id: "bcode.review.result/v1".to_string(),
                schema_version: 1,
                checksum_sha256: "checksum".to_string(),
                value: bcode_workflow_view_models::WorkflowOutputValue::Resolved {
                    value: serde_json::json!({"verdict":"fail","findings":["issue"]}),
                },
                artifact_reference: None,
                created_at_ms: 3,
            }],
            descendant_runs: Vec::new(),
            child_sessions: vec![bcode_workflow_view_models::WorkflowChildSessionView {
                node_id: "reviewer".to_string(),
                activation_id: "activation-1".to_string(),
                attempt: 2,
                session_id: "00000000-0000-0000-0000-000000000001".to_string(),
            }],
            actions: vec![
                bcode_workflow_view_models::WorkflowActionAffordance {
                    kind: bcode_workflow_view_models::WorkflowActionKind::Pause,
                    target: bcode_workflow_view_models::WorkflowActionTarget::Run {
                        run_id: "run-1".to_string(),
                    },
                    enabled: true,
                    unavailable_reason: None,
                },
                bcode_workflow_view_models::WorkflowActionAffordance {
                    kind: bcode_workflow_view_models::WorkflowActionKind::Cancel,
                    target: bcode_workflow_view_models::WorkflowActionTarget::Run {
                        run_id: "run-1".to_string(),
                    },
                    enabled: true,
                    unavailable_reason: None,
                },
                bcode_workflow_view_models::WorkflowActionAffordance {
                    kind: bcode_workflow_view_models::WorkflowActionKind::Approve,
                    target: bcode_workflow_view_models::WorkflowActionTarget::Activation {
                        run_id: "run-1".to_string(),
                        node_id: "approval".to_string(),
                        activation_id: "approval-activation".to_string(),
                    },
                    enabled: true,
                    unavailable_reason: None,
                },
                bcode_workflow_view_models::WorkflowActionAffordance {
                    kind: bcode_workflow_view_models::WorkflowActionKind::RetryNode,
                    target: bcode_workflow_view_models::WorkflowActionTarget::Attempt {
                        run_id: "run-1".to_string(),
                        node_id: "reviewer".to_string(),
                        activation_id: "activation-1".to_string(),
                        attempt: 2,
                    },
                    enabled: true,
                    unavailable_reason: None,
                },
            ],
            terminal: None,
            health: bcode_workflow_view_models::WorkflowProjectionHealth::Current,
        };
        WorkflowStatusSurface {
            options: serde_json::Value::Null,
            selected_approval: 0,
            text_view: TextViewState::new(),
            selected_run_id: Some("run-1".to_string()),
            selected_node_id: Some("reviewer".to_string()),
            selected_wait_id: Some(("approval".to_string(), "approval-activation".to_string())),
            selected_approval_id: None,
            selected_attempt_id: Some(("reviewer".to_string(), "activation-1".to_string(), 2)),
            selected_output_id: Some("output-1".to_string()),
            selected_child_session_id: Some("00000000-0000-0000-0000-000000000001".to_string()),
            detail_loading_run_id: None,
            catalog_loading: false,
            catalog_stale: false,
            catalog_error: None,
            detail_errors: std::collections::BTreeMap::new(),
            workspace_focus: WorkflowWorkspaceFocus::Catalog,
            narrow_page: WorkflowNarrowPage::Runs,
            descendants_expanded: false,
            active_detail_tab: 0,
            input_form: None,
            pending_confirmation: None,
            pending_action_target: None,
            inline_error: None,
            catalog_search_buffer: None,
            updates: None,
            catalog: Some(bcode_workflow_view_models::WorkflowCatalogView {
                version: bcode_workflow_view_models::WORKFLOW_VIEW_VERSION,
                runs: vec![run.run.clone()],
                next_cursor: None,
                has_more: false,
                filter: bcode_workflow_view_models::WorkflowCatalogFilter::All,
                sort: bcode_workflow_view_models::WorkflowCatalogSort::UpdatedAt,
                group: bcode_workflow_view_models::WorkflowCatalogGroup::None,
                search: None,
            }),
            runs: std::collections::BTreeMap::from([("run-1".to_string(), run)]),
            live_status: "live".to_string(),
            subscription_requested: true,
        }
    }

    #[test]
    fn stale_visible_action_fails_closed_before_dispatch() {
        let mut surface = projected_surface();
        surface
            .runs
            .get_mut("run-1")
            .expect("run")
            .actions
            .retain(|action| action.kind != bcode_workflow_view_models::WorkflowActionKind::Pause);

        assert_eq!(
            surface.handle_control_center_event(&Event::Key(bmux_keyboard::KeyStroke::simple(
                KeyCode::Char('p')
            ))),
            PluginTuiAction::Redraw
        );
        assert!(
            surface
                .inline_error
                .as_deref()
                .is_some_and(|error| error.contains("stale or unavailable"))
        );
        assert!(surface.pending_action_target.is_none());
    }

    #[test]
    fn projected_control_center_renders_typed_results_and_routes_actions() {
        let mut surface = projected_surface();
        let rendered = workflow_view_lines(
            surface.catalog.as_ref(),
            &surface.runs,
            &surface.live_status,
            Some("run-1"),
            Some("reviewer"),
            None,
        )
        .join("\n");
        assert!(rendered.contains("bcode.review.result/v1"));
        assert!(rendered.contains("findings"));
        assert!(rendered.contains("issue"));
        assert_eq!(
            surface.handle_control_center_event(&Event::Key(bmux_keyboard::KeyStroke::simple(
                KeyCode::Char('c')
            ))),
            PluginTuiAction::Redraw
        );
        assert!(surface.pending_confirmation.is_some());
        assert!(matches!(
            surface.handle_control_center_event(&Event::Key(
                bmux_keyboard::KeyStroke::simple(KeyCode::Enter)
            )),
            PluginTuiAction::InvokePluginCommand { ref command_id, .. }
                if command_id == "workflow.cancel"
        ));
        assert!(matches!(
            surface.handle_control_center_event(&Event::Key(bmux_keyboard::KeyStroke::simple(KeyCode::Char('r')))),
            PluginTuiAction::InvokePluginCommand { ref command_id, .. }
                if command_id == "workflow.retry-node"
        ));
        assert!(matches!(
            surface.handle_control_center_event(&Event::Key(bmux_keyboard::KeyStroke::simple(KeyCode::Char('a')))),
            PluginTuiAction::InvokePluginCommand { ref command_id, .. }
                if command_id == "workflow.approve"
        ));
        assert!(matches!(
            surface.handle_control_center_event(&Event::Key(bmux_keyboard::KeyStroke::simple(
                KeyCode::Char('o')
            ))),
            PluginTuiAction::OpenSession { .. }
        ));
    }
    #[test]
    fn input_form_validates_schema_and_retains_text_after_rejection() {
        let mut surface = projected_surface();
        let mut run = surface.runs.get("run-1").expect("run").clone();
        run.waits = vec![bcode_workflow_view_models::WorkflowWaitView {
            node_id: "input".to_string(),
            activation_id: "input-1".to_string(),
            kind: bcode_workflow_view_models::WorkflowWaitKind::Input,
            prompt: "Provide approved flag".to_string(),
            expected_schema: Some(serde_json::json!({
                "type": "object",
                "required": ["approved"],
                "properties": {"approved": {"type": "boolean"}}
            })),
            input: None,
            requested_at_ms: 1,
        }];
        surface.runs.insert("run-1".to_string(), run);
        surface.selected_wait_id = Some(("input".to_string(), "input-1".to_string()));
        assert_eq!(
            surface.handle_control_center_event(&Event::Key(bmux_keyboard::KeyStroke::simple(
                KeyCode::Char('i')
            ))),
            PluginTuiAction::Redraw
        );
        let form = surface.input_form.as_mut().expect("input form");
        assert_eq!(form.fields.len(), 1);
        assert_eq!(form.fields[0].name, "approved");
        assert_eq!(form.fields[0].kind, WorkflowInputFieldKind::Boolean);
        form.fields[0].editor = TextInputState::new(TextEditBuffer::from_text("yes"));
        let ctrl_enter = Event::Key(bmux_keyboard::KeyStroke {
            key: KeyCode::Enter,
            modifiers: bmux_keyboard::Modifiers {
                ctrl: true,
                ..bmux_keyboard::Modifiers::NONE
            },
        });
        assert_eq!(
            surface.handle_control_center_event(&ctrl_enter),
            PluginTuiAction::Redraw
        );
        let form = surface.input_form.as_ref().expect("retained form");
        assert!(
            form.error
                .as_deref()
                .is_some_and(|error| error.contains("true or false"))
        );
        assert_eq!(form.fields[0].editor.buffer().text(), "yes");

        surface.input_form.as_mut().expect("input form").fields[0].editor =
            TextInputState::new(TextEditBuffer::from_text("true"));
        assert!(matches!(
            surface.handle_control_center_event(&ctrl_enter),
            PluginTuiAction::InvokePluginCommand {
                ref command_id,
                arguments: Some(ref arguments),
                ..
            } if command_id == "workflow.provide-input"
                && arguments.contains("run_id=run-1")
                && arguments.contains("activation_id=input-1")
        ));
        assert!(surface.input_form.is_some());
    }

    #[test]
    fn mutation_approval_confirmation_renders_exact_operation_facts() {
        let mut surface = projected_surface();
        let run = surface.runs.get_mut("run-1").expect("run");
        run.mutation_approvals = vec![bcode_workflow_view_models::WorkflowMutationApprovalView {
            approval_id: "approval-exact".to_string(),
            node_id: "mutate".to_string(),
            activation_id: "activation-exact".to_string(),
            plugin_id: "bcode.git".to_string(),
            block_id: "git.commit".to_string(),
            block_version: 3,
            operation: "commit".to_string(),
            effect: bcode_workflow_view_models::WorkflowOperationEffect::Mutating,
            input_summary: serde_json::json!({"message":"reviewed"}),
            resource_claims: vec![bcode_workflow_view_models::WorkflowResourceClaimView {
                resource: "repository".to_string(),
                access: "write".to_string(),
            }],
            workspace_snapshot: "snapshot-exact".to_string(),
            reconciliation_warning: Some("verify HEAD".to_string()),
            requested_at_ms: 4,
            expires_at_ms: Some(10),
        }];
        run.actions
            .push(bcode_workflow_view_models::WorkflowActionAffordance {
                kind: bcode_workflow_view_models::WorkflowActionKind::DenyMutation,
                target: bcode_workflow_view_models::WorkflowActionTarget::MutationApproval {
                    approval_id: "approval-exact".to_string(),
                },
                enabled: true,
                unavailable_reason: None,
            });
        surface.selected_approval_id = Some("approval-exact".to_string());

        assert_eq!(
            surface.handle_control_center_event(&Event::Key(bmux_keyboard::KeyStroke::simple(
                KeyCode::Char('d')
            ))),
            PluginTuiAction::Redraw
        );
        let confirmation = surface.pending_confirmation.as_ref().expect("confirmation");
        assert!(confirmation.detail.contains("bcode.git / git.commit"));
        assert!(confirmation.detail.contains("operation commit"));
        assert!(confirmation.detail.contains("snapshot snapshot-exact"));
        assert!(matches!(
            confirmation.target,
            bcode_workflow_view_models::WorkflowActionTarget::MutationApproval {
                ref approval_id
            } if approval_id == "approval-exact"
        ));
    }

    #[test]
    fn definition_navigation_targets_exact_existing_draft_or_published_revision() {
        let mut surface = projected_surface();
        surface
            .runs
            .get_mut("run-1")
            .expect("run")
            .run
            .definition_disposition =
            bcode_workflow_view_models::WorkflowDefinitionDisposition::Published {
                workflow_id: "review-workflow".to_string(),
                revision: 7,
                editable_draft_id: Some("draft-current".to_string()),
            };
        assert!(matches!(
            surface.definition_navigation_action(),
            PluginTuiAction::OpenSurface {
                ref plugin_id,
                ref surface_id,
                ref options,
            } if plugin_id == "bcode.workflow"
                && surface_id == WORKFLOW_AUTHOR_SURFACE_KIND
                && options["workflow_id"] == "review-workflow"
                && options["draft_id"] == "draft-current"
                && options.get("fork_revision").is_none()
        ));

        if let bcode_workflow_view_models::WorkflowDefinitionDisposition::Published {
            editable_draft_id,
            ..
        } = &mut surface
            .runs
            .get_mut("run-1")
            .expect("run")
            .run
            .definition_disposition
        {
            *editable_draft_id = None;
        }
        assert!(matches!(
            surface.definition_navigation_action(),
            PluginTuiAction::OpenSurface {
                ref options,
                ..
            } if options["workflow_id"] == "review-workflow"
                && options["draft"]["identity"]["draft_id"] == "revision-7-fork"
                && options["fork_revision"] == 7
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn projected_control_center_handles_navigation_pause_mutation_and_input() {
        let mut surface = projected_surface();
        let second = surface.runs.get("run-1").expect("first run").clone();
        let mut second_item = second.run.clone();
        second_item.run_id = "run-2".to_string();
        second_item.status = bcode_workflow_view_models::WorkflowRunStatus::Paused;
        let mut second = second;
        second.run = second_item.clone();
        second.actions = vec![
            bcode_workflow_view_models::WorkflowActionAffordance {
                kind: bcode_workflow_view_models::WorkflowActionKind::Resume,
                target: bcode_workflow_view_models::WorkflowActionTarget::Run {
                    run_id: "run-2".to_string(),
                },
                enabled: true,
                unavailable_reason: None,
            },
            bcode_workflow_view_models::WorkflowActionAffordance {
                kind: bcode_workflow_view_models::WorkflowActionKind::DenyMutation,
                target: bcode_workflow_view_models::WorkflowActionTarget::MutationApproval {
                    approval_id: "mutation-1".to_string(),
                },
                enabled: true,
                unavailable_reason: None,
            },
            bcode_workflow_view_models::WorkflowActionAffordance {
                kind: bcode_workflow_view_models::WorkflowActionKind::ProvideInput,
                target: bcode_workflow_view_models::WorkflowActionTarget::Activation {
                    run_id: "run-2".to_string(),
                    node_id: "input".to_string(),
                    activation_id: "input-activation".to_string(),
                },
                enabled: true,
                unavailable_reason: None,
            },
        ];
        second.waits = vec![bcode_workflow_view_models::WorkflowWaitView {
            node_id: "input".to_string(),
            activation_id: "input-activation".to_string(),
            kind: bcode_workflow_view_models::WorkflowWaitKind::Input,
            prompt: "Provide input".to_string(),
            expected_schema: Some(serde_json::json!({"type": "object"})),
            input: None,
            requested_at_ms: 4,
        }];
        second.mutation_approvals =
            vec![bcode_workflow_view_models::WorkflowMutationApprovalView {
                approval_id: "mutation-1".to_string(),
                node_id: "mutate".to_string(),
                activation_id: "mutation-activation".to_string(),
                plugin_id: "bcode.git".to_string(),
                block_id: "git.commit".to_string(),
                block_version: 1,
                operation: "commit".to_string(),
                effect: bcode_workflow_view_models::WorkflowOperationEffect::Mutating,
                input_summary: serde_json::json!({"message": "reviewed"}),
                resource_claims: Vec::new(),
                workspace_snapshot: "snapshot-1".to_string(),
                reconciliation_warning: None,
                requested_at_ms: 4,
                expires_at_ms: None,
            }];
        surface
            .catalog
            .as_mut()
            .expect("catalog")
            .runs
            .push(second_item);
        surface.runs.insert("run-2".to_string(), second);
        surface.selected_approval_id = Some("mutation-1".to_string());
        surface.selected_wait_id = Some(("input".to_string(), "input-activation".to_string()));

        let key =
            |character| Event::Key(bmux_keyboard::KeyStroke::simple(KeyCode::Char(character)));
        assert_eq!(surface.selected_run_id.as_deref(), Some("run-1"));
        assert!(matches!(
            surface.handle_control_center_event(&key('l')),
            PluginTuiAction::SelectWorkflowRun { ref run_id } if run_id == "run-2"
        ));
        assert_eq!(surface.selected_run_id.as_deref(), Some("run-2"));
        surface.selected_approval_id = Some("mutation-1".to_string());
        surface.selected_wait_id = Some(("input".to_string(), "input-activation".to_string()));
        assert!(matches!(
            surface.handle_control_center_event(&key('p')),
            PluginTuiAction::InvokePluginCommand { ref command_id, .. }
                if command_id == "workflow.resume"
        ));
        assert_eq!(
            surface.handle_control_center_event(&key('d')),
            PluginTuiAction::Redraw
        );
        assert!(surface.pending_confirmation.is_some());
        assert!(matches!(
            surface.handle_control_center_event(&Event::Key(
                bmux_keyboard::KeyStroke::simple(KeyCode::Enter)
            )),
            PluginTuiAction::InvokePluginCommand { ref command_id, .. }
                if command_id == "workflow.deny-mutation"
        ));
        assert_eq!(
            surface.handle_control_center_event(&key('i')),
            PluginTuiAction::Redraw
        );
        assert!(surface.input_form.is_some());
        surface.input_form.as_mut().expect("input form").editor =
            TextInputState::new(TextEditBuffer::from_text("{\"ok\":true}"));
        let submit = surface.handle_control_center_event(&Event::Key(bmux_keyboard::KeyStroke {
            key: KeyCode::Enter,
            modifiers: bmux_keyboard::Modifiers {
                ctrl: true,
                ..bmux_keyboard::Modifiers::NONE
            },
        }));
        assert!(matches!(
            submit,
            PluginTuiAction::InvokePluginCommand {
                ref command_id,
                arguments: Some(ref arguments),
                ..
            } if command_id == "workflow.provide-input" && arguments.contains("value={\"ok\":true}")
        ));
        assert!(
            surface.input_form.is_some(),
            "form remains until authoritative refresh"
        );

        assert_eq!(
            surface.handle_control_center_event(&Event::Key(bmux_keyboard::KeyStroke::simple(
                KeyCode::Escape
            ),)),
            PluginTuiAction::Redraw
        );
        assert!(surface.input_form.is_none());
        assert_eq!(
            surface.handle_control_center_event(&key('l')),
            PluginTuiAction::Redraw
        );
        assert_eq!(
            surface.selected_run_id.as_deref(),
            Some("run-2"),
            "run navigation stays bounded"
        );
    }

    #[test]
    fn catalog_search_preserves_query_controls_and_supports_clearing() {
        let mut surface = projected_surface();
        let key =
            |character| Event::Key(bmux_keyboard::KeyStroke::simple(KeyCode::Char(character)));
        assert_eq!(
            surface.handle_control_center_event(&key('/')),
            PluginTuiAction::Redraw
        );
        for character in ['r', 'e', 'v', 'i', 'e', 'w'] {
            assert_eq!(
                surface.handle_control_center_event(&key(character)),
                PluginTuiAction::Redraw
            );
        }
        assert!(matches!(
            surface.handle_control_center_event(&Event::Key(
                bmux_keyboard::KeyStroke::simple(KeyCode::Enter)
            )),
            PluginTuiAction::UpdateWorkflowCatalogQuery {
                filter: bcode_workflow_view_models::WorkflowCatalogFilter::All,
                sort: bcode_workflow_view_models::WorkflowCatalogSort::UpdatedAt,
                group: bcode_workflow_view_models::WorkflowCatalogGroup::None,
                search: Some(ref search),
            } if search == "review"
        ));

        surface.catalog.as_mut().expect("catalog").search = Some("review".to_string());
        assert_eq!(
            surface.handle_control_center_event(&key('/')),
            PluginTuiAction::Redraw
        );
        for _ in 0..6 {
            assert_eq!(
                surface.handle_control_center_event(&Event::Key(bmux_keyboard::KeyStroke::simple(
                    KeyCode::Backspace
                ))),
                PluginTuiAction::Redraw
            );
        }
        assert!(matches!(
            surface.handle_control_center_event(&Event::Key(bmux_keyboard::KeyStroke::simple(
                KeyCode::Enter
            ))),
            PluginTuiAction::UpdateWorkflowCatalogQuery { search: None, .. }
        ));
    }

    #[test]
    fn catalog_live_insertion_appears_without_loading_new_run_detail() {
        let mut surface = projected_surface();
        let mut inserted = surface.catalog.as_ref().expect("catalog").runs[0].clone();
        inserted.run_id = "run-new".to_string();
        inserted.display_title = "New live run".to_string();
        let mut catalog = surface.catalog.clone().expect("catalog");
        catalog.runs.insert(0, inserted);
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        surface.attach_updates(receiver);
        sender
            .try_send(PluginTuiSurfaceUpdate::WorkflowCatalog(catalog))
            .expect("catalog update");

        assert_eq!(surface.poll(&TestHost), PluginTuiAction::Redraw);
        assert_eq!(surface.selected_run_id.as_deref(), Some("run-1"));
        assert!(
            surface
                .catalog
                .as_ref()
                .expect("catalog")
                .runs
                .iter()
                .any(|run| run.run_id == "run-new")
        );
        assert!(!surface.runs.contains_key("run-new"));
        assert!(surface.detail_loading_run_id.is_none());
    }

    #[test]
    fn catalog_snapshot_reorder_preserves_exact_run_and_node_selection() {
        let mut surface = projected_surface();
        let mut second = surface.catalog.as_ref().expect("catalog").runs[0].clone();
        second.run_id = "run-2".to_string();
        let catalog = surface.catalog.as_mut().expect("catalog");
        catalog.runs.push(second);
        catalog.runs.swap(0, 1);
        let catalog = catalog.clone();
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        surface.attach_updates(receiver);
        sender
            .try_send(PluginTuiSurfaceUpdate::WorkflowCatalog(catalog))
            .expect("catalog update");

        assert_eq!(surface.poll(&TestHost), PluginTuiAction::Redraw);
        assert_eq!(surface.selected_run_id.as_deref(), Some("run-1"));
        assert_eq!(surface.selected_node_id.as_deref(), Some("reviewer"));
        assert!(surface.detail_loading_run_id.is_none());
    }

    #[test]
    fn catalog_snapshot_reconciles_stable_selection_without_retargeting_detail() {
        let mut surface = projected_surface();
        surface.selected_run_id = Some("removed-run".to_string());
        surface.selected_node_id = Some("removed-node".to_string());
        surface.selected_wait_id = Some(("removed-node".to_string(), "activation".to_string()));
        let catalog = surface.catalog.clone().expect("catalog");
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        surface.attach_updates(receiver);
        sender
            .try_send(PluginTuiSurfaceUpdate::WorkflowCatalog(catalog))
            .expect("catalog update");

        assert_eq!(surface.poll(&TestHost), PluginTuiAction::Redraw);
        assert_eq!(surface.selected_run_id.as_deref(), Some("run-1"));
        assert!(surface.selected_node_id.is_none());
        assert!(surface.selected_wait_id.is_none());
        assert_eq!(surface.detail_loading_run_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn selected_run_refresh_preserves_exact_node_identity_after_reorder() {
        let mut surface = projected_surface();
        let mut view = surface.runs.get("run-1").expect("run").clone();
        view.nodes
            .push(bcode_workflow_view_models::WorkflowNodeView {
                node_id: "prepare".to_string(),
                name: "Prepare".to_string(),
                kind: bcode_workflow_view_models::WorkflowNodeKind::Task,
                activation_id: None,
                status: bcode_workflow_view_models::WorkflowNodeStatus::Completed,
            });
        view.nodes.swap(0, 1);
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        surface.attach_updates(receiver);
        sender
            .try_send(PluginTuiSurfaceUpdate::WorkflowRun(Box::new(view)))
            .expect("run update");

        assert_eq!(surface.poll(&TestHost), PluginTuiAction::Redraw);
        assert_eq!(surface.selected_run_id.as_deref(), Some("run-1"));
        assert_eq!(surface.selected_node_id.as_deref(), Some("reviewer"));
    }

    #[test]
    fn stale_live_detail_cannot_regress_authoritative_terminal_state() {
        let mut surface = projected_surface();
        let run = surface.runs.get_mut("run-1").expect("run");
        run.run.status = bcode_workflow_view_models::WorkflowRunStatus::Completed;
        run.terminal = Some(
            bcode_workflow_view_models::WorkflowTerminalView::Completed {
                output_id: "output-1".to_string(),
            },
        );
        let mut stale = run.clone();
        stale.run.status = bcode_workflow_view_models::WorkflowRunStatus::Running;
        stale.terminal = None;
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        surface.attach_updates(receiver);
        sender
            .try_send(PluginTuiSurfaceUpdate::WorkflowRun(Box::new(stale)))
            .expect("stale live update");

        assert_eq!(surface.poll(&TestHost), PluginTuiAction::Redraw);
        let retained = surface.runs.get("run-1").expect("retained run");
        assert_eq!(
            retained.run.status,
            bcode_workflow_view_models::WorkflowRunStatus::Completed
        );
        assert!(matches!(
            retained.terminal,
            Some(bcode_workflow_view_models::WorkflowTerminalView::Completed { .. })
        ));
        assert!(surface.live_status.contains("ignored stale"));
    }

    #[test]
    fn loading_and_request_errors_are_scoped_without_discarding_stale_data() {
        let mut surface = projected_surface();
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        surface.attach_updates(receiver);
        sender
            .try_send(PluginTuiSurfaceUpdate::WorkflowCatalogLoading { stale: true })
            .expect("loading update");
        sender
            .try_send(PluginTuiSurfaceUpdate::WorkflowCatalogError {
                message: "catalog unavailable".to_string(),
            })
            .expect("catalog error");
        sender
            .try_send(PluginTuiSurfaceUpdate::WorkflowRunError {
                run_id: "run-1".to_string(),
                message: "detail unavailable".to_string(),
            })
            .expect("detail error");

        assert_eq!(surface.poll(&TestHost), PluginTuiAction::Redraw);
        assert!(surface.catalog.is_some(), "stale catalog remains visible");
        assert_eq!(
            surface.catalog_error.as_deref(),
            Some("catalog unavailable")
        );
        assert!(surface.catalog_stale);
        assert_eq!(
            surface.detail_errors.get("run-1").map(String::as_str),
            Some("detail unavailable")
        );
        assert_ne!(surface.live_status, "live updates unavailable");
    }

    #[test]
    fn inspector_sections_present_semantic_outputs_approvals_sessions_and_definition() {
        let mut surface = projected_surface();
        let run = surface.runs.get_mut("run-1").expect("run");
        run.mutation_approvals = vec![bcode_workflow_view_models::WorkflowMutationApprovalView {
            approval_id: "approval-1".to_string(),
            node_id: "reviewer".to_string(),
            activation_id: "activation-1".to_string(),
            plugin_id: "bcode.git".to_string(),
            block_id: "git.commit".to_string(),
            block_version: 1,
            operation: "commit".to_string(),
            effect: bcode_workflow_view_models::WorkflowOperationEffect::Mutating,
            input_summary: serde_json::json!({"message":"reviewed"}),
            resource_claims: vec![bcode_workflow_view_models::WorkflowResourceClaimView {
                resource: "repository".to_string(),
                access: "write".to_string(),
            }],
            workspace_snapshot: "snapshot-1".to_string(),
            reconciliation_warning: Some("verify HEAD".to_string()),
            requested_at_ms: 4,
            expires_at_ms: Some(10),
        }];
        run.run.definition_disposition =
            bcode_workflow_view_models::WorkflowDefinitionDisposition::Published {
                workflow_id: "review-workflow".to_string(),
                revision: 3,
                editable_draft_id: None,
            };

        let rendered = |tab| {
            inspector_lines(Some(run), tab, WorkflowSurfaceTheme::resolve(None))
                .into_iter()
                .flat_map(|line| {
                    line.spans
                        .into_iter()
                        .map(|span| span.content)
                        .collect::<Vec<_>>()
                })
                .collect::<String>()
        };
        assert!(rendered(0).contains("descendants"));
        assert!(rendered(1).contains("Expected schema"));
        assert!(rendered(2).contains("Verdict"));
        assert!(rendered(2).contains("Findings (1)"));
        assert!(rendered(3).contains("dispatch-2"));
        assert!(rendered(4).contains("repository:write"));
        assert!(rendered(4).contains("verify HEAD"));
        assert!(rendered(5).contains("attempt 2"));
        assert!(rendered(6).contains("Immutable revision · fork to edit"));
    }

    #[test]
    fn nested_workflows_expand_without_changing_canonical_selection() {
        let mut surface = projected_surface();
        let child = surface.catalog.as_ref().expect("catalog").runs[0].clone();
        let run = surface.runs.get_mut("run-1").expect("run");
        run.descendant_runs = vec![bcode_workflow_view_models::WorkflowDescendantRunView {
            run: child,
            parent_run_id: "run-1".to_string(),
            parent_node_id: "reviewer".to_string(),
            depth: 1,
        }];
        let collapsed = workflow_graph_lines(
            run,
            Some("reviewer"),
            80,
            30,
            false,
            WorkflowSurfaceTheme::resolve(None),
        );
        let expanded = workflow_graph_lines(
            run,
            Some("reviewer"),
            80,
            30,
            true,
            WorkflowSurfaceTheme::resolve(None),
        );
        let text = |lines: Vec<Line>| {
            lines
                .into_iter()
                .flat_map(|line| {
                    line.spans
                        .into_iter()
                        .map(|span| span.content)
                        .collect::<Vec<_>>()
                })
                .collect::<String>()
        };
        assert!(text(collapsed).contains("▸ Nested"));
        assert!(text(expanded).contains("▾ Nested"));
        assert_eq!(
            surface.handle_control_center_event(&Event::Key(bmux_keyboard::KeyStroke::simple(
                KeyCode::Char('n')
            ))),
            PluginTuiAction::Redraw
        );
        assert!(surface.descendants_expanded);
        assert_eq!(surface.selected_run_id.as_deref(), Some("run-1"));
        assert_eq!(surface.selected_node_id.as_deref(), Some("reviewer"));
    }

    #[test]
    fn graph_layout_groups_dependency_levels_and_preserves_control_flow_edges() {
        let mut surface = projected_surface();
        let run = surface.runs.get_mut("run-1").expect("run");
        run.nodes
            .push(bcode_workflow_view_models::WorkflowNodeView {
                node_id: "branch".to_string(),
                name: "Branch".to_string(),
                kind: bcode_workflow_view_models::WorkflowNodeKind::Branch,
                activation_id: Some("activation-2".to_string()),
                status: bcode_workflow_view_models::WorkflowNodeStatus::Running,
            });
        run.nodes
            .push(bcode_workflow_view_models::WorkflowNodeView {
                node_id: "publish".to_string(),
                name: "Publish".to_string(),
                kind: bcode_workflow_view_models::WorkflowNodeKind::WorkflowCall,
                activation_id: None,
                status: bcode_workflow_view_models::WorkflowNodeStatus::NotStarted,
            });
        run.edges = vec![
            bcode_workflow_view_models::WorkflowEdgeView {
                from: "reviewer".to_string(),
                to: "branch".to_string(),
                kind: "direct".to_string(),
            },
            bcode_workflow_view_models::WorkflowEdgeView {
                from: "branch".to_string(),
                to: "publish".to_string(),
                kind: "conditional".to_string(),
            },
            bcode_workflow_view_models::WorkflowEdgeView {
                from: "publish".to_string(),
                to: "branch".to_string(),
                kind: "retry".to_string(),
            },
        ];
        let run = surface.runs.get("run-1").expect("run");
        let generations = workflow_graph_generations(run).expect("acyclic forward graph");
        assert_eq!(generations, vec![vec![0], vec![1], vec![2]]);
        let rendered = workflow_graph_lines(
            run,
            Some("branch"),
            80,
            30,
            false,
            WorkflowSurfaceTheme::resolve(None),
        )
        .into_iter()
        .flat_map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content)
                .collect::<Vec<_>>()
        })
        .collect::<String>();
        assert!(rendered.contains("Stage 1"));
        assert!(rendered.contains("├?→ publish (conditional)"));
        assert!(rendered.contains("↻ branch (retry)"));
        assert!(rendered.contains("▶ ● Branch"));
    }

    #[test]
    fn graph_matrix_covers_branches_joins_repeat_retry_fanout_parallel_and_nested_runs() {
        let mut surface = projected_surface();
        let template = surface.runs.get("run-1").expect("run").nodes[0].clone();
        let run = surface.runs.get_mut("run-1").expect("run");
        for (id, name, kind) in [
            (
                "fanout",
                "Fan out",
                bcode_workflow_view_models::WorkflowNodeKind::FanOut,
            ),
            (
                "parallel",
                "Parallel",
                bcode_workflow_view_models::WorkflowNodeKind::Parallel,
            ),
            (
                "join",
                "Join",
                bcode_workflow_view_models::WorkflowNodeKind::Task,
            ),
            (
                "repeat",
                "Repeat",
                bcode_workflow_view_models::WorkflowNodeKind::Repeat,
            ),
            (
                "retry",
                "Retry",
                bcode_workflow_view_models::WorkflowNodeKind::Retry,
            ),
        ] {
            let mut node = template.clone();
            node.node_id = id.to_string();
            node.name = name.to_string();
            node.kind = kind;
            run.nodes.push(node);
        }
        run.edges = vec![
            ("reviewer", "fanout", "direct"),
            ("fanout", "parallel", "conditional"),
            ("fanout", "join", "direct"),
            ("parallel", "join", "direct"),
            ("join", "repeat", "direct"),
            ("repeat", "retry", "back"),
            ("retry", "repeat", "retry"),
        ]
        .into_iter()
        .map(
            |(from, to, kind)| bcode_workflow_view_models::WorkflowEdgeView {
                from: from.to_string(),
                to: to.to_string(),
                kind: kind.to_string(),
            },
        )
        .collect();
        let child = surface.catalog.as_ref().expect("catalog").runs[0].clone();
        run.descendant_runs = vec![bcode_workflow_view_models::WorkflowDescendantRunView {
            run: child,
            parent_run_id: "run-1".to_string(),
            parent_node_id: "repeat".to_string(),
            depth: 1,
        }];
        let rendered = workflow_graph_lines(
            run,
            Some("join"),
            100,
            40,
            true,
            WorkflowSurfaceTheme::resolve(None),
        )
        .into_iter()
        .flat_map(|line| line.spans.into_iter().map(|span| span.content))
        .collect::<String>();
        for expected in [
            "fan-out",
            "parallel",
            "Join",
            "repeat",
            "retry",
            "conditional",
            "back",
            "Nested",
        ] {
            assert!(
                rendered.contains(expected),
                "missing graph semantic {expected}"
            );
        }
    }

    #[test]
    fn graph_layout_falls_back_for_narrow_or_cyclic_geometry_and_keeps_selection_visible() {
        let mut surface = projected_surface();
        let run = surface.runs.get_mut("run-1").expect("run");
        for index in 0..12 {
            run.nodes
                .push(bcode_workflow_view_models::WorkflowNodeView {
                    node_id: format!("node-{index}"),
                    name: format!("Node {index}"),
                    kind: bcode_workflow_view_models::WorkflowNodeKind::Task,
                    activation_id: None,
                    status: bcode_workflow_view_models::WorkflowNodeStatus::NotStarted,
                });
        }
        run.edges = vec![bcode_workflow_view_models::WorkflowEdgeView {
            from: "node-11".to_string(),
            to: "reviewer".to_string(),
            kind: "direct".to_string(),
        }];
        let lines = workflow_graph_lines(
            run,
            Some("node-11"),
            32,
            5,
            false,
            WorkflowSurfaceTheme::resolve(None),
        );
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_str()))
            .collect::<String>();
        assert!(lines.len() <= 5);
        assert!(rendered.contains("Node 11"));
        assert!(rendered.contains('▶'));
    }

    #[test]
    fn lifecycle_and_failure_states_render_purposeful_workspace_messages() {
        let mut surface = projected_surface();
        let catalog = surface.catalog.as_mut().expect("catalog");
        catalog.runs.clear();
        assert!(render_workspace_text(&surface, 88, 24).contains("No workflow runs yet"));

        let mut surface = projected_surface();
        surface.catalog_loading = true;
        surface.catalog_stale = true;
        assert!(render_workspace_text(&surface, 88, 24).contains("showing stale results"));
        surface.catalog_loading = false;
        surface.catalog_stale = false;
        surface.live_status = "live updates unavailable: offline".to_string();
        assert!(render_workspace_text(&surface, 88, 24).contains("offline"));
        surface.live_status = "resync required; reopen /workflow".to_string();
        assert!(render_workspace_text(&surface, 88, 24).contains("resync required"));
        surface.live_status = "live".to_string();
        surface.runs.get_mut("run-1").expect("run").health =
            bcode_workflow_view_models::WorkflowProjectionHealth::Degraded {
                reason: "bounded detail unavailable".to_string(),
            };
        assert!(render_workspace_text(&surface, 88, 24).contains("projection is degraded"));
        surface.runs.get_mut("run-1").expect("run").health =
            bcode_workflow_view_models::WorkflowProjectionHealth::RepairRequired {
                reason: "explicit repair required".to_string(),
            };
        assert!(render_workspace_text(&surface, 88, 24).contains("requires repair"));
    }

    #[test]
    fn projection_health_and_repair_states_produce_visible_workspace_notices() {
        let mut surface = projected_surface();
        surface.runs.get_mut("run-1").expect("run").health =
            bcode_workflow_view_models::WorkflowProjectionHealth::Degraded {
                reason: "bounded projection incomplete".to_string(),
            };
        assert!(surface.has_workspace_notice());
        assert!(render_workspace_text(&surface, 88, 24).contains("projection is degraded"));

        surface.runs.get_mut("run-1").expect("run").health =
            bcode_workflow_view_models::WorkflowProjectionHealth::UnsupportedVersion {
                version: 99,
            };
        assert!(render_workspace_text(&surface, 88, 24).contains("unsupported projection"));

        let run = surface.runs.get_mut("run-1").expect("run");
        run.health = bcode_workflow_view_models::WorkflowProjectionHealth::Current;
        run.run.status = bcode_workflow_view_models::WorkflowRunStatus::RepairRequired;
        assert!(render_workspace_text(&surface, 88, 24).contains("requires explicit repair"));
    }

    #[test]
    fn long_identity_and_reviewer_findings_render_within_bounded_terminal_area() {
        let mut surface = projected_surface();
        let long_name = "workflow-".repeat(40);
        surface.catalog.as_mut().expect("catalog").runs[0].display_title = long_name.clone();
        let run = surface.runs.get_mut("run-1").expect("run");
        run.run.display_title = long_name;
        run.outputs[0].value = bcode_workflow_view_models::WorkflowOutputValue::Resolved {
            value: serde_json::json!({
                "verdict": "fail",
                "findings": ["bounded reviewer finding ".repeat(80)]
            }),
        };
        surface.active_detail_tab = 2;

        for (width, height) in [(132, 28), (88, 24)] {
            let buffer = render_workspace_buffer(&surface, width, height);
            assert_eq!(
                buffer.cells().len(),
                usize::from(width) * usize::from(height)
            );
            let rendered = buffer
                .cells()
                .iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>();
            assert!(rendered.contains("Verdict"));
            assert!(rendered.contains("Findings"));
        }
        surface.narrow_page = WorkflowNarrowPage::Inspector;
        let buffer = render_workspace_buffer(&surface, 62, 20);
        assert_eq!(buffer.cells().len(), 62 * 20);
        let rendered = buffer
            .cells()
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<String>();
        assert!(rendered.contains("Verdict"));
        assert!(rendered.contains("Findings"));
    }

    #[test]
    fn every_run_and_node_status_has_a_semantic_style_and_glyph() {
        let theme = WorkflowSurfaceTheme::resolve(None);
        for (status, expected) in [
            (
                bcode_workflow_view_models::WorkflowRunStatus::Running,
                theme.info,
            ),
            (
                bcode_workflow_view_models::WorkflowRunStatus::Paused,
                theme.muted,
            ),
            (
                bcode_workflow_view_models::WorkflowRunStatus::Completed,
                theme.success,
            ),
            (
                bcode_workflow_view_models::WorkflowRunStatus::Failed,
                theme.error,
            ),
            (
                bcode_workflow_view_models::WorkflowRunStatus::Cancelled,
                theme.muted,
            ),
            (
                bcode_workflow_view_models::WorkflowRunStatus::RepairRequired,
                theme.error,
            ),
        ] {
            assert_eq!(workflow_run_style(status, theme), expected);
        }

        for (status, expected_style, expected_glyph) in [
            (
                bcode_workflow_view_models::WorkflowNodeStatus::NotStarted,
                theme.muted,
                "○",
            ),
            (
                bcode_workflow_view_models::WorkflowNodeStatus::Pending,
                theme.info,
                "◌",
            ),
            (
                bcode_workflow_view_models::WorkflowNodeStatus::Running,
                theme.info,
                "●",
            ),
            (
                bcode_workflow_view_models::WorkflowNodeStatus::WaitingInput,
                theme.warning,
                "◆",
            ),
            (
                bcode_workflow_view_models::WorkflowNodeStatus::WaitingApproval,
                theme.warning,
                "◆",
            ),
            (
                bcode_workflow_view_models::WorkflowNodeStatus::WaitingMutationApproval,
                theme.warning,
                "◆",
            ),
            (
                bcode_workflow_view_models::WorkflowNodeStatus::Completed,
                theme.success,
                "✓",
            ),
            (
                bcode_workflow_view_models::WorkflowNodeStatus::Failed,
                theme.error,
                "✕",
            ),
            (
                bcode_workflow_view_models::WorkflowNodeStatus::Cancelled,
                theme.muted,
                "⊘",
            ),
            (
                bcode_workflow_view_models::WorkflowNodeStatus::Skipped,
                theme.muted,
                "–",
            ),
            (
                bcode_workflow_view_models::WorkflowNodeStatus::RepairRequired,
                theme.error,
                "!",
            ),
            (
                bcode_workflow_view_models::WorkflowNodeStatus::Unknown("future".to_string()),
                theme.muted,
                "?",
            ),
        ] {
            assert_eq!(workflow_node_style(&status, theme), expected_style);
            assert_eq!(workflow_node_glyph(&status), expected_glyph);
        }
    }

    #[test]
    fn semantic_status_styles_and_focus_are_applied_to_rendered_workspace() {
        let surface = projected_surface();
        let theme = WorkflowSurfaceTheme::resolve(None);
        assert_eq!(
            workflow_run_style(
                bcode_workflow_view_models::WorkflowRunStatus::Running,
                theme
            ),
            theme.info
        );
        assert_eq!(
            workflow_run_style(
                bcode_workflow_view_models::WorkflowRunStatus::Completed,
                theme
            ),
            theme.success
        );
        assert_eq!(
            workflow_run_style(bcode_workflow_view_models::WorkflowRunStatus::Failed, theme),
            theme.error
        );
        assert_eq!(
            workflow_node_style(
                &bcode_workflow_view_models::WorkflowNodeStatus::WaitingApproval,
                theme
            ),
            theme.warning
        );
        assert_eq!(
            workflow_node_style(
                &bcode_workflow_view_models::WorkflowNodeStatus::Cancelled,
                theme
            ),
            theme.muted
        );

        let buffer = render_workspace_buffer(&surface, 132, 28);
        assert!(
            buffer
                .cells()
                .iter()
                .any(|cell| cell.style == theme.focused)
        );
        assert!(
            buffer
                .cells()
                .iter()
                .any(|cell| cell.style == theme.selected)
        );
        assert!(buffer.cells().iter().any(|cell| cell.style == theme.info));
        assert!(buffer.cells().iter().any(|cell| cell.style == theme.error));
    }

    #[test]
    fn multiple_host_themes_drive_workspace_styles_without_changing_semantics() {
        let surface = projected_surface();
        for (foreground, accent) in [
            (bmux_tui::style::Color::White, bmux_tui::style::Color::Cyan),
            (
                bmux_tui::style::Color::Black,
                bmux_tui::style::Color::Magenta,
            ),
        ] {
            let plugin_theme = test_plugin_theme(foreground, accent);
            let resolved = WorkflowSurfaceTheme::resolve(Some(plugin_theme));
            let buffer = render_workspace_buffer_with_theme(&surface, 132, 28, Some(plugin_theme));
            assert_eq!(resolved.text.fg, Some(foreground));
            assert_eq!(resolved.focused.fg, Some(accent));
            assert!(
                buffer
                    .cells()
                    .iter()
                    .any(|cell| cell.style == resolved.focused)
            );
            assert!(
                buffer
                    .cells()
                    .iter()
                    .any(|cell| cell.style == resolved.selected)
            );
            assert_eq!(surface.selected_run_id.as_deref(), Some("run-1"));
        }
    }

    #[test]
    fn missing_selected_detail_is_not_reported_as_loading() {
        let mut surface = projected_surface();
        surface.runs.clear();
        surface.detail_loading_run_id = None;
        let rendered = render_workspace_text(&surface, 88, 24);
        assert!(rendered.contains("Selected run detail is not loaded"));
        assert!(!rendered.contains("Loading selected run"));
    }

    #[test]
    fn responsive_workspace_uses_explicit_narrow_pages_and_bordered_panes() {
        let mut surface = projected_surface();
        let wide = render_workspace_text(&surface, 132, 28);
        assert!(wide.contains("Runs"));
        assert!(wide.contains("Execution graph"));
        assert!(wide.contains("Inspector"));

        let narrow_runs = render_workspace_text(&surface, 62, 24);
        assert!(narrow_runs.contains("Runs"));
        assert!(narrow_runs.contains("Graph"));
        assert!(narrow_runs.contains("Inspector"));
        assert!(narrow_runs.contains("Actions"));
        assert!(narrow_runs.contains("Review"));

        assert_eq!(
            surface.handle_control_center_event(&Event::Key(bmux_keyboard::KeyStroke::simple(
                KeyCode::Char('2')
            ))),
            PluginTuiAction::Redraw
        );
        assert_eq!(surface.narrow_page, WorkflowNarrowPage::Graph);
        assert_eq!(surface.workspace_focus, WorkflowWorkspaceFocus::Graph);
        let narrow_graph = render_workspace_text(&surface, 62, 24);
        assert!(narrow_graph.contains("Execution graph"));
        assert!(narrow_graph.contains("Reviewer"));

        assert_eq!(
            surface.handle_control_center_event(&Event::Key(bmux_keyboard::KeyStroke::simple(
                KeyCode::Char('4')
            ))),
            PluginTuiAction::Redraw
        );
        assert_eq!(surface.narrow_page, WorkflowNarrowPage::Actions);
        assert_eq!(surface.workspace_focus, WorkflowWorkspaceFocus::Actions);
        assert!(render_workspace_text(&surface, 62, 24).contains("Actions"));
        assert_eq!(surface.selected_run_id.as_deref(), Some("run-1"));
        assert_eq!(surface.selected_node_id.as_deref(), Some("reviewer"));
    }

    #[test]
    fn responsive_layout_changes_preserve_stable_selection() {
        let surface = projected_surface();
        for (width, expected) in [(132, "Execution graph"), (88, "Overview"), (62, "Review")] {
            let rendered = render_workspace_text(&surface, width, 24);
            assert!(rendered.contains(expected), "missing {expected} at {width}");
            assert_eq!(surface.selected_run_id.as_deref(), Some("run-1"));
            assert_eq!(surface.selected_node_id.as_deref(), Some("reviewer"));
        }
    }

    #[test]
    fn large_catalog_render_and_navigation_remain_page_bounded() {
        let mut surface = projected_surface();
        let first = surface.catalog.as_ref().expect("catalog").runs[0].clone();
        surface.catalog.as_mut().expect("catalog").runs = (0..100)
            .map(|index| {
                let mut run = first.clone();
                run.run_id = format!("run-{index:03}");
                run.display_title = format!("Review workflow {index:03}");
                run
            })
            .collect();
        surface.selected_run_id = Some("run-000".to_string());

        let rendered = render_workspace_text(&surface, 132, 28);
        assert!(rendered.contains("Showing 100"));
        for _ in 0..150 {
            let _ = surface.handle_control_center_event(&Event::Key(
                bmux_keyboard::KeyStroke::simple(KeyCode::Right),
            ));
        }
        assert_eq!(surface.selected_run_id.as_deref(), Some("run-099"));
        assert_eq!(surface.catalog.as_ref().expect("catalog").runs.len(), 100);
        assert_eq!(surface.runs.len(), 1, "catalog rows do not retain details");
    }

    #[test]
    fn bounded_catalog_pages_and_navigation_do_not_load_unselected_details() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packages/tui/src/plugin_surface_host.rs"
        ));
        assert!(source.contains("const CATALOG_PAGE_SIZE: usize = 100"));
        assert!(source.contains("const RUN_DETAIL_LIMIT: usize = 1_000"));
        assert!(source.contains("WorkflowViewRequest::LoadMore(cursor)"));
        assert!(source.contains("workflow_event_refreshes_selected_detail("));

        let mut surface = projected_surface();
        let first = surface.catalog.as_ref().expect("catalog").runs[0].clone();
        let mut runs = Vec::with_capacity(100);
        for index in 0..100 {
            let mut run = first.clone();
            run.run_id = format!("run-{index:03}");
            runs.push(run);
        }
        let cursor = bcode_workflow_view_models::WorkflowCatalogCursor {
            sort: bcode_workflow_view_models::WorkflowCatalogSort::UpdatedAt,
            timestamp_ms: 1,
            status_rank: 0,
            run_id: "run-099".to_string(),
        };
        let catalog = surface.catalog.as_mut().expect("catalog");
        catalog.runs = runs;
        catalog.has_more = true;
        catalog.next_cursor = Some(cursor.clone());
        surface.selected_run_id = Some("run-050".to_string());
        assert_eq!(surface.runs.len(), 1, "only selected detail is retained");
        assert_eq!(
            surface.handle_control_center_event(&Event::Key(bmux_keyboard::KeyStroke::simple(
                KeyCode::Char('m')
            ))),
            PluginTuiAction::LoadMoreWorkflowRuns { cursor }
        );
        assert_eq!(
            surface.runs.len(),
            1,
            "pagination does not load row details"
        );
    }

    #[test]
    fn projected_updates_replace_authoritative_state_and_surface_degradation() {
        let mut surface = projected_surface();
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        surface.attach_updates(receiver);
        sender
            .try_send(PluginTuiSurfaceUpdate::ResyncRequired)
            .expect("resync update");
        assert_eq!(surface.poll(&TestHost), PluginTuiAction::Redraw);
        assert!(surface.live_status.contains("resync required"));

        sender
            .try_send(PluginTuiSurfaceUpdate::Disconnected {
                message: "offline".to_string(),
            })
            .expect("disconnect update");
        assert_eq!(surface.poll(&TestHost), PluginTuiAction::Redraw);
        assert!(surface.live_status.contains("offline"));
    }

    struct TestHost;

    impl PluginTuiHost for TestHost {
        fn spawn(&self, _task: bcode_plugin_sdk::tui::PluginTask) {}

        fn spawn_blocking(&self, _task: Box<dyn FnOnce() + Send + 'static>) {}

        fn request_redraw(&self) {}
    }
}
