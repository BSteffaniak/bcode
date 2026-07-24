//! Workflow graph/status TUI surface.

use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiAction, PluginTuiHost, PluginTuiRegistry, PluginTuiSurface,
    PluginTuiSurfaceFactory, PluginTuiSurfaceFuture, PluginTuiSurfaceOpenRequest,
};
use bmux_keyboard::KeyCode;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::style::{Color, Style};
use bmux_tui::text::Line;

pub const WORKFLOW_STATUS_SURFACE_KIND: &str = "workflow.status";

#[must_use]
pub fn tui_registry() -> PluginTuiRegistry {
    let mut registry = PluginTuiRegistry::default();
    registry.register_factory(Box::new(WorkflowStatusFactory));
    registry
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
            }) as BoxedPluginTuiSurface)
        })
    }
}

#[derive(Debug)]
struct WorkflowStatusSurface {
    options: serde_json::Value,
}

impl PluginTuiSurface for WorkflowStatusSurface {
    fn id(&self) -> &'static str {
        "bcode.workflow-status"
    }

    fn title(&self) -> &'static str {
        "Workflow Status"
    }

    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        frame.fill(area, " ", Style::new().fg(Color::White).bg(Color::Black));
        for (offset, row) in surface_lines(&self.options).into_iter().enumerate() {
            if offset >= usize::from(area.height) {
                break;
            }
            let offset = u16::try_from(offset).expect("workflow status has fewer than u16 rows");
            frame.write_line(
                Rect::new(area.x, area.y.saturating_add(offset), area.width, 1),
                &Line::from(row),
            );
        }
    }

    fn handle_event(&mut self, event: &Event, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        match event {
            Event::Key(key) if matches!(key.key, KeyCode::Escape | KeyCode::Char('q')) => {
                PluginTuiAction::Close { outcome: None }
            }
            _ => PluginTuiAction::None,
        }
    }
}

#[allow(clippy::too_many_lines)]
fn surface_lines(options: &serde_json::Value) -> Vec<String> {
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
    append_named_rows(&mut lines, options, "waits", "Waits", |value| {
        format!(
            "  {} · {} · {}",
            text(value, "node_id"),
            text(value, "kind"),
            text(value, "activation_id")
        )
    });
    append_named_rows(&mut lines, options, "attempts", "Attempts", |value| {
        format!(
            "  {} #{} · {}",
            text(value, "node_id"),
            number(value, "attempt"),
            text(value, "status")
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
    append_named_rows(
        &mut lines,
        options,
        "outputs",
        "Outputs/artifacts",
        |value| {
            format!(
                "  {} · {} · {}",
                text(value, "node_id"),
                text(value, "schema_id"),
                text(value, "artifact_reference")
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
            number(value, "sequence"),
            text(value, "event_type")
        )
    });
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
            lines.push(format!("  [{id}] {}", text(node, "kind")));
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
    fn status_lines_render_daemon_backed_sections() {
        let definition = serde_json::json!({
            "nodes": {"review": {"kind": "agent"}, "approve": {"kind": "approval"}},
            "edges": [{"from": "review", "to": "approve", "kind": "direct"}],
        });
        let lines = surface_lines(&serde_json::json!({
            "command_id": "workflow.inspect",
            "definition": {"definition_json": definition.to_string()},
            "blocks": [{"block_id": "code_review.bundle", "block_version": 1, "plugin_id": "bcode.code_review"}],
            "run": {"run_id": "run-1", "status": "running", "definition_id": "review", "definition_version": 2},
            "waits": [{"node_id": "approve", "kind": "approval", "activation_id": "a1"}],
            "attempts": [{"node_id": "review", "attempt": 1, "status": "succeeded"}],
            "grants": [{"grant_id": "grant-1", "node_id": "review", "scope": {"capability": "read_only"}}],
            "resource_leases": [{"node_id": "review", "mode": "read", "resource_key": "repo"}],
            "outputs": [{"node_id": "review", "schema_id": "review/v1", "artifact_reference": "bcode-artifact://result"}],
            "child_sessions": [{"id": "session-1", "name": "reviewer"}],
            "events": [{"event_seq": 3, "event_type": "attempt_succeeded"}],
        }));
        let rendered = lines.join("\n");
        assert!(rendered.contains("Available plugin blocks (1)"));
        assert!(rendered.contains("code_review.bundle v1 · bcode.code_review"));
        assert!(rendered.contains("Run run-1 · running · review v2"));
        assert!(rendered.contains("Graph (2 nodes, 1 edges)"));
        assert!(rendered.contains("review -> approve · direct"));
        assert!(rendered.contains("Waits (1)"));
        assert!(rendered.contains("Attempts (1)"));
        assert!(rendered.contains("Grants (1)"));
        assert!(rendered.contains("Resource leases (1)"));
        assert!(rendered.contains("Outputs/artifacts (1)"));
        assert!(rendered.contains("Child sessions (1)"));
        assert!(rendered.contains("Events (1)"));
    }
}
