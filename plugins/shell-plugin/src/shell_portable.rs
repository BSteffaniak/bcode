//! Producer-owned semantic command/output presentation; no terminal emulator or host execution.
use bcode_tui_components::transcript::{
    tool_visual::{
        CanonicalPluginVisual,
        portable::{
            PortableVisualAdapter, PortableVisualContext, PortableVisualDescriptor,
            PortableVisualOutput, PortableVisualRegistration,
        },
    },
    transcript_layout::TranscriptLayoutRows,
};
use bmux_tui::prelude::{Color, Line, Span, Style};
use std::collections::BTreeSet;

#[cfg(not(feature = "static-bundled"))]
#[path = "shell_run_tui/command.rs"]
mod command;
#[cfg(feature = "static-bundled")]
use crate::shell_run_tui::command;

#[cfg(not(feature = "static-bundled"))]
#[path = "shell_run_tui/partial_json.rs"]
mod partial_json;
#[cfg(feature = "static-bundled")]
use crate::shell_run_tui::partial_json;

#[derive(Default)]
pub struct CommandProjection {
    pub rows: Vec<Line>,
    pub anchors: Vec<bcode_plugin_models::tui_visual::TuiVisualAnchor>,
    pub selection: Vec<(
        usize,
        bcode_plugin_models::tui_selection::PluginTuiSelectionRow,
    )>,
}

pub struct CommandContext {
    theme: Option<bmux_tui_components::theme::ComponentTheme>,
    syntax: Option<CommandSyntax>,
    base: Option<std::path::PathBuf>,
}
#[derive(Clone, Copy)]
pub struct CommandSyntax {
    function: Color,
    keyword: Color,
    string: Color,
    operator: Color,
    pub variable: Color,
    pub number: Color,
}
impl CommandContext {
    #[cfg(feature = "static-bundled")]
    pub(super) fn from_native(
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> Self {
        Self {
            theme: context.theme().and_then(|t| t.component_theme()),
            syntax: context.theme().map(|t| CommandSyntax {
                function: t.syntax.function.to_tui(),
                keyword: t.syntax.keyword.to_tui(),
                string: t.syntax.string.to_tui(),
                operator: t.syntax.operator.to_tui(),
                variable: t.syntax.variable.to_tui(),
                number: t.syntax.number.to_tui(),
            }),
            base: context
                .working_directory()
                .map(std::path::Path::to_path_buf),
        }
    }
    pub(super) fn muted_style(&self) -> Style {
        self.theme
            .map_or(Style::new().fg(Color::BrightBlack), |t| t.muted)
    }
    pub(super) fn prompt_style(&self) -> Style {
        self.theme
            .map_or(Style::new().fg(Color::Magenta), |t| t.focused)
    }
    pub(super) fn argument_style(&self) -> Style {
        self.theme.map_or(Style::new(), |t| t.text)
    }
    pub(super) fn syntax_style(
        &self,
        select: impl FnOnce(CommandSyntax) -> Color,
        fallback: Color,
    ) -> Style {
        Style::new().fg(self.syntax.map_or(fallback, select))
    }
    pub(super) fn command_style(&self) -> Style {
        self.syntax_style(|s| s.function, Color::Cyan)
    }
    pub(super) fn flag_style(&self) -> Style {
        self.syntax_style(|s| s.keyword, Color::Yellow)
    }
    pub(super) fn string_style(&self) -> Style {
        self.syntax_style(|s| s.string, Color::Green)
    }
    pub(super) fn operator_style(&self) -> Style {
        self.syntax_style(|s| s.operator, Color::BrightBlack)
    }
    pub(super) fn prompt_spans(&self, cwd: Option<&str>) -> Vec<Span> {
        let mut spans = vec![Span::styled("  ", self.muted_style())];
        if let Some(cwd) = cwd {
            let path = std::path::Path::new(cwd);
            let display = self
                .base
                .as_ref()
                .and_then(|base| path.strip_prefix(base).ok())
                .map_or_else(
                    || cwd.to_owned(),
                    |path| {
                        if path.as_os_str().is_empty() {
                            ".".to_owned()
                        } else {
                            path.display().to_string()
                        }
                    },
                );
            spans.push(Span::styled(
                display,
                self.theme
                    .map_or(Style::new().fg(Color::Blue), |t| t.focused),
            ));
            spans.push(Span::styled(" ❯ ", self.prompt_style()));
        } else {
            spans.push(Span::styled("❯ ", self.prompt_style()));
        }
        spans
    }
}

/// Local projection policy, not a new wire/plugin contract.
// Mirrors the existing serialized native policy exactly; independent switches are compatibility data.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CommandPresentationPolicy {
    pub version: u32,
    pub enabled: bool,
    pub embedded: bool,
    pub python: bool,
    pub javascript: bool,
    pub nested_shell: bool,
    pub max_source_bytes: usize,
    pub max_regions: usize,
    pub max_depth: usize,
}
impl Default for CommandPresentationPolicy {
    fn default() -> Self {
        Self {
            version: 1,
            enabled: true,
            embedded: true,
            python: true,
            javascript: true,
            nested_shell: true,
            max_source_bytes: 64 * 1024,
            max_regions: 32,
            max_depth: 4,
        }
    }
}

fn semantic_payload(payload: &serde_json::Value) -> serde_json::Value {
    let mut payload = payload.clone();
    if let Some(draft) = payload
        .get("preview")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
    {
        let command = partial_json::string_field(&draft, "command").unwrap_or_default();
        let cwd = partial_json::string_field(&draft, "cwd");
        let format = partial_json::bool_field(&draft, "format_commands");
        if let Some(object) = payload.as_object_mut() {
            object.insert("command".into(), command.into());
            if let Some(cwd) = cwd {
                object.insert("cwd".into(), cwd.into());
            }
            if let Some(format) = format {
                object.insert("format_commands".into(), format.into());
            }
        }
    }
    let preview = payload
        .get("_artifact_refs")
        .and_then(serde_json::Value::as_array)
        .and_then(|refs| refs.iter().find(|r| r["key"] == "clean_output"))
        .and_then(|r| r.get("metadata"))
        .and_then(|m| m.get("semantic_preview"))
        .cloned();
    if let Some(preview) = preview.filter(|p| p["version"] == 1)
        && let Some(object) = payload.as_object_mut()
    {
        object.insert("output".into(), preview["text"].clone());
        if object.get("command").is_none() {
            object.insert("command".into(), preview["command"].clone());
        }
        if object.get("cwd").is_none() {
            object.insert("cwd".into(), preview["cwd"].clone());
        }
        object.insert("output_truncated".into(), preview["truncated"].clone());
        object.insert("output_bytes".into(), preview["bytes"].clone());
    }
    payload
}

#[cfg(feature = "static-bundled")]
pub fn native_projection(
    kind: &str,
    payload: &serde_json::Value,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> Option<bcode_plugin_sdk::tui::PluginTuiVisualProjection> {
    let input = CanonicalPluginVisual {
        invocation_id: None,
        revision: 0,
        producer_plugin_id: Some("bcode.shell".into()),
        schema: kind.into(),
        schema_version: 1,
        title: None,
        subtitle: None,
        payload: payload.clone(),
        streaming: kind == "bcode.shell.output",
    };
    let theme = context.theme().and_then(|t| t.component_theme());
    let output = ShellVisual::project(
        &input,
        &PortableVisualContext {
            width: context.width(),
            theme: theme.as_ref(),
        },
        &CommandContext::from_native(context),
    )?;
    let TranscriptLayoutRows::Selected {
        rows,
        anchors,
        selection,
    } = output.rows
    else {
        return None;
    };
    Some(bcode_plugin_sdk::tui::PluginTuiVisualProjection {
        rows,
        anchors,
        selection: selection.into_iter().collect(),
    })
}

pub struct ShellVisual;
impl PortableVisualAdapter for ShellVisual {
    fn render(
        &self,
        visual: &CanonicalPluginVisual,
        context: &PortableVisualContext<'_>,
    ) -> Option<PortableVisualOutput> {
        let cx = CommandContext {
            theme: context.theme.copied(),
            syntax: None,
            base: None,
        };
        Self::project(visual, context, &cx)
    }
}

impl ShellVisual {
    fn project(
        visual: &CanonicalPluginVisual,
        context: &PortableVisualContext<'_>,
        cx: &CommandContext,
    ) -> Option<PortableVisualOutput> {
        if visual.schema == "bcode.shell.output" {
            let mut semantic_payload = visual.payload.clone();
            if let Some(object) = semantic_payload.as_object_mut() {
                object.retain(|key, _| {
                    !key.starts_with("_bcode_")
                        && !matches!(key.as_str(), "_artifact_refs" | "title" | "subtitle")
                });
            }
            let semantic: crate::shell_output::ShellSemanticOutput =
                serde_json::from_value(semantic_payload).ok()?;
            if semantic.version != 1 {
                return None;
            }
        }
        let payload = &semantic_payload(&visual.payload);
        // Command payload is semantic producer data, never a terminal replay/frame.
        let command = payload
            .get("command")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                payload
                    .get("arguments")
                    .and_then(|v| v.get("command"))
                    .and_then(serde_json::Value::as_str)
            })?;
        let mut policy = match payload.get("presentation_policy").or_else(|| {
            payload
                .get("arguments")
                .and_then(|a| a.get("presentation_policy"))
        }) {
            Some(value) => {
                serde_json::from_value::<CommandPresentationPolicy>(value.clone()).ok()?
            }
            None => CommandPresentationPolicy::default(),
        };
        if policy.version != 1
            || policy.max_source_bytes > 64 * 1024
            || policy.max_depth > 8
            || policy.max_regions > 128
            || command.len() > 64 * 1024
        {
            return None;
        }
        if payload
            .get("format_commands")
            .and_then(serde_json::Value::as_bool)
            == Some(false)
        {
            policy.enabled = false;
        }
        let document = crate::presentation::present_portable(command, &policy);
        let mut projection = command::project_document(
            command,
            payload
                .get("cwd")
                .or_else(|| payload.get("arguments").and_then(|args| args.get("cwd")))
                .and_then(serde_json::Value::as_str),
            context.width,
            cx,
            &document,
        );
        // Plain semantic output only. Never inspect recording references, parse ANSI,
        // instantiate a grid, or fetch arbitrary service/filesystem data in WASM.
        if let Some(output) = payload.get("output").and_then(serde_json::Value::as_str) {
            if output.len() > 128 * 1024 || output.contains('\u{1b}') {
                return None;
            }
            append_output(&mut projection, output, context.width);
        }
        append_status(&mut projection, payload, visual, context, cx, command);
        Some(
            PortableVisualOutput {
                rows: TranscriptLayoutRows::Selected {
                    rows: projection.rows,
                    anchors: projection.anchors,
                    selection: projection.selection.into_iter().collect(),
                },
                ..PortableVisualOutput::default()
            }
            .with_source_copy_actions(),
        )
    }
}

fn append_status(
    projection: &mut CommandProjection,
    payload: &serde_json::Value,
    visual: &CanonicalPluginVisual,
    context: &PortableVisualContext<'_>,
    cx: &CommandContext,
    command: &str,
) {
    let mut statuses = Vec::new();
    for (key, label) in [("cancelled", "cancelled"), ("timed_out", "timed out")] {
        if payload.get(key).and_then(serde_json::Value::as_bool) == Some(true) {
            statuses.push(label.to_owned());
        }
    }
    if let Some(code) = payload.get("exit_code").and_then(serde_json::Value::as_i64) {
        statuses.push(format!("exit code {code}"));
    }
    if command.is_empty() && payload.get("preview").is_some() {
        projection.rows.extend(bmux_tui::text::wrap_line_bounded(
            &Line::from_spans(vec![Span::styled(
                "Shell command · assembling…",
                cx.muted_style(),
            )]),
            bmux_tui::text::TextWrapGeometry::uniform(usize::from(context.width)),
            bmux_tui::text::TextWrap::Character,
        ));
    }
    if payload
        .get("output_truncated")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        statuses.push("output preview truncated".into());
    }
    if visual.streaming && visual.schema == "bcode.shell.output" {
        statuses.push("running".into());
    }
    if !statuses.is_empty() {
        projection.rows.extend(bmux_tui::text::wrap_line_bounded(
            &Line::from_spans(vec![Span::styled(
                format!("  {}", statuses.join(" · ")),
                cx.muted_style(),
            )]),
            bmux_tui::text::TextWrapGeometry::uniform(usize::from(context.width)),
            bmux_tui::text::TextWrap::Character,
        ));
    }
}

fn append_output(projection: &mut CommandProjection, output: &str, width: u16) {
    use bmux_tui::component::{Component, Constraints, LayoutCx};

    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    output.hash(&mut hash);
    let revision = hash.finish();
    let identity = format!("shell-output:{revision:016x}");
    let text = bmux_tui::text::Text::raw(output);
    let block =
        bmux_tui::composition::TextBlock::new(text).wrap(bmux_tui::text::TextWrap::Character);
    let layout = block.layout(
        Constraints::for_width(u64::from(width)),
        &mut LayoutCx::new(),
    );
    let mut frontier = 0usize;
    for measured in block.projection(&layout) {
        let row = projection.rows.len();
        let start = measured.source_range.start.min(output.len());
        let end = measured.source_range.end.min(output.len());
        if !output.is_char_boundary(start) || !output.is_char_boundary(end) {
            continue;
        }
        let cells = bmux_tui::selection::plain_text_fragments(
            "shell-output",
            identity.clone(),
            bmux_tui::geometry::Rect::new(0, 0, width, 1),
            0,
            &output[start..end],
            start,
            revision,
        )
        .into_iter()
        .map(
            |fragment| bcode_plugin_models::tui_selection::PluginTuiSelectionCell {
                column: fragment.area.x,
                width: fragment.area.width,
                bytes: fragment.source_range,
            },
        )
        .collect();
        projection.selection.push((
            row,
            bcode_plugin_models::tui_selection::PluginTuiSelectionRow {
                identity: identity.clone(),
                byte_start: frontier,
                text: output[frontier..end].to_owned(),
                cells,
                revision,
            },
        ));
        projection
            .anchors
            .push(bcode_plugin_models::tui_visual::TuiVisualAnchor {
                key: format!("{identity}:{start}"),
                row,
                source: Some(bcode_plugin_models::tui_visual::TuiVisualSourceRange {
                    identity: identity.clone(),
                    start,
                    end,
                }),
            });
        frontier = end;
        projection
            .rows
            .push(measured.line.viewport(0, usize::from(width)));
    }
    if let Some((_, last)) = projection.selection.last_mut() {
        output[last.byte_start..].clone_into(&mut last.text);
    }
}

/// Static semantic command/result registrations with native manifest identities.
#[must_use]
pub fn portable_visual_registrations() -> Vec<PortableVisualRegistration> {
    [
        ("shell-run-request-card", "bcode.tool.request.shell.run"),
        ("shell-run-terminal-card", "bcode.shell.run"),
        ("shell-semantic-output-card", "bcode.shell.output"),
    ]
    .into_iter()
    .map(|(id, schema)| PortableVisualRegistration {
        descriptor: PortableVisualDescriptor {
            plugin_id: "bcode.shell".into(),
            adapter_id: id.into(),
            schemas: BTreeSet::from([(schema.into(), 1)]),
            priority: 100,
            producer_default: true,
        },
        adapter: Box::new(ShellVisual),
    })
    .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn producer_live_and_final_semantic_previews_render_same_source() {
        use super::*;
        let snapshot = crate::shell_output::ShellSemanticOutput {
            version: 1,
            command: "printf '%s' '界🙂'".into(),
            arguments: serde_json::json!({}),
            output: "界🙂\n".into(),
            output_bytes: 8,
            output_truncated: false,
            complete: false,
        };
        let mut live = CanonicalPluginVisual {
            invocation_id: Some("fixture".into()),
            revision: 1,
            producer_plugin_id: Some("bcode.shell".into()),
            schema: "bcode.shell.output".into(),
            schema_version: 1,
            title: None,
            subtitle: None,
            payload: serde_json::to_value(&snapshot).unwrap(),
            streaming: true,
        };
        let context = PortableVisualContext {
            width: 40,
            theme: None,
        };
        let prepared = ShellVisual.render(&live, &context).unwrap();
        live.schema = "bcode.shell.run".into();
        live.streaming = false;
        live.payload = serde_json::json!({"_artifact_refs":[{"key":"clean_output", "metadata":{"semantic_preview":{
            "version":1, "command":snapshot.command, "text":snapshot.output, "bytes":snapshot.output_bytes, "truncated":false
        }}}]});
        let final_output = ShellVisual.render(&live, &context).unwrap();
        let TranscriptLayoutRows::Selected {
            selection: live_sources,
            ..
        } = &prepared.rows
        else {
            panic!("source-backed live output required");
        };
        let TranscriptLayoutRows::Selected {
            selection: final_sources,
            ..
        } = &final_output.rows
        else {
            panic!("source-backed final output required");
        };
        assert_eq!(live_sources, final_sources);
        let TranscriptLayoutRows::Selected {
            rows: live_rows, ..
        } = &prepared.rows
        else {
            unreachable!();
        };
        assert!(
            live_rows
                .iter()
                .any(|row| row.plain_text().contains("running"))
        );
        #[cfg(feature = "static-bundled")]
        {
            let native = native_projection(
                "bcode.shell.run",
                &live.payload,
                &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                    40,
                    bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified,
                    None,
                ),
            )
            .unwrap();
            assert_eq!(
                final_output.rows,
                TranscriptLayoutRows::Selected {
                    rows: native.rows,
                    anchors: native.anchors,
                    selection: native.selection.into_iter().collect()
                }
            );
        }
    }

    use super::*;
    #[test]
    fn progressive_command_json_preserves_completed_unicode_source_only() {
        use super::*;
        for (preview, expected) in [
            ("{\"command\":\"echo ", "echo "),
            ("{\"command\":\"echo \\u754c", "echo 界"),
            ("{\"command\":\"echo \\u75", "echo "),
        ] {
            let visual = CanonicalPluginVisual {
                invocation_id: Some("draft".into()),
                revision: 1,
                producer_plugin_id: Some("bcode.shell".into()),
                schema: "bcode.tool.request.shell.run".into(),
                schema_version: 1,
                title: None,
                subtitle: None,
                payload: serde_json::json!({"preview":preview}),
                streaming: true,
            };
            let output = ShellVisual
                .render(
                    &visual,
                    &PortableVisualContext {
                        width: 20,
                        theme: None,
                    },
                )
                .unwrap();
            let TranscriptLayoutRows::Selected { selection, .. } = output.rows else {
                panic!("source-backed draft required");
            };
            assert_eq!(
                selection
                    .values()
                    .map(|row| row.text.as_str())
                    .collect::<String>(),
                expected
            );
        }
    }

    #[test]
    fn semantic_output_maps_original_bytes_across_unicode_and_newlines() {
        let output = "alpha 界🙂e\u{301}\nsecond line\n";
        for width in [1, 2, 8, 40] {
            let mut projection = CommandProjection::default();
            append_output(&mut projection, output, width);
            assert!(!projection.rows.is_empty());
            assert!(
                projection
                    .rows
                    .iter()
                    .all(|line| line.width() <= usize::from(width))
            );
            assert!(projection.selection.iter().all(|(_, row)| row.is_valid()));
            assert_eq!(
                projection
                    .selection
                    .iter()
                    .map(|(_, row)| row.text.as_str())
                    .collect::<String>(),
                output
            );
        }
    }
    #[test]
    fn command_and_output_use_source_backed_projection_without_terminal_replay() {
        let input = CanonicalPluginVisual {
            invocation_id: Some("fixture".into()),
            revision: 1,
            producer_plugin_id: Some("bcode.shell".into()),
            schema: "bcode.shell.run".into(),
            schema_version: 1,
            title: None,
            subtitle: None,
            payload: serde_json::json!({"command":"printf '%s' '界🙂'", "output":"界🙂\n", "exit_code":0}),
            streaming: false,
        };
        let output = ShellVisual
            .render(
                &input,
                &PortableVisualContext {
                    width: 40,
                    theme: None,
                },
            )
            .unwrap();
        let TranscriptLayoutRows::Selected {
            rows, selection, ..
        } = output.rows
        else {
            panic!("source projection required");
        };
        let command = selection
            .values()
            .filter(|r| r.identity == "shell-command:original")
            .map(|r| r.text.as_str())
            .collect::<String>();
        assert_eq!(command, "printf '%s' '界🙂'");
        assert!(
            rows.iter()
                .any(|row| row.plain_text().contains("exit code 0"))
        );
        let mut terminal_only = input;
        terminal_only.payload =
            serde_json::json!({"terminal_recording":{"uri":"file:///not-browser-readable"}});
        assert!(
            ShellVisual
                .render(
                    &terminal_only,
                    &PortableVisualContext {
                        width: 40,
                        theme: None
                    }
                )
                .is_none()
        );
    }
}
