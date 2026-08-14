//! Workflow graph/status TUI surface.

use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiAction, PluginTuiHost, PluginTuiRegistry, PluginTuiSurface,
    PluginTuiSurfaceFactory, PluginTuiSurfaceFuture, PluginTuiSurfaceOpenRequest,
};
use bmux_keyboard::KeyCode;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text::{Line, Span};
use bmux_tui_components::pane::{Pane, PaneState, PaneStyles};
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
            }) as BoxedPluginTuiSurface)
        })
    }
}

#[derive(Debug)]
struct WorkflowStatusSurface {
    options: serde_json::Value,
    selected_approval: usize,
    text_view: TextViewState,
}

struct WorkflowSurfaceTheme {
    canvas: Style,
    text: Style,
    focused: Style,
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
                    focused: component.focused,
                    component,
                }
            },
            |theme| {
                let component = theme.component_theme().unwrap_or_default();
                Self {
                    canvas: theme.canvas,
                    text: theme.text,
                    focused: theme.focused,
                    component,
                }
            },
        )
    }
}

impl WorkflowStatusSurface {
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

    fn handle_event(&mut self, event: &Event, _host: &dyn PluginTuiHost) -> PluginTuiAction {
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
                mutation_approval_command(&self.options, self.selected_approval, approve)
                    .map_or(PluginTuiAction::None, |command| {
                        PluginTuiAction::RunCommand { command }
                    })
            }
            _ => PluginTuiAction::None,
        }
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
}
