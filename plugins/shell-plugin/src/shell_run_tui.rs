//! Native TUI rendering for shell run artifacts.
//!
//! Terminal replay and emulation are shell-domain behavior. This adapter is the only component
//! that may interpret shell artifact schemas and terminal recording references; generic TUI and
//! transcript code routes opaque plugin visuals without understanding those values.

use bcode_tui_components::terminal_viewer::{
    MAX_INLINE_TERMINAL_ROWS, TerminalViewerInput, TerminalViewerLiveState, TerminalViewerSizing,
    terminal_viewer_rows,
};
use bmux_terminal_grid::{
    Color as GridColor, GridLimits, PhysicalRow, Style as GridStyle, TerminalGrid,
    TerminalGridStream,
};
use bmux_tui::prelude::{Color, Line, Span, Style};
use bmux_tui::style::Modifier;
use std::collections::BTreeMap;
#[cfg(test)]
use std::fs;
use std::sync::Mutex;

const DEFAULT_TERMINAL_COLUMNS: u16 = 120;
const DEFAULT_TERMINAL_ROWS: u16 = 30;
const TERMINAL_PTY_STREAM_REF_KEY: &str = "terminal_pty_stream";
const TERMINAL_PTY_STREAM_CONTENT_TYPE: &str = "application/x-bcode-terminal-pty-stream";
const SHELL_RECORDING_REF_KEY: &str = "shell_recording";
const SHELL_RECORDING_CONTENT_TYPE: &str = "application/x-bcode-shell-recording";

#[derive(Default)]
struct LiveTerminalReplay {
    output: Vec<u8>,
    frames: Vec<TerminalReplayFrame>,
    pending_resizes: Vec<TerminalReplayFrame>,
    last_frame_sequence: u64,
    initial_columns: u16,
    initial_rows: u16,
    columns: u16,
    rows: u16,
    exit_code: Option<i32>,
    signal: Option<String>,
    timed_out: bool,
    cancelled: bool,
}

#[derive(Default)]
struct LiveArtifactReplay {
    decoder: crate::recording::IncrementalShellRecordingDecoder,
    next_offset: u64,
    finalized: bool,
}

/// Native TUI visual adapter for shell run artifacts.
#[derive(Default)]
pub struct ShellRunTuiVisualAdapter {
    live_states: Mutex<BTreeMap<String, TerminalViewerLiveState>>,
    live_replays: Mutex<BTreeMap<String, LiveTerminalReplay>>,
    artifact_replays: Mutex<BTreeMap<String, LiveArtifactReplay>>,
}

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for ShellRunTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        matches!(kind, "bcode.shell.run" | "bcode.tool.request.shell.run")
    }

    fn render_mode(
        &self,
        kind: &str,
        _payload: &serde_json::Value,
    ) -> bcode_plugin_sdk::tui::PluginTuiVisualRenderMode {
        if matches!(kind, "bcode.shell.run" | "bcode.tool.request.shell.run") {
            bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::TranscriptBlock
        } else {
            bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::Inline
        }
    }

    fn invocation_event_action(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        event: &bmux_tui::event::Event,
    ) -> Option<bcode_tool::PluginInvocationAction> {
        if !self.supports(kind) {
            return None;
        }
        let bmux_tui::event::Event::Resize(size) = event else {
            return None;
        };
        let key = payload
            .get("_bcode_runtime")
            .and_then(|runtime| runtime.get("live_state_key"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                payload
                    .get("live_state_key")
                    .and_then(serde_json::Value::as_str)
            });
        if let Some(key) = key
            && let Ok(mut replays) = self.live_replays.lock()
        {
            let replay = replays.entry(key.to_owned()).or_default();
            if replay.initial_columns == 0 || replay.initial_rows == 0 {
                let runtime = payload.get("_bcode_runtime").unwrap_or(payload);
                replay.initial_columns =
                    payload_u16(runtime, "columns").unwrap_or(DEFAULT_TERMINAL_COLUMNS);
                replay.initial_rows = payload_u16(runtime, "rows").unwrap_or(DEFAULT_TERMINAL_ROWS);
            }
            replay.columns = size.width;
            replay.rows = size.height;
            replay.pending_resizes.push(TerminalReplayFrame::Resize {
                columns: size.width,
                rows: size.height,
            });
        }
        Some(bcode_tool::PluginInvocationAction {
            producer_plugin_id: "bcode.shell".to_owned(),
            schema: "bcode.shell.invocation-action".to_owned(),
            schema_version: 1,
            payload: serde_json::json!({
                "type": "resize",
                "columns": size.width,
                "rows": size.height,
            }),
        })
    }

    fn artifact_chunk(
        &self,
        chunk: &bcode_plugin_sdk::tui::PluginTuiArtifactChunk,
    ) -> Result<(), String> {
        if chunk.reference_key != SHELL_RECORDING_REF_KEY || chunk.offset > chunk.total_bytes {
            return Err("invalid shell recording artifact range metadata".to_owned());
        }
        let mut artifacts = self
            .artifact_replays
            .lock()
            .map_err(|_| "shell artifact replay state poisoned".to_owned())?;
        let artifact = artifacts.entry(chunk.tool_call_id.clone()).or_default();
        if chunk.offset != artifact.next_offset {
            return Err(format!(
                "shell recording range is not contiguous: expected {} got {}",
                artifact.next_offset, chunk.offset
            ));
        }
        let frames = artifact
            .decoder
            .push(chunk.offset, &chunk.bytes)
            .map_err(|error| error.to_string())?;
        artifact.next_offset = artifact
            .next_offset
            .saturating_add(u64::try_from(chunk.bytes.len()).unwrap_or(u64::MAX));
        artifact.finalized |= chunk.finalized;
        let dimensions = artifact.decoder.dimensions();
        drop(artifacts);

        let mut replays = self
            .live_replays
            .lock()
            .map_err(|_| "shell live replay state poisoned".to_owned())?;
        let replay = replays.entry(chunk.tool_call_id.clone()).or_default();
        if let Some((columns, rows)) = dimensions
            && (replay.initial_columns == 0 || replay.initial_rows == 0)
        {
            replay.initial_columns = columns;
            replay.initial_rows = rows;
            replay.columns = columns;
            replay.rows = rows;
        }
        for frame in frames {
            match frame {
                crate::recording::ShellRecordingFrame::ReplayOutput { bytes, .. } => {
                    replay.output.extend_from_slice(&bytes);
                    replay.frames.push(TerminalReplayFrame::Output(bytes));
                }
                crate::recording::ShellRecordingFrame::Resize { columns, rows, .. } => {
                    replay.columns = columns;
                    replay.rows = rows;
                    replay
                        .frames
                        .push(TerminalReplayFrame::Resize { columns, rows });
                }
                crate::recording::ShellRecordingFrame::Finish {
                    exit_code,
                    signal,
                    timed_out,
                    cancelled,
                    ..
                } => {
                    replay.exit_code = exit_code;
                    replay.signal = signal;
                    replay.timed_out = timed_out;
                    replay.cancelled = cancelled;
                }
                crate::recording::ShellRecordingFrame::Start { .. }
                | crate::recording::ShellRecordingFrame::Unknown { .. }
                | crate::recording::ShellRecordingFrame::Output { .. } => {}
            }
        }
        drop(replays);
        Ok(())
    }

    fn rows(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> Vec<Line> {
        let width = context.width();
        if kind == "bcode.tool.request.shell.run" {
            return self.shell_request_rows(payload, width, context);
        }
        if kind == "bcode.shell.run"
            && let Some(key) = payload
                .get("_bcode_runtime")
                .and_then(|runtime| runtime.get("live_state_key"))
                .and_then(serde_json::Value::as_str)
            && let Some(replay) = self.live_replay_data(key)
        {
            return Self::shell_result_rows(payload, width, context, &replay, None);
        }
        let mode = payload
            .get("mode")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let payload_columns = payload_u16(payload, "columns").unwrap_or(DEFAULT_TERMINAL_COLUMNS);
        let payload_rows = payload_u16(payload, "rows").unwrap_or(DEFAULT_TERMINAL_ROWS);
        let replay_error = terminal_replay_unavailable_reason(payload);
        let has_artifact_reference = terminal_replay_ref(payload).is_some();
        let replay = TerminalReplayData {
            output: if has_artifact_reference {
                String::new()
            } else {
                match mode {
                    "terminal" => payload
                        .get("output_tail")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    _ => serde_json::to_string_pretty(payload).unwrap_or_default(),
                }
            },
            frames: None,
            columns: payload_columns,
            rows: payload_rows,
            initial_columns: payload_columns,
            initial_rows: payload_rows,
            exit_code: payload_exit_code(payload),
            signal: None,
            timed_out: payload
                .get("timed_out")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            cancelled: payload
                .get("cancelled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        };
        Self::shell_result_rows(payload, width, context, &replay, replay_error)
    }
}

impl ShellRunTuiVisualAdapter {
    fn live_replay_data(&self, key: &str) -> Option<TerminalReplayData> {
        self.live_replays
            .lock()
            .ok()?
            .get(key)
            .map(|replay| TerminalReplayData {
                output: String::from_utf8_lossy(&replay.output).into_owned(),
                frames: Some(replay.frames.clone()),
                columns: replay.columns,
                rows: replay.rows,
                initial_columns: replay.initial_columns,
                initial_rows: replay.initial_rows,
                exit_code: replay.exit_code,
                signal: replay.signal.clone(),
                timed_out: replay.timed_out,
                cancelled: replay.cancelled,
            })
    }

    fn shell_result_rows(
        payload: &serde_json::Value,
        width: u16,
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
        replay: &TerminalReplayData,
        replay_error: Option<String>,
    ) -> Vec<Line> {
        let mut lines = shell_terminal_prompt_rows(payload, width, context);
        lines.extend(shell_replay_status_rows(replay));
        if let Some(error) = replay_error {
            lines.push(Line::from_spans(vec![Span::styled(
                format!("  durable shell recording unavailable: {error}; inline output was not substituted"),
                Style::new().fg(Color::Red),
            )]));
        }
        let input = TerminalViewerInput {
            output: &replay.output,
            columns: replay.initial_columns,
            rows: replay.initial_rows,
            exit_code: replay.exit_code,
            timed_out: Some(replay.timed_out),
            elapsed: None,
            output_truncated: terminal_replay_truncated(payload).unwrap_or_else(|| {
                payload
                    .get("output_truncated")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
            }),
            output_bytes: payload
                .get("output_bytes")
                .and_then(serde_json::Value::as_u64),
            retained_output_bytes: payload
                .get("retained_output_bytes")
                .and_then(serde_json::Value::as_u64),
            show_status: false,
            sizing: TerminalViewerSizing::Compact,
        };
        append_terminal_replay_rows(&mut lines, replay, input, width);
        lines
    }

    fn shell_request_rows(
        &self,
        payload: &serde_json::Value,
        width: u16,
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> Vec<Line> {
        let Some(runtime) = payload.get("_bcode_runtime") else {
            return shell_terminal_prompt_rows(payload, width, context);
        };
        let output = runtime
            .get("output")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let key = runtime
            .get("live_state_key")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                payload
                    .get("arguments")
                    .and_then(|arguments| arguments.get("command"))
                    .and_then(serde_json::Value::as_str)
            })
            .unwrap_or("shell-live-terminal");
        let initial_columns = payload_u16(runtime, "columns").unwrap_or(DEFAULT_TERMINAL_COLUMNS);
        let initial_rows = payload_u16(runtime, "rows").unwrap_or(DEFAULT_TERMINAL_ROWS);
        let live_bytes = output.as_bytes().to_vec();
        let frames = self.update_live_replay(key, &live_bytes, None, initial_columns, initial_rows);
        let streaming = runtime
            .get("streaming")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let mut input = TerminalViewerInput {
            output,
            columns: initial_columns,
            rows: initial_rows,
            exit_code: payload_exit_code(runtime),
            timed_out: runtime
                .get("timed_out")
                .and_then(serde_json::Value::as_bool),
            elapsed: None,
            output_truncated: false,
            output_bytes: None,
            retained_output_bytes: None,
            show_status: false,
            sizing: TerminalViewerSizing::Compact,
        };
        if streaming {
            let visible_rows = self.live_visible_rows(key, input, &frames);
            input.sizing = TerminalViewerSizing::Live {
                visible_rows,
                max_rows: MAX_INLINE_TERMINAL_ROWS,
            };
        }
        let mut lines = shell_terminal_prompt_rows(payload, width, context);
        lines.extend(self.live_replay_status_rows(key, runtime));
        lines.extend(shell_terminal_frame_rows(input, &frames, width));
        lines
    }

    fn update_live_replay(
        &self,
        key: &str,
        output: &[u8],
        incoming_frames: Option<&[(u64, TerminalReplayFrame)]>,
        initial_columns: u16,
        initial_rows: u16,
    ) -> Vec<TerminalReplayFrame> {
        let Ok(mut replays) = self.live_replays.lock() else {
            return vec![TerminalReplayFrame::Output(output.to_vec())];
        };
        let replay = replays
            .entry(key.to_owned())
            .or_insert_with(|| LiveTerminalReplay {
                initial_columns,
                initial_rows,
                columns: initial_columns,
                rows: initial_rows,
                ..LiveTerminalReplay::default()
            });
        if replay.initial_columns == 0 || replay.initial_rows == 0 {
            replay.initial_columns = initial_columns;
            replay.initial_rows = initial_rows;
            replay.columns = initial_columns;
            replay.rows = initial_rows;
        }
        if let Some(incoming_frames) = incoming_frames {
            for (sequence, frame) in incoming_frames {
                if *sequence <= replay.last_frame_sequence {
                    continue;
                }
                if let TerminalReplayFrame::Resize { columns, rows } = frame {
                    replay.columns = *columns;
                    replay.rows = *rows;
                    if let Some(index) = replay
                        .pending_resizes
                        .iter()
                        .position(|pending| pending == frame)
                    {
                        replay.pending_resizes.remove(index);
                    }
                }
                replay.frames.push(frame.clone());
                replay.last_frame_sequence = *sequence;
            }
        } else if !output.is_empty() && output.starts_with(&replay.output) {
            let appended = &output[replay.output.len()..];
            if !appended.is_empty() {
                replay
                    .frames
                    .push(TerminalReplayFrame::Output(appended.to_vec()));
            }
        } else if !output.is_empty() && output != replay.output {
            replay.frames.clear();
            replay.pending_resizes.clear();
            replay
                .frames
                .push(TerminalReplayFrame::Output(output.to_vec()));
            replay.initial_columns = initial_columns;
            replay.initial_rows = initial_rows;
            replay.columns = initial_columns;
            replay.rows = initial_rows;
        }
        if !output.is_empty() {
            replay.output.clear();
            replay.output.extend_from_slice(output);
        }
        let mut frames = replay.frames.clone();
        frames.extend(replay.pending_resizes.iter().cloned());
        frames
    }

    fn live_replay_status_rows(&self, key: &str, fallback: &serde_json::Value) -> Vec<Line> {
        self.live_replays
            .lock()
            .ok()
            .and_then(|replays| {
                replays.get(key).map(|replay| {
                    shell_replay_status_rows(&TerminalReplayData {
                        output: String::new(),
                        frames: None,
                        columns: replay.columns,
                        rows: replay.rows,
                        initial_columns: replay.initial_columns,
                        initial_rows: replay.initial_rows,
                        exit_code: replay.exit_code,
                        signal: replay.signal.clone(),
                        timed_out: replay.timed_out,
                        cancelled: replay.cancelled,
                    })
                })
            })
            .filter(|rows| !rows.is_empty())
            .unwrap_or_else(|| shell_status_rows(fallback))
    }

    fn live_visible_rows(
        &self,
        key: &str,
        input: TerminalViewerInput<'_>,
        frames: &[TerminalReplayFrame],
    ) -> usize {
        let Ok(mut states) = self.live_states.lock() else {
            return 1;
        };
        let state = states.entry(key.to_owned()).or_default();
        let content_rows = shell_terminal_stream(input.columns, input.rows, frames).map_or_else(
            || terminal_viewer_rows(input, u16::MAX).len(),
            |stream| {
                stream
                    .grid()
                    .main_content_tail_rows(MAX_INLINE_TERMINAL_ROWS)
                    .len()
            },
        );
        state.update_rows(content_rows.max(1), MAX_INLINE_TERMINAL_ROWS);
        state.visible_rows()
    }
}

fn shell_replay_status_rows(replay: &TerminalReplayData) -> Vec<Line> {
    let mut parts = Vec::new();
    if replay.cancelled {
        parts.push("cancelled".to_owned());
    } else if replay.timed_out {
        parts.push("timed out".to_owned());
    }
    if let Some(signal) = replay.signal.as_deref() {
        parts.push(format!("signal {signal}"));
    }
    if let Some(exit_code) = replay.exit_code {
        parts.push(format!("exit code {exit_code}"));
    }
    if parts.is_empty() {
        return Vec::new();
    }
    vec![Line::from_spans(vec![
        Span::styled("  ", muted_style()),
        Span::styled(parts.join(" · "), muted_style()),
    ])]
}

fn shell_status_rows(payload: &serde_json::Value) -> Vec<Line> {
    let mut parts = Vec::new();
    if payload
        .get("cancelled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        parts.push("cancelled".to_owned());
    }
    if let Some(exit_code) = payload_exit_code(payload) {
        parts.push(format!("exit code {exit_code}"));
    }
    if parts.is_empty() {
        return Vec::new();
    }
    vec![Line::from_spans(vec![
        Span::styled("  ", muted_style()),
        Span::styled(parts.join(" · "), muted_style()),
    ])]
}

fn shell_terminal_prompt_rows(
    payload: &serde_json::Value,
    _width: u16,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> Vec<Line> {
    let arguments = payload.get("arguments").unwrap_or(payload);
    let command = arguments
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let cwd = arguments.get("cwd").and_then(serde_json::Value::as_str);
    let format_commands = arguments
        .get("format_commands")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    if command.is_empty() {
        return Vec::new();
    }

    let display_command = if format_commands {
        format_shell_command_for_display(command)
    } else {
        command.to_owned()
    };
    display_command
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let mut spans = if index == 0 {
                prompt_spans(cwd, context)
            } else {
                vec![Span::styled("    ", muted_style())]
            };
            spans.extend(shell_command_spans(line));
            Line::from_spans(spans)
        })
        .collect()
}

fn prompt_spans(
    cwd: Option<&str>,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> Vec<Span> {
    let mut spans = vec![Span::styled("  ", muted_style())];
    if let Some(cwd) = cwd {
        spans.push(Span::styled(
            context.display_path(cwd).to_string(),
            path_style(),
        ));
        spans.push(Span::styled(" ❯ ", prompt_style()));
    } else {
        spans.push(Span::styled("❯ ", prompt_style()));
    }
    spans
}

fn format_shell_command_for_display(command: &str) -> String {
    use shuck_formatter::{
        FormattedSource, IndentStyle, ShellDialect, ShellFormatOptions, format_source,
    };

    let options = ShellFormatOptions::default()
        .with_dialect(ShellDialect::Bash)
        .with_indent_style(IndentStyle::Space)
        .with_indent_width(2);
    match format_source(command, None, &options) {
        Ok(FormattedSource::Formatted(formatted)) => trim_formatted_shell_command(&formatted),
        Ok(FormattedSource::Unchanged) | Err(_) => command.to_owned(),
    }
}

fn trim_formatted_shell_command(command: &str) -> String {
    command.trim_end_matches(['\r', '\n']).to_owned()
}

fn shell_command_spans(command: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    for (index, token) in command.split_whitespace().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        let style = if index == 0 {
            command_style()
        } else if token.starts_with('-') {
            flag_style()
        } else if token.starts_with('\'') || token.starts_with('"') {
            string_style()
        } else if matches!(token, "|" | "&&" | "||" | ";" | ">" | ">>" | "<") {
            operator_style()
        } else {
            argument_style()
        };
        spans.push(Span::styled(token.to_owned(), style));
    }
    spans
}

const fn muted_style() -> Style {
    Style::new().fg(Color::BrightBlack)
}

const fn path_style() -> Style {
    Style::new().fg(Color::Blue)
}

const fn prompt_style() -> Style {
    Style::new().fg(Color::Magenta)
}

const fn command_style() -> Style {
    Style::new().fg(Color::Cyan)
}

const fn flag_style() -> Style {
    Style::new().fg(Color::Yellow)
}

const fn string_style() -> Style {
    Style::new().fg(Color::Green)
}

const fn operator_style() -> Style {
    Style::new().fg(Color::BrightBlack)
}

const fn argument_style() -> Style {
    Style::new()
}

fn payload_exit_code(payload: &serde_json::Value) -> Option<i32> {
    payload
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
}

fn append_terminal_replay_rows(
    lines: &mut Vec<Line>,
    replay: &TerminalReplayData,
    input: TerminalViewerInput<'_>,
    width: u16,
) {
    if let Some(frames) = replay.frames.as_deref() {
        lines.extend(shell_terminal_frame_rows(input, frames, width));
    } else {
        lines.extend(terminal_viewer_rows(input, width));
    }
}

fn shell_terminal_stream(
    columns: u16,
    rows: u16,
    frames: &[TerminalReplayFrame],
) -> Option<TerminalGridStream> {
    let output_rows = frames.iter().fold(0_usize, |total, frame| match frame {
        TerminalReplayFrame::Output(bytes) => total.saturating_add(bytes.len()),
        TerminalReplayFrame::Resize { .. } => total,
    });
    let mut stream = TerminalGridStream::new(
        columns.max(1),
        rows.max(1),
        GridLimits {
            // Every retained row requires at least one output byte. This byte-derived bound keeps
            // all possible scrollback for this complete frame set without a terminal-domain cap.
            scrollback_rows: output_rows,
        },
    )
    .ok()?;
    for frame in frames {
        match frame {
            TerminalReplayFrame::Output(bytes) => stream.process(bytes),
            TerminalReplayFrame::Resize { columns, rows } => {
                stream.resize((*columns).max(1), (*rows).max(1)).ok()?;
            }
        }
    }
    Some(stream)
}

fn shell_terminal_frame_rows(
    input: TerminalViewerInput<'_>,
    frames: &[TerminalReplayFrame],
    width: u16,
) -> Vec<Line> {
    let Some(stream) = shell_terminal_stream(input.columns, input.rows, frames) else {
        return terminal_viewer_rows(input, width);
    };
    let grid = stream.grid();
    let max_rows = match input.sizing {
        TerminalViewerSizing::Compact => MAX_INLINE_TERMINAL_ROWS,
        TerminalViewerSizing::Live { max_rows, .. } => max_rows,
    };
    let mut output = grid
        .main_content_tail_rows(max_rows)
        .iter()
        .map(|row| {
            let mut line = shell_terminal_grid_row_to_line(grid, row);
            line.spans
                .insert(0, Span::styled("    ", Style::new().fg(Color::BrightBlack)));
            line
        })
        .collect::<Vec<_>>();
    if let TerminalViewerSizing::Live {
        visible_rows,
        max_rows,
    } = input.sizing
    {
        let target_rows = visible_rows.max(1).min(max_rows);
        if output.len() > target_rows {
            output = output[output.len().saturating_sub(target_rows)..].to_vec();
        }
        while output.len() < target_rows {
            output.push(Line::default());
        }
    }
    output
}

fn shell_terminal_grid_row_to_line(grid: &TerminalGrid, row: &PhysicalRow) -> Line {
    let mut spans = Vec::new();
    let mut current_style = None;
    let mut current_text = String::new();
    for cell in row.cells() {
        if cell.is_wide_continuation() {
            continue;
        }
        let style = shell_terminal_grid_style(grid.palette().get(cell.style()));
        if current_style == Some(style) {
            current_text.push_str(cell.text());
            continue;
        }
        if !current_text.is_empty() {
            spans.push(Span::styled(
                current_text,
                current_style.unwrap_or_default(),
            ));
            current_text = String::new();
        }
        current_style = Some(style);
        current_text.push_str(cell.text());
    }
    if !current_text.is_empty() {
        spans.push(Span::styled(
            current_text,
            current_style.unwrap_or_default(),
        ));
    }
    Line::from_spans(spans)
}

const fn shell_terminal_grid_style(style: GridStyle) -> Style {
    let mut output = Style::new();
    if let Some(fg) = style.fg {
        output = output.fg(shell_terminal_grid_color(fg));
    }
    if let Some(bg) = style.bg {
        output = output.bg(shell_terminal_grid_color(bg));
    }
    if style.bold {
        output = output.add_modifier(Modifier::BOLD);
    }
    if style.italic {
        output = output.add_modifier(Modifier::ITALIC);
    }
    if style.underline {
        output = output.add_modifier(Modifier::UNDERLINE);
    }
    if style.dim {
        output = output.add_modifier(Modifier::DIM);
    }
    if style.inverse {
        output = output.add_modifier(Modifier::REVERSED);
    }
    if style.strike {
        output = output.add_modifier(Modifier::CROSSED_OUT);
    }
    output
}

const fn shell_terminal_grid_color(color: GridColor) -> Color {
    match color {
        GridColor::Indexed(index) => match index {
            0 => Color::Black,
            1 => Color::Red,
            2 => Color::Green,
            3 => Color::Yellow,
            4 => Color::Blue,
            5 => Color::Magenta,
            6 => Color::Cyan,
            7 => Color::White,
            8 => Color::BrightBlack,
            9 => Color::BrightRed,
            10 => Color::BrightGreen,
            11 => Color::BrightYellow,
            12 => Color::BrightBlue,
            13 => Color::BrightMagenta,
            14 => Color::BrightCyan,
            15 => Color::BrightWhite,
            other => Color::Indexed(other),
        },
        GridColor::Rgb { r, g, b } => Color::Rgb(r, g, b),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TerminalReplayFrame {
    Output(Vec<u8>),
    Resize { columns: u16, rows: u16 },
}

pub(crate) struct TerminalReplayData {
    pub(crate) output: String,
    frames: Option<Vec<TerminalReplayFrame>>,
    #[cfg_attr(not(test), allow(dead_code))] // Retained for replay-fidelity validation.
    columns: u16,
    #[cfg_attr(not(test), allow(dead_code))] // Retained for replay-fidelity validation.
    rows: u16,
    initial_columns: u16,
    initial_rows: u16,
    exit_code: Option<i32>,
    signal: Option<String>,
    timed_out: bool,
    cancelled: bool,
}

#[cfg(test)]
enum TerminalReplayOutput {
    Ready(TerminalReplayData),
    Unavailable(String),
    Absent,
}

#[cfg_attr(not(test), allow(dead_code))] // Also validates recording fidelity in crate tests.
pub(crate) fn decode_recording_replay(
    summary: &crate::recording::ShellRecordingSummary,
    frames: Vec<crate::recording::ShellRecordingFrame>,
) -> TerminalReplayData {
    let mut bytes = Vec::new();
    let mut replay_frames = Vec::new();
    let mut replay = TerminalReplayData {
        output: String::new(),
        frames: None,
        columns: summary.columns,
        rows: summary.rows,
        initial_columns: summary.columns,
        initial_rows: summary.rows,
        exit_code: None,
        signal: None,
        timed_out: false,
        cancelled: false,
    };
    let use_presentation_frames = frames.iter().any(|frame| {
        matches!(
            frame,
            crate::recording::ShellRecordingFrame::ReplayOutput { .. }
        )
    });
    for frame in frames {
        match frame {
            crate::recording::ShellRecordingFrame::Output { bytes: output, .. }
            | crate::recording::ShellRecordingFrame::ReplayOutput { bytes: output, .. }
                if matches!(frame, crate::recording::ShellRecordingFrame::Output { .. })
                    != use_presentation_frames =>
            {
                bytes.extend_from_slice(&output);
                replay_frames.push(TerminalReplayFrame::Output(output));
            }
            crate::recording::ShellRecordingFrame::Output { .. }
            | crate::recording::ShellRecordingFrame::ReplayOutput { .. }
            | crate::recording::ShellRecordingFrame::Start { .. }
            | crate::recording::ShellRecordingFrame::Unknown { .. } => {}
            crate::recording::ShellRecordingFrame::Resize { columns, rows, .. } => {
                replay.columns = columns;
                replay.rows = rows;
                replay_frames.push(TerminalReplayFrame::Resize { columns, rows });
            }
            crate::recording::ShellRecordingFrame::Finish {
                exit_code,
                signal,
                timed_out,
                cancelled,
                ..
            } => {
                replay.exit_code = exit_code;
                replay.signal = signal;
                replay.timed_out = timed_out;
                replay.cancelled = cancelled;
            }
        }
    }
    replay.output = String::from_utf8_lossy(&bytes).into_owned();
    replay.frames = Some(replay_frames);
    replay
}

#[cfg(test)]
fn local_recording_path(uri: &str) -> Result<std::path::PathBuf, String> {
    if let Ok(url) = url::Url::parse(uri) {
        if url.scheme() != "file" {
            return Err(format!(
                "recording storage scheme '{}' is not available locally",
                url.scheme()
            ));
        }
        return url
            .to_file_path()
            .map_err(|()| "recording file location is invalid".to_owned());
    }
    let legacy_path = std::path::PathBuf::from(uri);
    if legacy_path.is_absolute() {
        Ok(legacy_path)
    } else {
        Err("recording storage location is invalid".to_owned())
    }
}

#[cfg(test)]
fn terminal_replay_output(payload: &serde_json::Value) -> TerminalReplayOutput {
    let Some(reference) = terminal_replay_ref(payload) else {
        return TerminalReplayOutput::Absent;
    };
    let authoritative =
        reference.get("key").and_then(serde_json::Value::as_str) == Some(SHELL_RECORDING_REF_KEY);
    if authoritative
        && reference
            .get("metadata")
            .and_then(|metadata| metadata.get("availability"))
            .and_then(serde_json::Value::as_str)
            == Some("evicted")
    {
        return TerminalReplayOutput::Unavailable(
            "recording was explicitly evicted by artifact retention policy".to_owned(),
        );
    }
    let Some(uri) = reference
        .get("storage_uri")
        .and_then(serde_json::Value::as_str)
    else {
        return if authoritative {
            TerminalReplayOutput::Unavailable(
                "recording reference has no storage location".to_owned(),
            )
        } else {
            TerminalReplayOutput::Absent
        };
    };
    let path = match local_recording_path(uri) {
        Ok(path) => path,
        Err(error) => return TerminalReplayOutput::Unavailable(error),
    };
    if authoritative {
        return match crate::recording::read_recording(&path) {
            Ok((summary, frames)) => {
                TerminalReplayOutput::Ready(decode_recording_replay(&summary, frames))
            }
            Err(error) => TerminalReplayOutput::Unavailable(format!(
                "recording could not be validated: {error}"
            )),
        };
    }
    fs::read_to_string(path).map_or(TerminalReplayOutput::Absent, |output| {
        TerminalReplayOutput::Ready(TerminalReplayData {
            output,
            frames: None,
            columns: payload_u16(payload, "columns").unwrap_or(DEFAULT_TERMINAL_COLUMNS),
            rows: payload_u16(payload, "rows").unwrap_or(DEFAULT_TERMINAL_ROWS),
            initial_columns: payload_u16(payload, "columns").unwrap_or(DEFAULT_TERMINAL_COLUMNS),
            initial_rows: payload_u16(payload, "rows").unwrap_or(DEFAULT_TERMINAL_ROWS),
            exit_code: payload_exit_code(payload),
            signal: None,
            timed_out: payload
                .get("timed_out")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            cancelled: payload
                .get("cancelled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
    })
}

fn terminal_replay_unavailable_reason(payload: &serde_json::Value) -> Option<String> {
    let reference = terminal_replay_ref(payload)?;
    let authoritative =
        reference.get("key").and_then(serde_json::Value::as_str) == Some(SHELL_RECORDING_REF_KEY);
    if authoritative
        && reference
            .get("metadata")
            .and_then(|metadata| metadata.get("availability"))
            .and_then(serde_json::Value::as_str)
            == Some("evicted")
    {
        Some("recording was explicitly evicted by artifact retention policy".to_owned())
    } else {
        None
    }
}

fn terminal_replay_truncated(payload: &serde_json::Value) -> Option<bool> {
    let reference = terminal_replay_ref(payload)?;
    if reference.get("key").and_then(serde_json::Value::as_str) == Some(SHELL_RECORDING_REF_KEY) {
        return Some(false);
    }
    reference
        .get("metadata")
        .and_then(|metadata| metadata.get("tail_truncated"))
        .and_then(serde_json::Value::as_bool)
}

fn terminal_replay_ref(payload: &serde_json::Value) -> Option<&serde_json::Value> {
    let references = payload
        .get("_artifact_refs")
        .and_then(serde_json::Value::as_array)?;
    references
        .iter()
        .find(|reference| {
            reference.get("key").and_then(serde_json::Value::as_str)
                == Some(SHELL_RECORDING_REF_KEY)
                || reference
                    .get("content_type")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|content_type| {
                        content_type.starts_with(SHELL_RECORDING_CONTENT_TYPE)
                    })
        })
        .or_else(|| {
            references.iter().find(|reference| {
                reference.get("key").and_then(serde_json::Value::as_str)
                    == Some(TERMINAL_PTY_STREAM_REF_KEY)
                    || reference
                        .get("content_type")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|content_type| {
                            content_type.starts_with(TERMINAL_PTY_STREAM_CONTENT_TYPE)
                        })
                    || reference
                        .get("metadata")
                        .and_then(|metadata| metadata.get("stream"))
                        .and_then(serde_json::Value::as_str)
                        == Some("pty")
            })
        })
}

fn payload_u16(payload: &serde_json::Value, key: &str) -> Option<u16> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Covers chunk ordering, decoding, lifecycle, rendering, and duplicate rejection together.
    fn artifact_chunks_incrementally_feed_shell_owned_live_replay_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("live-artifact.bcsr");
        let mut writer =
            crate::recording::ShellRecordingWriter::create(&path, 12, 3).expect("recording writer");
        writer
            .write_replay_output(1, b"first\r\n")
            .expect("first replay");
        writer.write_resize(2, 9, 4).expect("resize");
        writer
            .write_replay_output(3, b"\xffsecond")
            .expect("second replay");
        writer
            .finish(4, Some(0), None, false, false)
            .expect("finish");
        let bytes = std::fs::read(path).expect("recording bytes");
        let adapter = ShellRunTuiVisualAdapter::default();
        let split = bytes.len() / 2;
        for (offset, range) in [(0_usize, &bytes[..split]), (split, &bytes[split..])] {
            bcode_plugin_sdk::tui::PluginTuiVisualAdapter::artifact_chunk(
                &adapter,
                &bcode_plugin_sdk::tui::PluginTuiArtifactChunk {
                    tool_call_id: "call".to_owned(),
                    artifact_id: "artifact".to_owned(),
                    reference_key: SHELL_RECORDING_REF_KEY.to_owned(),
                    producer_plugin_id: "bcode.shell".to_owned(),
                    schema: "bcode.tool.request.shell.run".to_owned(),
                    schema_version: 1,
                    content_type: Some(SHELL_RECORDING_CONTENT_TYPE.to_owned()),
                    offset: u64::try_from(offset).expect("offset"),
                    total_bytes: u64::try_from(bytes.len()).expect("length"),
                    revision: u64::try_from(offset + range.len()).expect("revision"),
                    finalized: offset == split,
                    bytes: range.to_vec(),
                },
            )
            .expect("artifact chunk");
        }
        let replays = adapter.live_replays.lock().expect("live replays");
        let replay = replays.get("call").expect("artifact replay");
        assert_eq!(replay.output, b"first\r\n\xffsecond");
        assert_eq!(
            replay.frames,
            vec![
                TerminalReplayFrame::Output(b"first\r\n".to_vec()),
                TerminalReplayFrame::Resize {
                    columns: 9,
                    rows: 4,
                },
                TerminalReplayFrame::Output(b"\xffsecond".to_vec()),
            ]
        );
        assert_eq!((replay.initial_columns, replay.initial_rows), (12, 3));
        assert_eq!((replay.columns, replay.rows), (9, 4));
        assert_eq!(replay.exit_code, Some(0));
        assert!(!replay.timed_out);
        assert!(!replay.cancelled);
        drop(replays);

        let duplicate = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::artifact_chunk(
            &adapter,
            &bcode_plugin_sdk::tui::PluginTuiArtifactChunk {
                tool_call_id: "call".to_owned(),
                artifact_id: "artifact".to_owned(),
                reference_key: SHELL_RECORDING_REF_KEY.to_owned(),
                producer_plugin_id: "bcode.shell".to_owned(),
                schema: "bcode.tool.request.shell.run".to_owned(),
                schema_version: 1,
                content_type: Some(SHELL_RECORDING_CONTENT_TYPE.to_owned()),
                offset: 0,
                total_bytes: u64::try_from(bytes.len()).expect("length"),
                revision: 3,
                finalized: true,
                bytes: bytes.clone(),
            },
        );
        assert!(duplicate.is_err(), "duplicate ranges must fail closed");

        let payload = serde_json::json!({
            "command": "printf first",
            "_bcode_runtime": {
                "live_state_key": "call",
                "columns": 12,
                "rows": 3,
                "output": "",
                "streaming": true
            }
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &adapter,
            "bcode.tool.request.shell.run",
            &payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                80,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified,
                None,
            ),
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("❯ printf first"), "{rendered}");
        assert!(rendered.contains("first"), "{rendered}");
        assert!(rendered.contains("second"), "{rendered}");
        assert!(rendered.contains("exit code 0"), "{rendered}");
        assert!(
            rows.len() >= 4,
            "live sizing should preserve terminal height"
        );

        let final_payload = serde_json::json!({
            "command": "printf first",
            "mode": "terminal",
            "output_tail": "must-not-be-rendered-again",
            "_bcode_runtime": {"live_state_key": "call"},
            "_artifact_refs": [{
                "key": SHELL_RECORDING_REF_KEY,
                "content_type": SHELL_RECORDING_CONTENT_TYPE,
                "storage_uri": "file:///definitely/missing/recording.bcsr",
                "byte_len": bytes.len(),
                "metadata": {"availability": "complete", "complete": true}
            }]
        });
        let final_rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &adapter,
            "bcode.shell.run",
            &final_payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                80,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified,
                None,
            ),
        );
        let final_rendered = final_rows
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(final_rendered.contains("first"), "{final_rendered}");
        assert!(final_rendered.contains("second"), "{final_rendered}");
        assert!(final_rendered.contains("exit code 0"), "{final_rendered}");
        assert!(!final_rendered.contains("must-not-be-rendered-again"));
        assert!(!final_rendered.contains("recording unavailable"));
    }

    #[test]
    fn live_and_recording_replay_share_exact_frame_and_terminal_state() {
        let adapter = ShellRunTuiVisualAdapter::default();
        let key = "parity-call";
        let mut first = b"\x1b[31mred\x1b[0m\r\nwide: \xe7\x95\x8c e\xcc\x81\r\n".to_vec();
        for index in 0..300 {
            first.extend_from_slice(format!("scrollback-{index:03}\r\n").as_bytes());
        }
        let second = b"\x1b[?1049halt\x1b[2;3H\x1b[32mZ\x1b[?25l";
        let mut cumulative = first.clone();
        let initial_frames = vec![(1, TerminalReplayFrame::Output(first.clone()))];
        adapter.update_live_replay(key, &cumulative, Some(&initial_frames), 12, 3);
        let payload = serde_json::json!({
            "_bcode_runtime": {
                "columns": 12,
                "rows": 3,
                "live_state_key": key,
            }
        });
        let action = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::invocation_event_action(
            &adapter,
            "bcode.tool.request.shell.run",
            &payload,
            &bmux_tui::event::Event::Resize(bmux_tui::geometry::Size::new(9, 4)),
        )
        .expect("resize action");
        cumulative.extend_from_slice(second);
        let incoming_frames = vec![
            (1, TerminalReplayFrame::Output(first.clone())),
            (
                2,
                TerminalReplayFrame::Resize {
                    columns: 9,
                    rows: 4,
                },
            ),
            (3, TerminalReplayFrame::Output(second.to_vec())),
        ];
        let live_frames =
            adapter.update_live_replay(key, &cumulative, Some(&incoming_frames), 12, 3);

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("parity.bcsr");
        let mut writer =
            crate::recording::ShellRecordingWriter::create(&path, 12, 3).expect("recording writer");
        writer.write_output(1, &first).expect("first output");
        writer.write_resize(2, 9, 4).expect("resize");
        writer.write_output(3, second).expect("second output");
        writer
            .finish(4, Some(0), None, false, false)
            .expect("finish recording");
        let (summary, recording_frames) =
            crate::recording::read_recording(&path).expect("read recording");
        let reopened = decode_recording_replay(&summary, recording_frames);
        let reopened_frames = reopened.frames.expect("recording frames");

        assert_eq!(action.producer_plugin_id, "bcode.shell");
        assert_eq!(live_frames, reopened_frames);
        let live = shell_terminal_stream(12, 3, &live_frames).expect("live terminal stream");
        let reopened =
            shell_terminal_stream(12, 3, &reopened_frames).expect("reopened terminal stream");
        let live_rows = live
            .grid()
            .scrollback_rows_hint()
            .saturating_add(live.grid().height());
        let reopened_rows = reopened
            .grid()
            .scrollback_rows_hint()
            .saturating_add(reopened.grid().height());
        assert_eq!(
            live.snapshot(0, live_rows),
            reopened.snapshot(0, reopened_rows)
        );
        assert_eq!(live.grid().mode(), bmux_terminal_grid::GridMode::Alternate);
        assert!(!live.grid().cursor().visible);
        assert!(live.grid().main_content_rows().len() > 300);
    }

    #[test]
    fn live_shell_visual_uses_plugin_owned_resize_dimensions() {
        let adapter = ShellRunTuiVisualAdapter::default();
        let payload = serde_json::json!({
            "arguments": {"command": "printf test"},
            "_bcode_runtime": {
                "output": "12345678ABCD",
                "columns": 8,
                "rows": 24,
                "live_state_key": "call-resize",
                "streaming": true
            }
        });
        let context = bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
            100,
            bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
            None,
        );
        let before = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &adapter,
            "bcode.tool.request.shell.run",
            &payload,
            &context,
        );
        let action = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::invocation_event_action(
            &adapter,
            "bcode.tool.request.shell.run",
            &payload,
            &bmux_tui::event::Event::Resize(bmux_tui::geometry::Size::new(4, 24)),
        );
        let after = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &adapter,
            "bcode.tool.request.shell.run",
            &payload,
            &context,
        );

        assert!(action.is_some());
        assert_ne!(before, after);
        let rendered = after.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("5678\n    ABCD"), "{rendered}");
    }

    #[test]
    fn shell_visual_adapter_owns_resize_action_payload() {
        let action = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::invocation_event_action(
            &ShellRunTuiVisualAdapter::default(),
            "bcode.tool.request.shell.run",
            &serde_json::json!({}),
            &bmux_tui::event::Event::Resize(bmux_tui::geometry::Size::new(132, 40)),
        );
        assert_eq!(
            action,
            Some(bcode_tool::PluginInvocationAction {
                producer_plugin_id: "bcode.shell".to_owned(),
                schema: "bcode.shell.invocation-action".to_owned(),
                schema_version: 1,
                payload: serde_json::json!({
                    "type": "resize",
                    "columns": 132,
                    "rows": 40,
                }),
            })
        );
    }

    #[test]
    fn adapter_formats_shell_commands_by_default() {
        let payload = serde_json::json!({
            "command": "if true;then echo 'hello world';else echo nope;fi",
            "cwd": "/Users/example/project"
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &ShellRunTuiVisualAdapter::default(),
            "bcode.tool.request.shell.run",
            &payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                64,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            ),
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(rendered.contains("; then"), "{rendered}");
        assert!(rendered.contains("; else"), "{rendered}");
        assert!(!rendered.contains(";then"), "{rendered}");
    }

    #[test]
    fn adapter_preserves_unformatted_shell_commands_when_disabled() {
        let command = "if true;then echo 'hello world';else echo nope;fi";
        let payload = serde_json::json!({
            "command": command,
            "format_commands": false
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &ShellRunTuiVisualAdapter::default(),
            "bcode.tool.request.shell.run",
            &payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                48,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            ),
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert_eq!(rendered.lines().count(), 1, "{rendered}");
        assert!(rendered.contains(command), "{rendered}");
    }

    #[test]
    fn adapter_does_not_split_quoted_shell_tokens() {
        let payload = serde_json::json!({
            "command": "printf 'hello world from a quoted argument' && echo done"
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &ShellRunTuiVisualAdapter::default(),
            "bcode.tool.request.shell.run",
            &payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                32,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            ),
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(
            rendered.contains("'hello world from a quoted argument'"),
            "{rendered}"
        );
    }

    #[test]
    fn adapter_renders_terminal_output_tail_from_raw_shell_run_artifact_metadata() {
        let payload = serde_json::json!({
            "mode": "terminal",
            "output_tail": "\u{1b}[31mhello\u{1b}[0m\nworld\n",
            "columns": 80,
            "rows": 24
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &ShellRunTuiVisualAdapter::default(),
            "bcode.shell.run",
            &payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                100,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            ),
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(rendered.contains("hello"), "{rendered}");
        assert!(rendered.contains("world"), "{rendered}");
    }

    fn render_rows(payload: &serde_json::Value) -> Vec<Line> {
        let columns = payload_u16(payload, "columns").unwrap_or(DEFAULT_TERMINAL_COLUMNS);
        let rows = payload_u16(payload, "rows").unwrap_or(DEFAULT_TERMINAL_ROWS);
        let (replay, error) = match terminal_replay_output(payload) {
            TerminalReplayOutput::Ready(replay) => (replay, None),
            TerminalReplayOutput::Unavailable(error) => (
                TerminalReplayData {
                    output: String::new(),
                    frames: None,
                    columns,
                    rows,
                    initial_columns: columns,
                    initial_rows: rows,
                    exit_code: payload_exit_code(payload),
                    signal: None,
                    timed_out: false,
                    cancelled: false,
                },
                Some(error),
            ),
            TerminalReplayOutput::Absent => (
                TerminalReplayData {
                    output: payload
                        .get("output_tail")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    frames: None,
                    columns,
                    rows,
                    initial_columns: columns,
                    initial_rows: rows,
                    exit_code: payload_exit_code(payload),
                    signal: None,
                    timed_out: false,
                    cancelled: false,
                },
                None,
            ),
        };
        ShellRunTuiVisualAdapter::shell_result_rows(
            payload,
            100,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                100,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            ),
            &replay,
            error,
        )
    }

    fn authoritative_recording_payload(path: &std::path::Path) -> serde_json::Value {
        serde_json::json!({
            "mode": "terminal",
            "output_tail": "forbidden fallback sentinel",
            "columns": 80,
            "rows": 24,
            "_artifact_refs": [{
                "key": SHELL_RECORDING_REF_KEY,
                "content_type": SHELL_RECORDING_CONTENT_TYPE,
                "storage_uri": url::Url::from_file_path(path).ok().map(|url| url.to_string()),
                "metadata": {"complete": true}
            }]
        })
    }

    fn assert_recording_unavailable(payload: &serde_json::Value, expected: &str) {
        let rendered = render_rows(payload)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("durable shell recording unavailable"),
            "{rendered}"
        );
        assert!(rendered.contains(expected), "{rendered}");
        assert!(
            !rendered.contains("forbidden fallback sentinel"),
            "{rendered}"
        );
    }

    #[test]
    fn production_adapter_never_uses_model_tail_for_referenced_authoritative_output() {
        let payload = serde_json::json!({
            "mode": "terminal",
            "output_tail": "forbidden bounded model fallback",
            "columns": 80,
            "rows": 24,
            "_artifact_refs": [{
                "key": SHELL_RECORDING_REF_KEY,
                "content_type": SHELL_RECORDING_CONTENT_TYPE,
                "storage_uri": "file:///definitely/not/read/by/rows.bcsr",
                "metadata": {"complete": true}
            }]
        });
        let rendered = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &ShellRunTuiVisualAdapter::default(),
            "bcode.shell.run",
            &payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                100,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            ),
        )
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");

        assert!(
            !rendered.contains("forbidden bounded model fallback"),
            "{rendered}"
        );
    }

    #[test]
    fn explicitly_evicted_recording_is_unavailable_without_fallback() {
        let payload = serde_json::json!({
            "mode": "terminal",
            "output_tail": "forbidden fallback sentinel",
            "columns": 80,
            "rows": 24,
            "_artifact_refs": [{
                "key": SHELL_RECORDING_REF_KEY,
                "content_type": SHELL_RECORDING_CONTENT_TYPE,
                "metadata": {"availability": "evicted", "complete": false}
            }]
        });
        assert_recording_unavailable(&payload, "explicitly evicted");
    }

    #[test]
    fn missing_authoritative_recording_is_explicit_and_never_falls_back() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let payload = authoritative_recording_payload(&temp_dir.path().join("missing.bcsr"));
        assert_recording_unavailable(&payload, "could not be validated");
    }

    #[test]
    fn incomplete_authoritative_recording_is_explicit_and_never_falls_back() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let final_path = temp_dir.path().join("recording.bcsr");
        let partial_path = final_path.with_extension("shell-recording.partial");
        let mut writer = crate::recording::ShellRecordingWriter::create(&final_path, 80, 24)
            .expect("recording writer");
        writer.write_output(1, b"partial").expect("output");
        drop(writer);
        let payload = authoritative_recording_payload(&partial_path);
        assert_recording_unavailable(&payload, "recording is incomplete");
    }

    #[test]
    fn checksum_mismatched_authoritative_recording_is_explicit_and_never_falls_back() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let path = temp_dir.path().join("recording.bcsr");
        let mut writer = crate::recording::ShellRecordingWriter::create(&path, 80, 24)
            .expect("recording writer");
        writer.write_output(1, b"checksum").expect("output");
        writer
            .finish(2, Some(0), None, false, false)
            .expect("finish recording");
        let mut bytes = fs::read(&path).expect("recording bytes");
        let output_payload_offset = 8 + 2 + 2 + 2 + (1 + 8 + 4) + (1 + 8 + 4);
        bytes[output_payload_offset] ^= 0xff;
        fs::write(&path, bytes).expect("corrupt output");
        let payload = authoritative_recording_payload(&path);
        assert_recording_unavailable(&payload, "checksum mismatch");
    }

    #[test]
    fn recording_resize_frames_are_emulated_at_their_exact_stream_position() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let path = temp_dir.path().join("resize-recording.bcsr");
        let mut writer =
            crate::recording::ShellRecordingWriter::create(&path, 8, 24).expect("recording writer");
        writer.write_output(1, b"12345678").expect("output");
        writer.write_resize(2, 4, 24).expect("resize");
        writer.write_output(3, b"ABCD").expect("output");
        writer
            .finish(4, Some(0), None, false, false)
            .expect("finish recording");
        let payload = authoritative_recording_payload(&path);
        let recorded_rows = render_rows(&payload);
        let expected_rows = shell_terminal_frame_rows(
            TerminalViewerInput {
                output: "12345678ABCD",
                columns: 8,
                rows: 24,
                exit_code: Some(0),
                timed_out: Some(false),
                elapsed: None,
                output_truncated: false,
                output_bytes: None,
                retained_output_bytes: None,
                show_status: false,
                sizing: TerminalViewerSizing::Compact,
            },
            &[
                TerminalReplayFrame::Output(b"12345678".to_vec()),
                TerminalReplayFrame::Resize {
                    columns: 4,
                    rows: 24,
                },
                TerminalReplayFrame::Output(b"ABCD".to_vec()),
            ],
            100,
        );
        let rendered_terminal_rows = &recorded_rows[1..];

        assert_eq!(rendered_terminal_rows, expected_rows);
        let final_size_only = terminal_viewer_rows(
            TerminalViewerInput {
                output: "12345678ABCD",
                columns: 4,
                rows: 24,
                exit_code: Some(0),
                timed_out: Some(false),
                elapsed: None,
                output_truncated: false,
                output_bytes: None,
                retained_output_bytes: None,
                show_status: false,
                sizing: TerminalViewerSizing::Compact,
            },
            100,
        );
        assert_ne!(rendered_terminal_rows, final_size_only);
    }

    #[test]
    fn recording_replay_uses_recorded_resize_and_lifecycle_state() {
        for (name, exit_code, signal, timed_out, cancelled) in [
            ("nonzero", Some(7), None, false, false),
            ("signal", Some(1), Some("SIGTERM"), false, false),
            ("timeout", Some(1), Some("SIGHUP"), true, false),
            ("cancelled", Some(1), Some("SIGHUP"), false, true),
        ] {
            let temp_dir = tempfile::tempdir().expect("temp dir");
            let path = temp_dir.path().join(format!("{name}.bcsr"));
            let mut writer = crate::recording::ShellRecordingWriter::create(&path, 80, 24)
                .expect("recording writer");
            writer.write_output(1, b"before resize\n").expect("output");
            writer.write_resize(2, 132, 40).expect("resize");
            writer.write_output(3, b"after resize\n").expect("output");
            writer
                .finish(4, exit_code, signal, timed_out, cancelled)
                .expect("finish recording");
            let payload = authoritative_recording_payload(&path);
            let TerminalReplayOutput::Ready(replay) = terminal_replay_output(&payload) else {
                panic!("{name}: recording should replay");
            };

            assert_eq!((replay.columns, replay.rows), (132, 40), "{name}");
            assert_eq!(replay.exit_code, exit_code, "{name}");
            assert_eq!(replay.signal.as_deref(), signal, "{name}");
            assert_eq!(replay.timed_out, timed_out, "{name}");
            assert_eq!(replay.cancelled, cancelled, "{name}");
            let status = shell_replay_status_rows(&replay)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(status.contains(&format!("exit code {}", exit_code.expect("exit"))));
            if timed_out {
                assert!(status.contains("timed out"), "{name}: {status}");
            }
            if cancelled {
                assert!(status.contains("cancelled"), "{name}: {status}");
            }
            if let Some(signal) = signal {
                assert!(status.contains(signal), "{name}: {status}");
            }
        }
    }

    #[test]
    fn very_large_plain_recording_replays_every_byte_beyond_legacy_tail_limit() {
        const OUTPUT_BYTES: usize = 11 * 1024 * 1024;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let path = temp_dir.path().join("large-recording.bcsr");
        let mut writer = crate::recording::ShellRecordingWriter::create(&path, 80, 24)
            .expect("recording writer");
        let mut expected = Vec::with_capacity(OUTPUT_BYTES);
        let chunk = b"plain terminal output line 0123456789\n";
        let mut offset = 1_u64;
        while expected.len() < OUTPUT_BYTES {
            let remaining = OUTPUT_BYTES.saturating_sub(expected.len());
            let bytes = &chunk[..remaining.min(chunk.len())];
            writer.write_output(offset, bytes).expect("record output");
            expected.extend_from_slice(bytes);
            offset = offset.saturating_add(1);
        }
        writer
            .finish(offset, Some(0), None, false, false)
            .expect("finish recording");
        let payload = authoritative_recording_payload(&path);
        let TerminalReplayOutput::Ready(replayed) = terminal_replay_output(&payload) else {
            panic!("large recording should replay");
        };

        assert_eq!(replayed.output.as_bytes(), expected);
        assert!(replayed.output.len() > 10 * 1024 * 1024);
    }

    #[test]
    fn recording_replay_handles_invalid_and_split_utf8_deterministically() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let path = temp_dir.path().join("recording.bcsr");
        let mut writer = crate::recording::ShellRecordingWriter::create(&path, 80, 24)
            .expect("recording writer");
        writer.write_output(1, b"valid ").expect("first output");
        writer.write_output(2, &[0xe7]).expect("split UTF-8 byte 1");
        writer
            .write_output(3, &[0x95, 0x8c])
            .expect("split UTF-8 bytes 2 and 3");
        writer
            .write_output(4, &[b' ', 0xff, b' ', b'e', 0xcc])
            .expect("invalid and combining prefix");
        writer
            .write_output(5, &[0x81, b'\n'])
            .expect("combining suffix");
        writer
            .finish(6, Some(0), None, false, false)
            .expect("finish recording");
        let payload = authoritative_recording_payload(&path);
        let rendered = render_rows(&payload)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("valid 界"), "{rendered}");
        assert!(rendered.contains('\u{fffd}'), "{rendered}");
        assert!(rendered.contains("e\u{301}"), "{rendered}");
        assert!(
            !rendered.contains("forbidden fallback sentinel"),
            "{rendered}"
        );
    }

    #[test]
    fn recording_migration_preserves_exact_rendered_grid_and_styles() {
        let fixtures: &[(&str, &[u8])] = &[
            ("plain", b"first line\nsecond line\n"),
            (
                "ansi_cursor_and_carriage_return",
                b"\x1b[31mred\x1b[0m plain\nprogress 10%\rprogress 100%\x1b[K\nabc\x1b[2DXY\n",
            ),
            (
                "erase_and_alternate_screen",
                b"before\n\x1b[2J\x1b[Hhome\n\x1b[?1049halt\x1b[32mgreen\x1b[0m\x1b[?1049lafter",
            ),
            ("wide_combining_no_newline", "界 e\u{301} fin".as_bytes()),
        ];

        for (name, output) in fixtures {
            let output = std::str::from_utf8(output).expect("fixture UTF-8");
            let legacy_payload = serde_json::json!({
                "mode": "terminal",
                "output_tail": output,
                "columns": 80,
                "rows": 24,
                "exit_code": 0,
                "timed_out": false,
                "cancelled": false
            });
            let temp_dir = tempfile::tempdir().expect("temp dir");
            let path = temp_dir.path().join("recording.bcsr");
            let mut writer = crate::recording::ShellRecordingWriter::create(&path, 80, 24)
                .expect("recording writer");
            for (sequence, chunk) in output.as_bytes().chunks(3).enumerate() {
                writer
                    .write_output(
                        u64::try_from(sequence).expect("sequence").saturating_add(1),
                        chunk,
                    )
                    .expect("record output");
            }
            writer
                .finish(10_000, Some(0), None, false, false)
                .expect("finish recording");
            let recording_payload = serde_json::json!({
                "mode": "terminal",
                "output_tail": "forbidden fallback sentinel",
                "columns": 80,
                "rows": 24,
                "exit_code": 0,
                "timed_out": false,
                "cancelled": false,
                "_artifact_refs": [{
                    "key": SHELL_RECORDING_REF_KEY,
                    "content_type": SHELL_RECORDING_CONTENT_TYPE,
                    "storage_uri": url::Url::from_file_path(&path).ok().map(|url| url.to_string()),
                    "metadata": {"complete": true}
                }]
            });

            assert_eq!(
                render_rows(&legacy_payload),
                render_rows(&recording_payload),
                "fixture {name}"
            );
        }
    }

    #[test]
    fn corrupt_authoritative_recording_is_explicit_and_never_falls_back() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let path = temp_dir.path().join("recording.bcsr");
        fs::write(&path, b"corrupt").expect("write corrupt recording");
        let payload = serde_json::json!({
            "mode": "terminal",
            "output_tail": "forbidden fallback sentinel",
            "columns": 80,
            "rows": 24,
            "_artifact_refs": [{
                "key": SHELL_RECORDING_REF_KEY,
                "content_type": SHELL_RECORDING_CONTENT_TYPE,
                "storage_uri": url::Url::from_file_path(&path).ok().map(|url| url.to_string()),
                "metadata": {"complete": true}
            }]
        });
        let rendered = render_rows(&payload)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("could not be validated"), "{rendered}");
        assert!(
            !rendered.contains("forbidden fallback sentinel"),
            "{rendered}"
        );
    }

    fn write_version_one_recording(path: &std::path::Path, output: &[u8]) {
        use sha2::{Digest as _, Sha256};

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"BCSHREC\0");
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&80_u16.to_le_bytes());
        bytes.extend_from_slice(&24_u16.to_le_bytes());
        bytes.push(1);
        bytes.extend_from_slice(&1_u64.to_le_bytes());
        bytes.extend_from_slice(
            &u32::try_from(output.len())
                .expect("output length")
                .to_le_bytes(),
        );
        bytes.extend_from_slice(output);
        let mut finish = [0_u8; 38];
        finish[0] = 1;
        finish[1..5].copy_from_slice(&0_i32.to_le_bytes());
        finish[6..].copy_from_slice(&Sha256::digest(output));
        bytes.push(3);
        bytes.extend_from_slice(&2_u64.to_le_bytes());
        bytes.extend_from_slice(&38_u32.to_le_bytes());
        bytes.extend_from_slice(&finish);
        fs::write(path, bytes).expect("version one recording");
    }

    #[test]
    fn mixed_version_authoritative_recording_wins_over_legacy_stream() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let legacy_path = temp_dir.path().join("legacy.txt");
        fs::write(&legacy_path, "forbidden legacy stream").expect("legacy stream");
        let recording_path = temp_dir.path().join("version-one.bcsr");
        write_version_one_recording(&recording_path, b"version one recording sentinel\n");
        let payload = serde_json::json!({
            "mode": "terminal",
            "columns": 80,
            "rows": 24,
            "_artifact_refs": [
                {
                    "key": TERMINAL_PTY_STREAM_REF_KEY,
                    "content_type": TERMINAL_PTY_STREAM_CONTENT_TYPE,
                    "storage_uri": legacy_path.to_string_lossy(),
                    "metadata": {"stream": "pty"}
                },
                {
                    "key": SHELL_RECORDING_REF_KEY,
                    "content_type": "application/x-bcode-shell-recording; version=1",
                    "storage_uri": recording_path.to_string_lossy(),
                    "metadata": {"complete": true}
                }
            ]
        });
        let rendered = render_rows(&payload)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rendered.contains("version one recording sentinel"),
            "{rendered}"
        );
        assert!(!rendered.contains("forbidden legacy stream"), "{rendered}");
    }

    #[test]
    fn interrupted_migration_never_falls_back_to_legacy_stream() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let legacy_path = temp_dir.path().join("legacy.txt");
        fs::write(&legacy_path, "forbidden legacy stream").expect("legacy stream");
        let final_path = temp_dir.path().join("recording.bcsr");
        let partial_path = final_path.with_extension("shell-recording.partial");
        let mut writer = crate::recording::ShellRecordingWriter::create(&final_path, 80, 24)
            .expect("recording writer");
        writer.write_output(1, b"partial bytes").expect("output");
        drop(writer);
        let payload = serde_json::json!({
            "mode": "terminal",
            "columns": 80,
            "rows": 24,
            "_artifact_refs": [
                {
                    "key": TERMINAL_PTY_STREAM_REF_KEY,
                    "content_type": TERMINAL_PTY_STREAM_CONTENT_TYPE,
                    "storage_uri": legacy_path.to_string_lossy(),
                    "metadata": {"stream": "pty"}
                },
                {
                    "key": SHELL_RECORDING_REF_KEY,
                    "content_type": SHELL_RECORDING_CONTENT_TYPE,
                    "storage_uri": partial_path.to_string_lossy(),
                    "metadata": {"complete": false}
                }
            ]
        });
        let rendered = render_rows(&payload)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("recording is incomplete"), "{rendered}");
        assert!(!rendered.contains("forbidden legacy stream"), "{rendered}");
        assert!(!final_path.exists());
    }

    #[test]
    fn legacy_absolute_artifact_path_remains_replayable() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let path = temp_dir.path().join("legacy-absolute.txt");
        fs::write(&path, "legacy absolute path sentinel\n").expect("legacy artifact");
        let payload = serde_json::json!({
            "mode": "terminal",
            "columns": 80,
            "rows": 24,
            "_artifact_refs": [{
                "key": TERMINAL_PTY_STREAM_REF_KEY,
                "content_type": TERMINAL_PTY_STREAM_CONTENT_TYPE,
                "storage_uri": path.to_string_lossy(),
                "metadata": {"stream": "pty"}
            }]
        });
        let rendered = render_rows(&payload)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rendered.contains("legacy absolute path sentinel"),
            "{rendered}"
        );
    }

    #[test]
    fn relative_legacy_artifact_path_is_rejected() {
        let payload = serde_json::json!({
            "mode": "terminal",
            "columns": 80,
            "rows": 24,
            "_artifact_refs": [{
                "key": TERMINAL_PTY_STREAM_REF_KEY,
                "content_type": TERMINAL_PTY_STREAM_CONTENT_TYPE,
                "storage_uri": "relative/legacy.txt",
                "metadata": {"stream": "pty"}
            }]
        });
        assert_recording_unavailable(&payload, "storage location is invalid");
    }

    #[test]
    fn authoritative_recording_is_preferred_over_earlier_legacy_stream() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let legacy_path = temp_dir.path().join("legacy.txt");
        fs::write(&legacy_path, "forbidden legacy stream").expect("legacy stream");
        let recording_path = temp_dir.path().join("recording.bcsr");
        let mut writer = crate::recording::ShellRecordingWriter::create(&recording_path, 80, 24)
            .expect("recording writer");
        writer
            .write_output(1, b"authoritative recording sentinel\n")
            .expect("record output");
        writer
            .finish(2, Some(0), None, false, false)
            .expect("finish recording");
        let payload = serde_json::json!({
            "mode": "terminal",
            "columns": 80,
            "rows": 24,
            "_artifact_refs": [
                {
                    "key": TERMINAL_PTY_STREAM_REF_KEY,
                    "content_type": TERMINAL_PTY_STREAM_CONTENT_TYPE,
                    "storage_uri": url::Url::from_file_path(&legacy_path).ok().map(|url| url.to_string()),
                    "metadata": {"stream": "pty"}
                },
                {
                    "key": SHELL_RECORDING_REF_KEY,
                    "content_type": SHELL_RECORDING_CONTENT_TYPE,
                    "storage_uri": url::Url::from_file_path(&recording_path).ok().map(|url| url.to_string()),
                    "metadata": {"complete": true}
                }
            ]
        });
        let rendered = render_rows(&payload)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rendered.contains("authoritative recording sentinel"),
            "{rendered}"
        );
        assert!(!rendered.contains("forbidden legacy stream"), "{rendered}");
    }

    #[test]
    fn artifact_replay_survives_fresh_process() {
        const CHILD_PATH_ENV: &str = "BCODE_TEST_FRESH_RECORDING_PATH";
        if let Some(path) = std::env::var_os(CHILD_PATH_ENV) {
            let payload = authoritative_recording_payload(std::path::Path::new(&path));
            let rendered = render_rows(&payload)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(rendered.contains("fresh process sentinel"), "{rendered}");
            assert!(
                !rendered.contains("forbidden fallback sentinel"),
                "{rendered}"
            );
            return;
        }

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let path = temp_dir.path().join("recording.bcsr");
        let mut writer = crate::recording::ShellRecordingWriter::create(&path, 80, 24)
            .expect("recording writer");
        writer
            .write_output(1, b"fresh process sentinel\n")
            .expect("record output");
        writer
            .finish(2, Some(0), None, false, false)
            .expect("finish recording");

        let status =
            std::process::Command::new(std::env::current_exe().expect("current test executable"))
                .arg("--exact")
                .arg("shell_run_tui::tests::artifact_replay_survives_fresh_process")
                .arg("--nocapture")
                .env(CHILD_PATH_ENV, &path)
                .status()
                .expect("fresh test process");
        assert!(status.success(), "fresh process replay failed: {status}");
    }

    #[test]
    fn fresh_adapter_renders_complete_shell_recording() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let path = temp_dir.path().join("recording.bcsr");
        let mut writer = crate::recording::ShellRecordingWriter::create(&path, 80, 24)
            .expect("recording writer");
        writer
            .write_output(1, b"first\rsecond\n")
            .expect("record output");
        writer
            .finish(2, Some(0), None, false, false)
            .expect("finish recording");
        let payload = serde_json::json!({
            "mode": "terminal",
            "output_tail": "fallback\n",
            "columns": 80,
            "rows": 24,
            "_artifact_refs": [{
                "key": SHELL_RECORDING_REF_KEY,
                "content_type": SHELL_RECORDING_CONTENT_TYPE,
                "storage_uri": url::Url::from_file_path(&path).ok().map(|url| url.to_string()),
                "metadata": {"complete": true}
            }]
        });
        let rendered = render_rows(&payload)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("second"), "{rendered}");
        assert!(!rendered.contains("first"), "{rendered}");
        assert!(!rendered.contains("fallback"), "{rendered}");
    }

    #[test]
    fn adapter_renders_terminal_replay_ref_through_terminal_viewer() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let path = temp_dir.path().join("pty.txt");
        fs::write(&path, "first\rsecond\n").expect("write pty");
        let payload = serde_json::json!({
            "mode": "terminal",
            "output_tail": "fallback\n",
            "columns": 80,
            "rows": 24,
            "_artifact_refs": [{
                "key": TERMINAL_PTY_STREAM_REF_KEY,
                "content_type": "application/x-bcode-terminal-pty-stream; charset=utf-8",
                "storage_uri": url::Url::from_file_path(&path).ok().map(|url| url.to_string()),
                "byte_len": 13,
                "metadata": {"stream": "pty", "tail_truncated": false}
            }]
        });
        let rendered = render_rows(&payload)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("second"), "{rendered}");
        assert!(!rendered.contains("first"), "{rendered}");
        assert!(!rendered.contains("fallback"), "{rendered}");
    }
}
