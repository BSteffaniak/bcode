//! Workflow graph/status TUI surface.

use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiAction, PluginTuiHost, PluginTuiRegistry, PluginTuiSurface,
    PluginTuiSurfaceFactory, PluginTuiSurfaceFuture, PluginTuiSurfaceOpenRequest,
    PluginTuiSurfaceUpdate, PluginTuiSurfaceUpdateReceiver,
};
use bmux_keyboard::KeyCode;
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
                active_detail_tab: 0,
                input_buffer: None,
                updates: None,
                catalog: None,
                runs: std::collections::BTreeMap::new(),
                live_status: "loading live workflow state".to_string(),
                subscription_requested: false,
            }) as BoxedPluginTuiSurface)
        })
    }
}

#[derive(Debug)]
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
    active_detail_tab: usize,
    input_buffer: Option<String>,
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

impl WorkflowStatusSurface {
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

    #[allow(clippy::too_many_lines)]
    fn render_workspace(&self, area: Rect, frame: &mut Frame<'_>, theme: WorkflowSurfaceTheme) {
        let Some(catalog) = self.catalog.as_ref() else {
            return;
        };
        if area.height < 4 || area.width < 24 {
            return;
        }
        let header = Rect::new(area.x, area.y, area.width, 2.min(area.height));
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
            let catalog_width = area.width.saturating_mul(28) / 100;
            let inspector_width = area.width.saturating_mul(30) / 100;
            let graph_width = body
                .width
                .saturating_sub(catalog_width)
                .saturating_sub(inspector_width);
            let catalog_area = Rect::new(body.x, body.y, catalog_width, body.height);
            let graph_area = Rect::new(catalog_area.right(), body.y, graph_width, body.height);
            let inspector_area =
                Rect::new(graph_area.right(), body.y, inspector_width, body.height);
            self.render_catalog(catalog_area, frame, theme, catalog);
            self.render_graph(graph_area, frame, theme);
            self.render_inspector(inspector_area, frame, theme, false);
        } else if area.width >= 72 {
            let catalog_width = area.width.saturating_mul(38) / 100;
            let catalog_area = Rect::new(body.x, body.y, catalog_width, body.height);
            let detail_area = Rect::new(
                catalog_area.right(),
                body.y,
                body.width.saturating_sub(catalog_width),
                body.height,
            );
            self.render_catalog(catalog_area, frame, theme, catalog);
            self.render_inspector(detail_area, frame, theme, true);
        } else {
            self.render_narrow_page(body, frame, theme, catalog);
        }
        let hints = [
            KeyHint::new("←/→", "run"),
            KeyHint::new("↑/↓", "node"),
            KeyHint::new("Tab", "section"),
            KeyHint::new("f/s/g", "filter/sort/group"),
            KeyHint::new("m", "more"),
            KeyHint::new("?", "help"),
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
                        " Filter: {:?}  Sort: {:?}  Group: {:?}  Showing {}{}",
                        catalog.filter,
                        catalog.sort,
                        catalog.group,
                        catalog.runs.len(),
                        if catalog.has_more { "+" } else { "" }
                    ),
                    theme.muted,
                )]),
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
                TableRow::rich(vec![
                    Line::from(format!("{group_prefix}{}", run.display_title)),
                    Line::from(format!("{:?}{attention}", run.status)),
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
        let pane = Pane::new()
            .title(Line::from("Execution graph"))
            .padding(Insets::new(1, 1, 1, 1))
            .styles(PaneStyles {
                background: Some(theme.canvas),
                border: theme.component.border,
                focused_border: theme.focused,
            });
        let state = PaneState::new(area);
        pane.render(&state, frame);
        let inner = pane.inner_area(&state);
        let lines = self.selected_run_view().map_or_else(
            || vec![Line::from("Loading selected run…")],
            |run| {
                run.nodes
                    .iter()
                    .map(|node| {
                        let marker =
                            if self.selected_node_id.as_deref() == Some(node.node_id.as_str()) {
                                "▶"
                            } else {
                                " "
                            };
                        let style = workflow_node_style(&node.status, theme);
                        Line::from_spans(vec![Span::styled(
                            format!("{marker} {}  {:?}", node.name, node.status),
                            style,
                        )])
                    })
                    .collect()
            },
        );
        TextView::new(&lines)
            .policy(TextViewPolicy::bare())
            .styles(TextViewStyles {
                text: theme.text,
                empty: theme.muted,
                background: theme.canvas,
            })
            .render(inner, &self.text_view, frame);
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
        if self.active_detail_tab == 3 {
            self.render_action_panel(area, frame, theme);
            return;
        }
        let tabs = [
            TabItem::new("overview", "Overview"),
            TabItem::new("outputs", "Outputs"),
            TabItem::new("attempts", "Attempts"),
            TabItem::new("actions", "Actions"),
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
        let lines = inspector_lines(self.selected_run_view(), self.active_detail_tab);
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
            frame.write_line(area, &Line::from("Loading selected run…"));
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
        match self.active_detail_tab {
            0 => self.render_catalog(area, frame, theme, catalog),
            1 => self.render_graph(area, frame, theme),
            _ => self.render_inspector(area, frame, theme, true),
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
                        serde_json::json!({
                            "workflow_id": workflow_id,
                            "base_revision": revision,
                            "fork_required": true,
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
        if let Some(buffer) = self.input_buffer.as_mut() {
            match key.key {
                KeyCode::Escape => {
                    self.input_buffer = None;
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
                    let value = std::mem::take(buffer);
                    self.input_buffer = None;
                    let Some(run) = self.selected_run_view() else {
                        return PluginTuiAction::None;
                    };
                    let Some(wait) =
                        self.selected_wait(bcode_workflow_view_models::WorkflowWaitKind::Input)
                    else {
                        return PluginTuiAction::None;
                    };
                    return plugin_command(
                        "workflow.provide-input",
                        format!(
                            "run_id={} node_id={} activation_id={} value={value}",
                            run.run.run_id, wait.node_id, wait.activation_id
                        ),
                    );
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
                self.active_detail_tab = (self.active_detail_tab + 1) % 4;
                PluginTuiAction::Redraw
            }
            KeyCode::Char('p') => self
                .selected_run_view()
                .map_or(PluginTuiAction::None, |run| {
                    let command = if run.run.status
                        == bcode_workflow_view_models::WorkflowRunStatus::Paused
                    {
                        "workflow.resume"
                    } else {
                        "workflow.pause"
                    };
                    plugin_command(command, format!("run_id={}", run.run.run_id))
                }),
            KeyCode::Char('c') => self
                .selected_run_view()
                .map_or(PluginTuiAction::None, |run| {
                    plugin_command("workflow.cancel", format!("run_id={}", run.run.run_id))
                }),
            KeyCode::Char('a' | 'd') => {
                let approve = key.key == KeyCode::Char('a');
                let Some(run) = self.selected_run_view() else {
                    return PluginTuiAction::None;
                };
                if let Some(approval) = self.selected_mutation_approval() {
                    return plugin_command(
                        if approve {
                            "workflow.approve-mutation"
                        } else {
                            "workflow.deny-mutation"
                        },
                        format!("approval_id={}", approval.approval_id),
                    );
                }
                let Some(wait) =
                    self.selected_wait(bcode_workflow_view_models::WorkflowWaitKind::Approval)
                else {
                    return PluginTuiAction::None;
                };
                plugin_command(
                    if approve {
                        "workflow.approve"
                    } else {
                        "workflow.deny"
                    },
                    format!(
                        "run_id={} node_id={} activation_id={}",
                        run.run.run_id, wait.node_id, wait.activation_id
                    ),
                )
            }
            KeyCode::Char('i') => {
                if self
                    .selected_wait(bcode_workflow_view_models::WorkflowWaitKind::Input)
                    .is_some()
                {
                    self.input_buffer = Some(String::new());
                    PluginTuiAction::Redraw
                } else {
                    PluginTuiAction::None
                }
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
                plugin_command(
                    "workflow.retry-node",
                    format!(
                        "run_id={} node_id={} activation_id={} failed_attempt={}",
                        run.run.run_id, attempt.node_id, attempt.activation_id, attempt.attempt
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
        if self.catalog.is_some() {
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
                PluginTuiSurfaceUpdate::WorkflowCatalog(catalog) => {
                    if let Err(error) = catalog.validate_version() {
                        self.live_status = error.to_string();
                        continue;
                    }
                    let prior_selection = self.selected_run_id.clone();
                    self.selected_run_id = prior_selection
                        .filter(|run_id| catalog.runs.iter().any(|run| &run.run_id == run_id))
                        .or_else(|| catalog.runs.first().map(|run| run.run_id.clone()));
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
                    self.live_status = "live".to_string();
                }
                PluginTuiSurfaceUpdate::WorkflowRun(view) => {
                    if let Err(error) = view.validate_version() {
                        self.live_status = error.to_string();
                        continue;
                    }
                    let run_id = view.run.run_id.clone();
                    if self.selected_run_id.is_none() {
                        self.selected_run_id = Some(run_id.clone());
                    }
                    if self.selected_run_id.as_deref() == Some(run_id.as_str()) {
                        self.detail_loading_run_id = None;
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
                    self.detail_loading_run_id = Some(run_id);
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
        if self.catalog.is_some() {
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
) -> Vec<Line> {
    let Some(run) = run else {
        return vec![Line::from("Loading selected run…")];
    };
    match tab {
        0 => vec![
            Line::from(run.run.display_title.clone()),
            Line::from(format!("Run: {}", run.run.run_id)),
            Line::from(format!("Status: {:?}", run.run.status)),
            Line::from(format!(
                "Progress: {}/{} completed · {} active · {} blocked",
                run.run.progress.completed,
                run.run.progress.total_nodes,
                run.run.progress.active,
                run.run.progress.blocked
            )),
            Line::from(format!(
                "Definition: {} v{}",
                run.run.definition_id, run.run.definition_version
            )),
            Line::from(format!("Health: {:?}", run.health)),
        ],
        1 => run
            .outputs
            .iter()
            .flat_map(|output| {
                let value = match &output.value {
                    bcode_workflow_view_models::WorkflowOutputValue::Resolved { value } => {
                        value.to_string()
                    }
                    bcode_workflow_view_models::WorkflowOutputValue::Unresolved => {
                        "unresolved".to_string()
                    }
                };
                [
                    Line::from(format!("{} · {}", output.node_id, output.schema_id)),
                    Line::from(value),
                ]
            })
            .collect(),
        2 => run
            .attempts
            .iter()
            .map(|attempt| {
                Line::from(format!(
                    "#{} {} · {}",
                    attempt.attempt, attempt.node_id, attempt.status
                ))
            })
            .collect(),
        _ => run
            .actions
            .iter()
            .map(|action| {
                Line::from(format!(
                    "{:?}{}",
                    action.kind,
                    action
                        .unavailable_reason
                        .as_ref()
                        .map_or(String::new(), |reason| format!(" · {reason}"))
                ))
            })
            .collect(),
    }
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
            actions: Vec::new(),
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
            active_detail_tab: 0,
            input_buffer: None,
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
        assert!(matches!(
            surface.handle_control_center_event(&Event::Key(bmux_keyboard::KeyStroke::simple(KeyCode::Char('c')))),
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
    #[allow(clippy::too_many_lines)]
    fn projected_control_center_handles_navigation_pause_mutation_and_input() {
        let mut surface = projected_surface();
        let second = surface.runs.get("run-1").expect("first run").clone();
        let mut second_item = second.run.clone();
        second_item.run_id = "run-2".to_string();
        second_item.status = bcode_workflow_view_models::WorkflowRunStatus::Paused;
        let mut second = second;
        second.run = second_item.clone();
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
        assert!(matches!(
            surface.handle_control_center_event(&key('d')),
            PluginTuiAction::InvokePluginCommand { ref command_id, .. }
                if command_id == "workflow.deny-mutation"
        ));
        assert_eq!(
            surface.handle_control_center_event(&key('i')),
            PluginTuiAction::Redraw
        );
        assert_eq!(surface.input_buffer.as_deref(), Some(""));
        for character in ['{', '"', 'o', 'k', '"', ':', 't', 'r', 'u', 'e', '}'] {
            assert_eq!(
                surface.handle_control_center_event(&key(character)),
                PluginTuiAction::Redraw
            );
        }
        let submit = surface.handle_control_center_event(&Event::Key(
            bmux_keyboard::KeyStroke::simple(KeyCode::Enter),
        ));
        assert!(matches!(
            submit,
            PluginTuiAction::InvokePluginCommand {
                ref command_id,
                arguments: Some(ref arguments),
                ..
            } if command_id == "workflow.provide-input" && arguments.contains("value={\"ok\":true}")
        ));
        assert!(surface.input_buffer.is_none());

        surface.input_buffer = Some("discard".to_string());
        assert_eq!(
            surface.handle_control_center_event(&Event::Key(bmux_keyboard::KeyStroke::simple(
                KeyCode::Escape
            ),)),
            PluginTuiAction::Redraw
        );
        assert!(surface.input_buffer.is_none());
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
