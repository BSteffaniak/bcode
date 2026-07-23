//! Native TUI rendering for shell run artifacts.
//!
//! Terminal replay and emulation are shell-domain behavior. This adapter is the only component
//! that may interpret shell artifact schemas and terminal recording references; generic TUI and
//! transcript code routes opaque plugin visuals without understanding those values.

#[cfg(test)]
use crate::contracts::SHELL_RECORDING_CONTENT_TYPE;
use crate::contracts::{
    SHELL_INVOCATION_INPUT_SCHEMA, SHELL_RECORDING_MEDIA_TYPE, SHELL_RECORDING_REF_KEY,
    SHELL_RUN_SCHEMA, SHELL_SCHEMA_VERSION, ShellInvocationAction,
    TERMINAL_PTY_STREAM_CONTENT_TYPE, TERMINAL_PTY_STREAM_REF_KEY,
};
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

#[derive(Debug, Default)]
struct ShellTuiDiagnostics {
    decode_bytes: u64,
    decode_frames: u64,
    emulate_bytes: u64,
    emulate_frames: u64,
    retained_bytes: u64,
    retained_frames: u64,
    emitted_rows: u64,
    resets: u64,
    discontinuities: u64,
}

const DEFAULT_TERMINAL_COLUMNS: u16 = 120;
const DEFAULT_TERMINAL_ROWS: u16 = 30;
const LIVE_TERMINAL_SCROLLBACK_ROWS: usize = MAX_INLINE_TERMINAL_ROWS * 8;

#[derive(Default)]
struct LiveTerminalReplay {
    output: Vec<u8>,
    frames: Vec<TerminalReplayFrame>,
    stream: Option<TerminalGridStream>,
    pending_resizes: Vec<TerminalReplayFrame>,
    next_input_sequence: u64,
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

impl LiveTerminalReplay {
    fn ensure_stream(&mut self) -> Option<&mut TerminalGridStream> {
        if self.stream.is_none() {
            self.stream = TerminalGridStream::new(
                self.initial_columns.max(1),
                self.initial_rows.max(1),
                GridLimits {
                    scrollback_rows: LIVE_TERMINAL_SCROLLBACK_ROWS,
                },
            )
            .ok();
        }
        self.stream.as_mut()
    }

    fn apply_frame(&mut self, frame: &TerminalReplayFrame) -> bool {
        let Some(stream) = self.ensure_stream() else {
            return false;
        };
        match frame {
            TerminalReplayFrame::Output(bytes) => stream.process(bytes),
            TerminalReplayFrame::Resize { columns, rows } => {
                if stream.resize((*columns).max(1), (*rows).max(1)).is_err() {
                    return false;
                }
            }
        }
        true
    }

    fn reset_stream(&mut self) {
        self.stream = None;
    }
}

#[derive(Default)]
struct LiveArtifactReplay {
    artifact_id: String,
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
    diagnostics: Mutex<ShellTuiDiagnostics>,
}

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for ShellRunTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        matches!(kind, SHELL_RUN_SCHEMA | "bcode.tool.request.shell.run")
    }

    fn render_mode(
        &self,
        kind: &str,
        _payload: &serde_json::Value,
    ) -> bcode_plugin_sdk::tui::PluginTuiVisualRenderMode {
        if matches!(kind, SHELL_RUN_SCHEMA | "bcode.tool.request.shell.run") {
            bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::TranscriptBlock
        } else {
            bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::Inline
        }
    }

    fn invocation_event_input(
        &self,
        invocation_id: &str,
        kind: &str,
        payload: &serde_json::Value,
        event: &bmux_tui::event::Event,
    ) -> Option<bcode_tool::ToolInvocationInput> {
        if !self.supports(kind) {
            return None;
        }
        let bmux_tui::event::Event::Resize(size) = event else {
            return None;
        };
        let input_sequence = if let Ok(mut replays) = self.live_replays.lock() {
            let replay = replays.entry(invocation_id.to_owned()).or_default();
            if replay.initial_columns == 0 || replay.initial_rows == 0 {
                let runtime = payload.get("_bcode_runtime").unwrap_or(payload);
                replay.initial_columns =
                    payload_u16(runtime, "columns").unwrap_or(DEFAULT_TERMINAL_COLUMNS);
                replay.initial_rows = payload_u16(runtime, "rows").unwrap_or(DEFAULT_TERMINAL_ROWS);
            }
            replay.columns = size.width;
            replay.rows = size.height;
            let resize = TerminalReplayFrame::Resize {
                columns: size.width,
                rows: size.height,
            };
            let _ = replay.apply_frame(&resize);
            if let Ok(mut diagnostics) = self.diagnostics.lock() {
                diagnostics.emulate_frames = diagnostics.emulate_frames.saturating_add(1);
            }
            replay.pending_resizes.push(resize);
            let sequence = replay.next_input_sequence;
            replay.next_input_sequence = replay.next_input_sequence.saturating_add(1);
            sequence
        } else {
            return None;
        };
        Some(bcode_tool::ToolInvocationInput {
            input_id: format!("{invocation_id}-input-{input_sequence}"),
            invocation_id: invocation_id.to_owned(),
            producer_id: "bcode.shell".to_owned(),
            schema: SHELL_INVOCATION_INPUT_SCHEMA.to_owned(),
            schema_version: SHELL_SCHEMA_VERSION,
            payload: serde_json::to_value(ShellInvocationAction::Resize {
                columns: size.width,
                rows: size.height,
            })
            .unwrap_or(serde_json::Value::Null),
        })
    }

    fn accepts_artifact_reference(
        &self,
        kind: &str,
        reference_key: &str,
        content_type: Option<&str>,
    ) -> bool {
        self.supports(kind)
            && reference_key == SHELL_RECORDING_REF_KEY
            && content_type
                .is_some_and(|content_type| content_type.starts_with(SHELL_RECORDING_MEDIA_TYPE))
    }

    #[allow(clippy::too_many_lines)]
    fn artifact_chunk(
        &self,
        chunk: &bcode_plugin_sdk::tui::PluginTuiArtifactChunk,
    ) -> Result<(), String> {
        let chunk_len = u64::try_from(chunk.bytes.len()).unwrap_or(u64::MAX);
        if chunk.reference_key != SHELL_RECORDING_REF_KEY
            || chunk.offset > chunk.total_bytes
            || chunk.offset.saturating_add(chunk_len) > chunk.total_bytes
        {
            return Err("invalid shell recording artifact range metadata".to_owned());
        }
        let (frames, dimensions, replaces_replay) = {
            let mut artifacts = self
                .artifact_replays
                .lock()
                .map_err(|_| "shell artifact replay state poisoned".to_owned())?;
            let current = artifacts.get(&chunk.tool_call_id);
            let replaces_replay = current.is_some_and(|artifact| {
                chunk.offset == 0 && artifact.artifact_id != chunk.artifact_id
            });
            let result = if current.is_none() || replaces_replay {
                let mut candidate = LiveArtifactReplay {
                    artifact_id: chunk.artifact_id.clone(),
                    ..LiveArtifactReplay::default()
                };
                let frames = candidate
                    .decoder
                    .push(chunk.offset, &chunk.bytes)
                    .map_err(|error| error.to_string())?;
                candidate.next_offset = candidate.next_offset.saturating_add(chunk_len);
                candidate.finalized |= chunk.finalized;
                let dimensions = candidate.decoder.dimensions();
                artifacts.insert(chunk.tool_call_id.clone(), candidate);
                (frames, dimensions, replaces_replay)
            } else {
                let artifact = artifacts
                    .get_mut(&chunk.tool_call_id)
                    .expect("existing artifact replay");
                if artifact.artifact_id != chunk.artifact_id {
                    return Err("replacement shell recording must start at offset zero".to_owned());
                }
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
                artifact.next_offset = artifact.next_offset.saturating_add(chunk_len);
                artifact.finalized |= chunk.finalized;
                (frames, artifact.decoder.dimensions(), false)
            };
            drop(artifacts);
            result
        };
        if let Ok(mut diagnostics) = self.diagnostics.lock() {
            diagnostics.decode_bytes = diagnostics.decode_bytes.saturating_add(chunk_len);
            diagnostics.decode_frames = diagnostics
                .decode_frames
                .saturating_add(u64::try_from(frames.len()).unwrap_or(u64::MAX));
            if replaces_replay {
                diagnostics.resets = diagnostics.resets.saturating_add(1);
            }
        }

        let mut replays = self
            .live_replays
            .lock()
            .map_err(|_| "shell live replay state poisoned".to_owned())?;
        if replaces_replay {
            replays.remove(&chunk.tool_call_id);
        }
        let replay = replays.entry(chunk.tool_call_id.clone()).or_default();
        if let Some((columns, rows)) = dimensions
            && (replay.initial_columns == 0 || replay.initial_rows == 0)
        {
            replay.initial_columns = columns;
            replay.initial_rows = rows;
            replay.columns = columns;
            replay.rows = rows;
        }
        let decoded_emulate_bytes = frames.iter().fold(0_u64, |total, frame| match frame {
            crate::recording::ShellRecordingFrame::ReplayOutput { bytes, .. } => {
                total.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            }
            _ => total,
        });
        let decoded_emulate_frames = u64::try_from(
            frames
                .iter()
                .filter(|frame| {
                    matches!(
                        frame,
                        crate::recording::ShellRecordingFrame::ReplayOutput { .. }
                            | crate::recording::ShellRecordingFrame::Resize { .. }
                    )
                })
                .count(),
        )
        .unwrap_or(u64::MAX);
        for frame in frames {
            match frame {
                crate::recording::ShellRecordingFrame::ReplayOutput { bytes, .. } => {
                    replay.output.extend_from_slice(&bytes);
                    let frame = TerminalReplayFrame::Output(bytes);
                    let _ = replay.apply_frame(&frame);
                    replay.frames.push(frame);
                }
                crate::recording::ShellRecordingFrame::Resize { columns, rows, .. } => {
                    replay.columns = columns;
                    replay.rows = rows;
                    let frame = TerminalReplayFrame::Resize { columns, rows };
                    let _ = replay.apply_frame(&frame);
                    replay.frames.push(frame);
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
        if let Ok(mut diagnostics) = self.diagnostics.lock() {
            diagnostics.emulate_bytes = diagnostics
                .emulate_bytes
                .saturating_add(decoded_emulate_bytes);
            diagnostics.emulate_frames = diagnostics
                .emulate_frames
                .saturating_add(decoded_emulate_frames);
            diagnostics.retained_bytes = u64::try_from(replay.output.len()).unwrap_or(u64::MAX);
            diagnostics.retained_frames = u64::try_from(replay.frames.len()).unwrap_or(u64::MAX);
        }
        drop(replays);
        Ok(())
    }

    fn drain_diagnostics(&self) -> Vec<bcode_plugin_sdk::tui::PluginTuiDiagnostic> {
        let Ok(mut diagnostics) = self.diagnostics.lock() else {
            return Vec::new();
        };
        let snapshot = std::mem::take(&mut *diagnostics);
        [
            ("decode_bytes", snapshot.decode_bytes),
            ("decode_frames", snapshot.decode_frames),
            ("emulate_bytes", snapshot.emulate_bytes),
            ("emulate_frames", snapshot.emulate_frames),
            ("retained_bytes", snapshot.retained_bytes),
            ("retained_frames", snapshot.retained_frames),
            ("emitted_rows", snapshot.emitted_rows),
            ("reset_total", snapshot.resets),
            ("discontinuity_total", snapshot.discontinuities),
        ]
        .into_iter()
        .filter(|(_, value)| *value > 0)
        .map(|(name, value)| bcode_plugin_sdk::tui::PluginTuiDiagnostic {
            name: name.to_owned(),
            value,
        })
        .collect()
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
        if kind == SHELL_RUN_SCHEMA
            && let Some(key) = payload
                .get("_bcode_runtime")
                .and_then(|runtime| runtime.get("live_state_key"))
                .and_then(serde_json::Value::as_str)
            && let Some(replay) = self.live_replay_data(key)
        {
            return self.live_shell_result_rows(key, payload, width, context, &replay);
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
    fn live_grid_rows(&self, key: &str, input: TerminalViewerInput<'_>) -> Option<Vec<Line>> {
        let replays = self.live_replays.lock().ok()?;
        let replay = replays.get(key)?;
        let stream = replay.stream.as_ref()?;
        let rows = shell_terminal_grid_rows(input, stream.grid());
        drop(replays);
        Some(rows)
    }

    fn live_replay_data(&self, key: &str) -> Option<TerminalReplayData> {
        self.live_replays
            .lock()
            .ok()?
            .get(key)
            .map(|replay| TerminalReplayData {
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
    }

    fn live_shell_result_rows(
        &self,
        key: &str,
        payload: &serde_json::Value,
        width: u16,
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
        replay: &TerminalReplayData,
    ) -> Vec<Line> {
        let mut lines = shell_terminal_prompt_rows(payload, width, context);
        lines.extend(shell_replay_status_rows(replay));
        let input = TerminalViewerInput {
            output: "",
            columns: replay.initial_columns,
            rows: replay.initial_rows,
            exit_code: replay.exit_code,
            timed_out: Some(replay.timed_out),
            elapsed: None,
            output_truncated: terminal_replay_truncated(payload).unwrap_or(false),
            output_bytes: payload
                .get("output_bytes")
                .and_then(serde_json::Value::as_u64),
            retained_output_bytes: payload
                .get("retained_output_bytes")
                .and_then(serde_json::Value::as_u64),
            show_status: false,
            sizing: TerminalViewerSizing::Compact,
        };
        if let Some(rows) = self.live_grid_rows(key, input) {
            if let Ok(mut diagnostics) = self.diagnostics.lock() {
                diagnostics.emitted_rows = diagnostics
                    .emitted_rows
                    .saturating_add(u64::try_from(rows.len()).unwrap_or(u64::MAX));
            }
            lines.extend(rows);
        } else {
            append_terminal_replay_rows(&mut lines, replay, input, width);
        }
        lines
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
        self.update_live_replay(key, &live_bytes, None, initial_columns, initial_rows);
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
            let visible_rows = self.live_visible_rows(key, input);
            input.sizing = TerminalViewerSizing::Live {
                visible_rows,
                max_rows: MAX_INLINE_TERMINAL_ROWS,
            };
        }
        let mut lines = shell_terminal_prompt_rows(payload, width, context);
        lines.extend(self.live_replay_status_rows(key, runtime));
        let retained_rows = self.live_grid_rows(key, input);
        let used_retained_grid = retained_rows.is_some();
        let terminal_rows = retained_rows.unwrap_or_else(|| terminal_viewer_rows(input, width));
        if !used_retained_grid {
            if let Ok(mut diagnostics) = self.diagnostics.lock() {
                diagnostics.resets = diagnostics.resets.saturating_add(1);
            }
        } else if let Ok(mut diagnostics) = self.diagnostics.lock() {
            diagnostics.emitted_rows = diagnostics
                .emitted_rows
                .saturating_add(u64::try_from(terminal_rows.len()).unwrap_or(u64::MAX));
        }
        lines.extend(terminal_rows);
        lines
    }

    #[allow(clippy::too_many_lines)]
    fn update_live_replay(
        &self,
        key: &str,
        output: &[u8],
        incoming_frames: Option<&[(u64, TerminalReplayFrame)]>,
        initial_columns: u16,
        initial_rows: u16,
    ) {
        let Ok(mut replays) = self.live_replays.lock() else {
            return;
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
            let mut emulated_bytes = 0_u64;
            let mut emulated_frames = 0_u64;
            for (sequence, frame) in incoming_frames {
                if *sequence <= replay.last_frame_sequence {
                    continue;
                }
                let mut already_applied = false;
                if let TerminalReplayFrame::Resize { columns, rows } = frame {
                    replay.columns = *columns;
                    replay.rows = *rows;
                    if let Some(index) = replay
                        .pending_resizes
                        .iter()
                        .position(|pending| pending == frame)
                    {
                        replay.pending_resizes.remove(index);
                        already_applied = true;
                    }
                }
                if !already_applied {
                    let _ = replay.apply_frame(frame);
                    emulated_bytes = emulated_bytes.saturating_add(match frame {
                        TerminalReplayFrame::Output(bytes) => {
                            u64::try_from(bytes.len()).unwrap_or(u64::MAX)
                        }
                        TerminalReplayFrame::Resize { .. } => 0,
                    });
                    emulated_frames = emulated_frames.saturating_add(1);
                }
                replay.frames.push(frame.clone());
                replay.last_frame_sequence = *sequence;
            }
            if let Ok(mut diagnostics) = self.diagnostics.lock() {
                diagnostics.emulate_bytes =
                    diagnostics.emulate_bytes.saturating_add(emulated_bytes);
                diagnostics.emulate_frames =
                    diagnostics.emulate_frames.saturating_add(emulated_frames);
                diagnostics.retained_bytes = u64::try_from(replay.output.len()).unwrap_or(u64::MAX);
                diagnostics.retained_frames =
                    u64::try_from(replay.frames.len()).unwrap_or(u64::MAX);
            }
        } else {
            let dimensions_changed = replay.initial_columns != 0
                && replay.initial_rows != 0
                && (replay.initial_columns != initial_columns
                    || replay.initial_rows != initial_rows);
            let discontinuous_output = !output.is_empty()
                && !output.starts_with(&replay.output)
                && output != replay.output;
            if dimensions_changed || discontinuous_output {
                replay.frames.clear();
                replay.pending_resizes.clear();
                replay.initial_columns = initial_columns;
                replay.initial_rows = initial_rows;
                replay.columns = initial_columns;
                replay.rows = initial_rows;
                replay.reset_stream();
                if !output.is_empty() {
                    let frame = TerminalReplayFrame::Output(output.to_vec());
                    let _ = replay.apply_frame(&frame);
                    replay.frames.push(frame);
                }
                if let Ok(mut diagnostics) = self.diagnostics.lock() {
                    diagnostics.resets = diagnostics.resets.saturating_add(1);
                    diagnostics.discontinuities = diagnostics.discontinuities.saturating_add(1);
                    diagnostics.emulate_bytes = diagnostics
                        .emulate_bytes
                        .saturating_add(u64::try_from(output.len()).unwrap_or(u64::MAX));
                    if !output.is_empty() {
                        diagnostics.emulate_frames = diagnostics.emulate_frames.saturating_add(1);
                    }
                }
            } else if !output.is_empty() && output.starts_with(&replay.output) {
                let appended = &output[replay.output.len()..];
                if !appended.is_empty() {
                    let frame = TerminalReplayFrame::Output(appended.to_vec());
                    let _ = replay.apply_frame(&frame);
                    if let Ok(mut diagnostics) = self.diagnostics.lock() {
                        diagnostics.emulate_bytes = diagnostics
                            .emulate_bytes
                            .saturating_add(u64::try_from(appended.len()).unwrap_or(u64::MAX));
                        diagnostics.emulate_frames = diagnostics.emulate_frames.saturating_add(1);
                    }
                    replay.frames.push(frame);
                }
            }
        }
        if !output.is_empty() {
            replay.output.clear();
            replay.output.extend_from_slice(output);
        }
        if let Ok(mut diagnostics) = self.diagnostics.lock() {
            diagnostics.retained_bytes = u64::try_from(replay.output.len()).unwrap_or(u64::MAX);
            diagnostics.retained_frames = u64::try_from(replay.frames.len()).unwrap_or(u64::MAX);
        }
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

    fn live_visible_rows(&self, key: &str, input: TerminalViewerInput<'_>) -> usize {
        let Ok(mut states) = self.live_states.lock() else {
            return 1;
        };
        let state = states.entry(key.to_owned()).or_default();
        let content_rows = self
            .live_replays
            .lock()
            .ok()
            .and_then(|replays| {
                replays.get(key).and_then(|replay| {
                    replay.stream.as_ref().map(|stream| {
                        live_viewport_content_rows(stream.grid(), MAX_INLINE_TERMINAL_ROWS).len()
                    })
                })
            })
            .unwrap_or_else(|| terminal_viewer_rows(input, u16::MAX).len());
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

fn live_viewport_content_rows(grid: &TerminalGrid, max_rows: usize) -> Vec<PhysicalRow> {
    let content_end = (0..grid.height())
        .rev()
        .find(|&row| {
            grid.viewport_row_ref(row)
                .is_some_and(|row| !row.cells().is_empty())
        })
        .map_or(0, |row| row.saturating_add(1));
    let end = content_end
        .max(grid.cursor().row.saturating_add(1))
        .min(grid.height());
    let start = end.saturating_sub(max_rows);
    (start..end)
        .filter_map(|row| grid.viewport_row_ref(row).cloned())
        .collect()
}

fn shell_terminal_grid_rows(input: TerminalViewerInput<'_>, grid: &TerminalGrid) -> Vec<Line> {
    let max_rows = match input.sizing {
        TerminalViewerSizing::Compact => MAX_INLINE_TERMINAL_ROWS,
        TerminalViewerSizing::Live { max_rows, .. } => max_rows,
    };
    let rows = match input.sizing {
        TerminalViewerSizing::Compact => grid.main_content_tail_rows(max_rows),
        TerminalViewerSizing::Live { .. } => live_viewport_content_rows(grid, max_rows),
    };
    let mut output = rows
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

fn shell_terminal_frame_rows(
    input: TerminalViewerInput<'_>,
    frames: &[TerminalReplayFrame],
    width: u16,
) -> Vec<Line> {
    let Some(stream) = shell_terminal_stream(input.columns, input.rows, frames) else {
        return terminal_viewer_rows(input, width);
    };
    shell_terminal_grid_rows(input, stream.grid())
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
                        content_type.starts_with(SHELL_RECORDING_MEDIA_TYPE)
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

    fn live_terminal_rows(frames: &[TerminalReplayFrame], columns: u16, rows: u16) -> Vec<Line> {
        shell_terminal_frame_rows(
            TerminalViewerInput {
                output: "",
                columns,
                rows,
                exit_code: None,
                timed_out: None,
                elapsed: None,
                output_truncated: false,
                output_bytes: None,
                retained_output_bytes: None,
                show_status: false,
                sizing: TerminalViewerSizing::Live {
                    visible_rows: usize::from(rows),
                    max_rows: MAX_INLINE_TERMINAL_ROWS,
                },
            },
            frames,
            columns,
        )
    }

    #[test]
    fn live_terminal_rows_render_current_viewport_without_scrolled_progress_history() {
        let frames = vec![TerminalReplayFrame::Output(
            b"progress 10%\rprogress 20%\x1b[K\r\ncompile one\r\ncompile two\r\nprogress 30%"
                .to_vec(),
        )];

        let rendered = live_terminal_rows(&frames, 40, 3)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(!rendered.contains("progress 10%"), "{rendered}");
        assert!(!rendered.contains("progress 20%"), "{rendered}");
        assert!(rendered.contains("compile one"), "{rendered}");
        assert!(rendered.contains("compile two"), "{rendered}");
        assert!(rendered.contains("progress 30%"), "{rendered}");
    }

    #[test]
    fn live_terminal_rows_apply_carriage_return_in_current_viewport() {
        let frames = vec![TerminalReplayFrame::Output(
            b"progress 10%\rprogress 100%\x1b[K".to_vec(),
        )];

        let rendered = live_terminal_rows(&frames, 40, 3)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(!rendered.contains("progress 10%"), "{rendered}");
        assert!(rendered.contains("progress 100%"), "{rendered}");
    }

    #[test]
    fn shell_adapter_drains_bounded_decode_and_replay_diagnostics() {
        let adapter = ShellRunTuiVisualAdapter::default();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("diagnostics.bcsr");
        let mut writer =
            crate::recording::ShellRecordingWriter::create(&path, 12, 4).expect("writer");
        writer
            .write_replay_output(1, b"hello\r\n")
            .expect("replay output");
        writer
            .finish(2, Some(0), None, false, false)
            .expect("finish");
        let bytes = fs::read(path).expect("recording bytes");
        deliver_recording_range(&adapter, "call", &bytes, 0, bytes.len(), true);
        let payload = serde_json::json!({
            "_bcode_runtime": {
                "live_state_key": "call",
                "streaming": true,
                "columns": 12,
                "rows": 4
            }
        });
        let context = bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
            80,
            bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified,
            None,
        );
        let _ = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &adapter,
            SHELL_RUN_SCHEMA,
            &payload,
            &context,
        );

        let diagnostics =
            bcode_plugin_sdk::tui::PluginTuiVisualAdapter::drain_diagnostics(&adapter);
        let values = diagnostics
            .into_iter()
            .map(|diagnostic| (diagnostic.name, diagnostic.value))
            .collect::<BTreeMap<_, _>>();
        assert!(values.get("decode_bytes").is_some_and(|value| *value > 0));
        assert!(values.get("decode_frames").is_some_and(|value| *value > 0));
        assert!(values.get("emulate_bytes").is_some_and(|value| *value > 0));
        assert!(values.get("emulate_frames").is_some_and(|value| *value > 0));
        assert!(values.get("retained_bytes").is_some_and(|value| *value > 0));
        assert!(values.get("emitted_rows").is_some_and(|value| *value > 0));
        assert!(
            bcode_plugin_sdk::tui::PluginTuiVisualAdapter::drain_diagnostics(&adapter).is_empty()
        );
    }

    fn try_deliver_recording_range(
        adapter: &ShellRunTuiVisualAdapter,
        key: &str,
        artifact_id: &str,
        all_bytes: &[u8],
        offset: usize,
        end: usize,
        finalized: bool,
    ) -> Result<(), String> {
        bcode_plugin_sdk::tui::PluginTuiVisualAdapter::artifact_chunk(
            adapter,
            &bcode_plugin_sdk::tui::PluginTuiArtifactChunk {
                tool_call_id: key.to_owned(),
                artifact_id: artifact_id.to_owned(),
                reference_key: SHELL_RECORDING_REF_KEY.to_owned(),
                producer_plugin_id: "bcode.shell".to_owned(),
                schema: "bcode.tool.request.shell.run".to_owned(),
                schema_version: SHELL_SCHEMA_VERSION,
                content_type: Some(SHELL_RECORDING_CONTENT_TYPE.to_owned()),
                offset: u64::try_from(offset).expect("offset"),
                total_bytes: u64::try_from(all_bytes.len()).expect("length"),
                revision: u64::try_from(end).expect("revision"),
                finalized,
                bytes: all_bytes[offset..end].to_vec(),
            },
        )
    }

    fn deliver_recording_range(
        adapter: &ShellRunTuiVisualAdapter,
        key: &str,
        all_bytes: &[u8],
        offset: usize,
        end: usize,
        finalized: bool,
    ) {
        try_deliver_recording_range(
            adapter,
            key,
            &format!("{key}-artifact"),
            all_bytes,
            offset,
            end,
            finalized,
        )
        .expect("recording range");
    }

    fn render_hydrated_recording(adapter: &ShellRunTuiVisualAdapter, key: &str) -> Vec<Line> {
        let payload = serde_json::json!({
            "command": "fixture",
            "mode": "terminal",
            "_bcode_runtime": {"live_state_key": key},
            "_artifact_refs": [{
                "key": SHELL_RECORDING_REF_KEY,
                "content_type": SHELL_RECORDING_CONTENT_TYPE,
                "metadata": {"availability": "complete", "complete": true}
            }]
        });
        bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            adapter,
            SHELL_RUN_SCHEMA,
            &payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                80,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified,
                None,
            ),
        )
    }

    #[test]
    fn damaged_and_discontinuous_artifact_ranges_preserve_the_last_valid_replay() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("damaged-range.bcsr");
        let mut writer =
            crate::recording::ShellRecordingWriter::create(&path, 20, 4).expect("recording writer");
        writer
            .write_replay_output(1, b"stable before damage\r\n")
            .expect("replay output");
        writer
            .finish(2, Some(0), None, false, false)
            .expect("finish");
        let bytes = std::fs::read(path).expect("recording bytes");
        let adapter = ShellRunTuiVisualAdapter::default();
        let prefix_len = 14 + 13 + 13 + 32 + b"stable before damage\r\n".len();
        deliver_recording_range(&adapter, "call", &bytes, 0, prefix_len, false);
        let before = retained_snapshot(&adapter, "call");

        assert!(
            try_deliver_recording_range(
                &adapter,
                "call",
                "call-artifact",
                &bytes,
                prefix_len + 1,
                bytes.len(),
                true,
            )
            .is_err()
        );
        assert_eq!(retained_snapshot(&adapter, "call"), before);

        let mut damaged = bytes.clone();
        damaged[prefix_len + 9..prefix_len + 13].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(
            try_deliver_recording_range(
                &adapter,
                "call",
                "call-artifact",
                &damaged,
                prefix_len,
                damaged.len(),
                true,
            )
            .is_err()
        );
        assert_eq!(retained_snapshot(&adapter, "call"), before);

        deliver_recording_range(&adapter, "call", &bytes, prefix_len, bytes.len(), true);
        let rendered = render_hydrated_recording(&adapter, "call")
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("stable before damage"), "{rendered}");
        assert!(rendered.contains("exit code 0"), "{rendered}");
    }

    #[test]
    fn finalized_artifact_identity_replacement_rebuilds_from_offset_zero() {
        let dir = tempfile::tempdir().expect("temp dir");
        let first_path = dir.path().join("first.bcsr");
        let mut first_writer = crate::recording::ShellRecordingWriter::create(&first_path, 20, 4)
            .expect("first recording writer");
        first_writer
            .write_replay_output(1, b"old artifact\r\n")
            .expect("old output");
        first_writer
            .finish(2, Some(7), None, false, false)
            .expect("old finish");
        let first = std::fs::read(first_path).expect("first recording bytes");

        let second_path = dir.path().join("second.bcsr");
        let mut second_writer = crate::recording::ShellRecordingWriter::create(&second_path, 9, 3)
            .expect("second recording writer");
        second_writer
            .write_replay_output(1, b"new artifact\r\n")
            .expect("new output");
        second_writer
            .finish(2, Some(0), None, false, false)
            .expect("new finish");
        let second = std::fs::read(second_path).expect("second recording bytes");

        let adapter = ShellRunTuiVisualAdapter::default();
        try_deliver_recording_range(
            &adapter,
            "call",
            "first-artifact",
            &first,
            0,
            first.len(),
            true,
        )
        .expect("first recording");
        try_deliver_recording_range(
            &adapter,
            "call",
            "second-artifact",
            &second,
            0,
            second.len(),
            true,
        )
        .expect("replacement recording");

        let rendered = render_hydrated_recording(&adapter, "call")
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("new artif"), "{rendered}");
        assert!(!rendered.contains("old artifact"), "{rendered}");
        assert!(rendered.contains("exit code 0"), "{rendered}");
        assert!(!rendered.contains("exit code 7"), "{rendered}");
        let replays = adapter.live_replays.lock().expect("live replays");
        let replay = replays.get("call").expect("replacement replay");
        assert_eq!(replay.output, b"new artifact\r\n");
        assert_eq!((replay.initial_columns, replay.initial_rows), (9, 3));
        drop(replays);
        let values = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::drain_diagnostics(&adapter)
            .into_iter()
            .map(|diagnostic| (diagnostic.name, diagnostic.value))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(values.get("reset_total"), Some(&1));
    }

    #[test]
    fn cumulative_discontinuity_and_initial_dimension_change_rebuild_authoritatively() {
        let adapter = ShellRunTuiVisualAdapter::default();
        let key = "cumulative-rebuild";
        adapter.update_live_replay(key, b"12345678ABCD", None, 8, 3);
        adapter.update_live_replay(key, b"replacement", None, 4, 2);

        let replays = adapter.live_replays.lock().expect("live replays");
        let replay = replays.get(key).expect("rebuilt replay");
        assert_eq!((replay.initial_columns, replay.initial_rows), (4, 2));
        assert_eq!((replay.columns, replay.rows), (4, 2));
        assert_eq!(replay.output, b"replacement");
        assert_eq!(
            replay.frames,
            vec![TerminalReplayFrame::Output(b"replacement".to_vec())]
        );
        let retained = replay.stream.as_ref().expect("retained stream");
        let authoritative = shell_terminal_stream(4, 2, &replay.frames).expect("fresh replay");
        let retained_rows = retained
            .grid()
            .scrollback_rows_hint()
            .saturating_add(retained.grid().height());
        let authoritative_rows = authoritative
            .grid()
            .scrollback_rows_hint()
            .saturating_add(authoritative.grid().height());
        assert_eq!(
            retained.snapshot(0, retained_rows),
            authoritative.snapshot(0, authoritative_rows)
        );
        drop(replays);
        let values = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::drain_diagnostics(&adapter)
            .into_iter()
            .map(|diagnostic| (diagnostic.name, diagnostic.value))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(values.get("reset_total"), Some(&1));
        assert_eq!(values.get("discontinuity_total"), Some(&1));
    }

    #[allow(clippy::too_many_lines)]
    #[test]
    fn incremental_and_authoritative_replay_match_terminal_and_lifecycle_matrix() {
        let terminal_prefix = b"\x1b[31mred\x1b[0m plain\r\nwrap-1234567890\r\ncursor-target\x1b[5DXY\r\nprogress 10%\rprogress 100%\x1b[K\r\n\x1b[2J\x1b[Hhome \xe7\x95\x8c e\xcc\x81\r\n";
        let terminal_suffix = b"after-resize\r\n\x1b[32mgreen\x1b[0m";

        for (name, exit_code, signal, timed_out, cancelled) in [
            ("final-status", Some(7), None, false, false),
            ("signal", Some(1), Some("SIGTERM"), false, false),
            ("timeout", Some(1), Some("SIGHUP"), true, false),
            ("cancelled", Some(1), Some("SIGHUP"), false, true),
        ] {
            let dir = tempfile::tempdir().expect("temp dir");
            let path = dir.path().join(format!("{name}.bcsr"));
            let mut writer = crate::recording::ShellRecordingWriter::create(&path, 10, 4)
                .expect("recording writer");
            writer
                .write_replay_output(1, terminal_prefix)
                .expect("terminal prefix");
            writer.write_resize(2, 8, 5).expect("resize");
            writer
                .write_replay_output(3, terminal_suffix)
                .expect("terminal suffix");
            writer
                .finish(4, exit_code, signal, timed_out, cancelled)
                .expect("finish");
            let bytes = std::fs::read(&path).expect("recording bytes");

            let adapter = ShellRunTuiVisualAdapter::default();
            for (index, chunk) in bytes.chunks(17).enumerate() {
                let offset = index.saturating_mul(17);
                deliver_recording_range(
                    &adapter,
                    name,
                    &bytes,
                    offset,
                    offset.saturating_add(chunk.len()),
                    offset.saturating_add(chunk.len()) == bytes.len(),
                );
            }

            let (summary, frames) =
                crate::recording::read_recording(&path).expect("authoritative recording");
            let authoritative = decode_recording_replay(&summary, frames);
            let authoritative_frames = authoritative.frames.as_ref().expect("replay frames");
            let authoritative_stream = shell_terminal_stream(
                authoritative.initial_columns,
                authoritative.initial_rows,
                authoritative_frames,
            )
            .expect("authoritative terminal stream");
            let authoritative_rows = authoritative_stream
                .grid()
                .scrollback_rows_hint()
                .saturating_add(authoritative_stream.grid().height());
            assert_eq!(
                retained_snapshot(&adapter, name),
                authoritative_stream.snapshot(0, authoritative_rows),
                "{name} terminal state"
            );

            let replays = adapter.live_replays.lock().expect("live replays");
            let incremental = replays.get(name).expect("incremental replay");
            assert_eq!(incremental.exit_code, authoritative.exit_code, "{name}");
            assert_eq!(incremental.signal, authoritative.signal, "{name}");
            assert_eq!(incremental.timed_out, authoritative.timed_out, "{name}");
            assert_eq!(incremental.cancelled, authoritative.cancelled, "{name}");
            let incremental_status = shell_replay_status_rows(&TerminalReplayData {
                output: String::new(),
                frames: None,
                columns: incremental.columns,
                rows: incremental.rows,
                initial_columns: incremental.initial_columns,
                initial_rows: incremental.initial_rows,
                exit_code: incremental.exit_code,
                signal: incremental.signal.clone(),
                timed_out: incremental.timed_out,
                cancelled: incremental.cancelled,
            });
            drop(replays);
            assert_eq!(
                incremental_status,
                shell_replay_status_rows(&authoritative),
                "{name} lifecycle status"
            );
        }
    }

    #[test]
    fn normal_live_emulation_processes_each_new_frame_once_at_sustained_volumes() {
        const FRAME_BYTES: usize = 16 * 1024;
        for total_bytes in [64 * 1024, 1024 * 1024, 8 * 1024 * 1024] {
            let dir = tempfile::tempdir().expect("temp dir");
            let path = dir.path().join(format!("scaling-{total_bytes}.bcsr"));
            let mut writer = crate::recording::ShellRecordingWriter::create(&path, 80, 24)
                .expect("recording writer");
            let frame = vec![b'x'; FRAME_BYTES];
            for sequence in 0..(total_bytes / FRAME_BYTES) {
                writer
                    .write_replay_output(
                        u64::try_from(sequence).expect("sequence").saturating_add(1),
                        &frame,
                    )
                    .expect("replay output");
            }
            writer
                .finish(u64::MAX, Some(0), None, false, false)
                .expect("finish");
            let bytes = std::fs::read(path).expect("recording bytes");
            let adapter = ShellRunTuiVisualAdapter::default();
            let key = format!("scaling-{total_bytes}");
            let mut offset = 14 + 13;
            deliver_recording_range(&adapter, &key, &bytes, 0, offset, false);
            let _ = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::drain_diagnostics(&adapter);

            let encoded_frame_bytes = 13 + 32 + FRAME_BYTES;
            let frame_count = total_bytes / FRAME_BYTES;
            for _ in 0..frame_count {
                let end = offset.saturating_add(encoded_frame_bytes);
                deliver_recording_range(&adapter, &key, &bytes, offset, end, false);
                offset = end;
                let values =
                    bcode_plugin_sdk::tui::PluginTuiVisualAdapter::drain_diagnostics(&adapter)
                        .into_iter()
                        .map(|diagnostic| (diagnostic.name, diagnostic.value))
                        .collect::<BTreeMap<_, _>>();
                assert_eq!(
                    values.get("emulate_bytes"),
                    Some(&u64::try_from(FRAME_BYTES).expect("frame bytes")),
                    "{total_bytes} byte stream"
                );
                assert_eq!(values.get("emulate_frames"), Some(&1));
                assert!(!values.contains_key("reset_total"));
                assert!(!values.contains_key("discontinuity_total"));
            }
            deliver_recording_range(&adapter, &key, &bytes, offset, bytes.len(), true);
            let values = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::drain_diagnostics(&adapter)
                .into_iter()
                .map(|diagnostic| (diagnostic.name, diagnostic.value))
                .collect::<BTreeMap<_, _>>();
            assert!(!values.contains_key("emulate_bytes"));
            assert!(!values.contains_key("emulate_frames"));
            let (retained_output_bytes, retained_frames) = {
                let replays = adapter.live_replays.lock().expect("live replays");
                let replay = replays.get(&key).expect("retained replay");
                let values = (replay.output.len(), replay.frames.len());
                drop(replays);
                values
            };
            assert_eq!(retained_output_bytes, total_bytes);
            assert_eq!(retained_frames, frame_count);
        }
    }

    #[test]
    fn seventeen_byte_live_frames_are_emulated_once_without_replaying_history() {
        const TOTAL_BYTES: usize = 64 * 1024;
        const FRAME_BYTES: usize = 17;
        let adapter = ShellRunTuiVisualAdapter::default();
        let key = "seventeen-byte-frames";
        let mut sequence = 1_u64;
        let full_frames = TOTAL_BYTES / FRAME_BYTES;
        for _ in 0..full_frames {
            let frame = TerminalReplayFrame::Output(vec![b'x'; FRAME_BYTES]);
            adapter.update_live_replay(key, &[], Some(&[(sequence, frame)]), 80, 24);
            sequence = sequence.saturating_add(1);
            let values = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::drain_diagnostics(&adapter)
                .into_iter()
                .map(|diagnostic| (diagnostic.name, diagnostic.value))
                .collect::<BTreeMap<_, _>>();
            assert_eq!(values.get("emulate_bytes"), Some(&17));
            assert_eq!(values.get("emulate_frames"), Some(&1));
        }
        let remainder = TOTAL_BYTES % FRAME_BYTES;
        adapter.update_live_replay(
            key,
            &[],
            Some(&[(sequence, TerminalReplayFrame::Output(vec![b'x'; remainder]))]),
            80,
            24,
        );
        let values = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::drain_diagnostics(&adapter)
            .into_iter()
            .map(|diagnostic| (diagnostic.name, diagnostic.value))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            values.get("emulate_bytes"),
            Some(&u64::try_from(remainder).expect("remainder"))
        );
        assert_eq!(values.get("emulate_frames"), Some(&1));
    }

    #[test]
    fn uninterrupted_reconnect_and_fresh_finalized_hydration_render_identically() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("reconnect-parity.bcsr");
        let mut writer =
            crate::recording::ShellRecordingWriter::create(&path, 16, 4).expect("recording writer");
        writer
            .write_output(1, b"\x1b[31mred\x1b[0m wide \xe7\x95\x8c\r\n")
            .expect("first output");
        writer.write_resize(2, 12, 3).expect("resize");
        writer
            .write_output(3, b"second\r\n\x1b[?1049halternate\x1b[?1049l")
            .expect("second output");
        writer
            .finish(4, Some(0), None, false, false)
            .expect("finish recording");
        let bytes = std::fs::read(path).expect("recording bytes");
        let first = bytes.len() / 3;
        let second = first.saturating_mul(2);

        let uninterrupted = ShellRunTuiVisualAdapter::default();
        deliver_recording_range(&uninterrupted, "call", &bytes, 0, first, false);
        deliver_recording_range(&uninterrupted, "call", &bytes, first, second, false);
        deliver_recording_range(&uninterrupted, "call", &bytes, second, bytes.len(), true);

        let reconnected = ShellRunTuiVisualAdapter::default();
        deliver_recording_range(&reconnected, "call", &bytes, 0, bytes.len(), true);

        let fresh_finalized = ShellRunTuiVisualAdapter::default();
        deliver_recording_range(&fresh_finalized, "call", &bytes, 0, bytes.len(), true);

        assert_eq!(
            render_hydrated_recording(&uninterrupted, "call"),
            render_hydrated_recording(&reconnected, "call")
        );
        assert_eq!(
            render_hydrated_recording(&uninterrupted, "call"),
            render_hydrated_recording(&fresh_finalized, "call")
        );
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
                    schema_version: SHELL_SCHEMA_VERSION,
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
                schema_version: SHELL_SCHEMA_VERSION,
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
            SHELL_RUN_SCHEMA,
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

    fn retained_snapshot(
        adapter: &ShellRunTuiVisualAdapter,
        key: &str,
    ) -> bmux_terminal_grid::GridSnapshot {
        let replays = adapter.live_replays.lock().expect("live replays");
        let retained = replays
            .get(key)
            .and_then(|replay| replay.stream.as_ref())
            .expect("retained terminal stream");
        let rows = retained
            .grid()
            .scrollback_rows_hint()
            .saturating_add(retained.grid().height());
        let snapshot = retained.snapshot(0, rows);
        drop(replays);
        snapshot
    }

    #[allow(clippy::too_many_lines)]
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
        let input = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::invocation_event_input(
            &adapter,
            key,
            "bcode.tool.request.shell.run",
            &payload,
            &bmux_tui::event::Event::Resize(bmux_tui::geometry::Size::new(9, 4)),
        )
        .expect("resize input");
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
        let live_frames = adapter
            .live_replays
            .lock()
            .expect("live replays")
            .get(key)
            .expect("live replay")
            .frames
            .clone();

        assert_eq!(input.producer_id, "bcode.shell");
        assert_eq!(live_frames, reopened_frames);
        let retained_snapshot = retained_snapshot(&adapter, key);
        let reopened =
            shell_terminal_stream(12, 3, &reopened_frames).expect("reopened terminal stream");
        let reopened_rows = reopened
            .grid()
            .scrollback_rows_hint()
            .saturating_add(reopened.grid().height());
        assert_eq!(retained_snapshot, reopened.snapshot(0, reopened_rows));
        let replays = adapter.live_replays.lock().expect("live replays");
        let retained = replays
            .get(key)
            .and_then(|replay| replay.stream.as_ref())
            .expect("retained terminal stream");
        assert_eq!(
            retained.grid().mode(),
            bmux_terminal_grid::GridMode::Alternate
        );
        assert!(!retained.grid().cursor().visible);
        drop(replays);

        let diagnostics =
            bcode_plugin_sdk::tui::PluginTuiVisualAdapter::drain_diagnostics(&adapter);
        let values = diagnostics
            .into_iter()
            .map(|diagnostic| (diagnostic.name, diagnostic.value))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            values.get("emulate_bytes"),
            Some(&u64::try_from(first.len() + second.len()).expect("emulated bytes"))
        );
        assert_eq!(values.get("emulate_frames"), Some(&3));
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
        let input = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::invocation_event_input(
            &adapter,
            "call-resize",
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

        assert!(input.is_some());
        assert_ne!(before, after);
        let rendered = after.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("5678\n    ABCD"), "{rendered}");
    }

    #[test]
    fn shell_visual_adapter_owns_resize_input_payload_and_identity() {
        let adapter = ShellRunTuiVisualAdapter::default();
        let payload = serde_json::json!({"live_state_key": "stale-renderer-key"});
        let input = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::invocation_event_input(
            &adapter,
            "shell-call",
            "bcode.tool.request.shell.run",
            &payload,
            &bmux_tui::event::Event::Resize(bmux_tui::geometry::Size::new(132, 40)),
        );
        assert_eq!(
            input,
            Some(bcode_tool::ToolInvocationInput {
                invocation_id: "shell-call".to_owned(),
                input_id: "shell-call-input-0".to_owned(),
                producer_id: "bcode.shell".to_owned(),
                schema: SHELL_INVOCATION_INPUT_SCHEMA.to_owned(),
                schema_version: SHELL_SCHEMA_VERSION,
                payload: serde_json::json!({
                    "type": "resize",
                    "columns": 132,
                    "rows": 40,
                }),
            })
        );
        let repeated = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::invocation_event_input(
            &adapter,
            "shell-call",
            "bcode.tool.request.shell.run",
            &payload,
            &bmux_tui::event::Event::Resize(bmux_tui::geometry::Size::new(132, 40)),
        )
        .expect("repeated resize input");
        assert_eq!(repeated.invocation_id, "shell-call");
        assert_eq!(repeated.input_id, "shell-call-input-1");
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
            SHELL_RUN_SCHEMA,
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
            SHELL_RUN_SCHEMA,
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
        let (_, frames) = crate::recording::read_recording(&path).expect("recording frames");
        let exact_bytes = frames
            .iter()
            .filter_map(|frame| match frame {
                crate::recording::ShellRecordingFrame::Output { bytes, .. } => {
                    Some(bytes.as_slice())
                }
                _ => None,
            })
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(
            exact_bytes,
            [
                b"valid ".as_slice(),
                &[0xe7],
                &[0x95, 0x8c],
                &[b' ', 0xff, b' ', b'e', 0xcc],
                &[0x81, b'\n'],
            ]
            .concat()
        );
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
                    "storage_uri": legacy_path.to_str().expect("UTF-8 legacy path"),
                    "metadata": {"stream": "pty"}
                },
                {
                    "key": SHELL_RECORDING_REF_KEY,
                    "content_type": "application/x-bcode-shell-recording; version=1",
                    "storage_uri": recording_path.to_str().expect("UTF-8 recording path"),
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
                    "storage_uri": legacy_path.to_str().expect("UTF-8 legacy path"),
                    "metadata": {"stream": "pty"}
                },
                {
                    "key": SHELL_RECORDING_REF_KEY,
                    "content_type": SHELL_RECORDING_CONTENT_TYPE,
                    "storage_uri": partial_path.to_str().expect("UTF-8 partial path"),
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
                "storage_uri": path.to_str().expect("UTF-8 legacy path"),
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
    fn live_final_state_matches_fresh_process_finalized_replay() {
        const CHILD_PATH_ENV: &str = "BCODE_TEST_FRESH_RECORDING_PATH";
        const EXPECTED_PATH_ENV: &str = "BCODE_TEST_FRESH_RECORDING_EXPECTED_PATH";
        if let (Some(path), Some(expected_path)) = (
            std::env::var_os(CHILD_PATH_ENV),
            std::env::var_os(EXPECTED_PATH_ENV),
        ) {
            let bytes = std::fs::read(path).expect("fresh process recording bytes");
            let adapter = ShellRunTuiVisualAdapter::default();
            deliver_recording_range(&adapter, "fresh-call", &bytes, 0, bytes.len(), true);
            let rendered = format!("{:?}", render_hydrated_recording(&adapter, "fresh-call"));
            let expected = std::fs::read_to_string(expected_path).expect("expected live rendering");
            assert_eq!(rendered, expected);
            return;
        }

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let path = temp_dir.path().join("recording.bcsr");
        let expected_path = temp_dir.path().join("expected-render.txt");
        let mut writer =
            crate::recording::ShellRecordingWriter::create(&path, 16, 4).expect("recording writer");
        writer
            .write_output(1, b"fresh \x1b[31mred\x1b[0m \xe7\x95\x8c\r\n")
            .expect("first output");
        writer.write_resize(2, 12, 3).expect("resize");
        writer
            .write_output(
                3,
                b"second\r\n\x1b[?1049halt\x1b[32mgreen\x1b[0m\x1b[?1049l",
            )
            .expect("second output");
        writer
            .finish(4, Some(0), None, false, false)
            .expect("finish recording");
        let bytes = std::fs::read(&path).expect("recording bytes");
        let split = bytes.len() / 2;
        let live = ShellRunTuiVisualAdapter::default();
        deliver_recording_range(&live, "fresh-call", &bytes, 0, split, false);
        deliver_recording_range(&live, "fresh-call", &bytes, split, bytes.len(), true);
        std::fs::write(
            &expected_path,
            format!("{:?}", render_hydrated_recording(&live, "fresh-call")),
        )
        .expect("expected live rendering");

        let status =
            std::process::Command::new(std::env::current_exe().expect("current test executable"))
                .arg("--exact")
                .arg(
                    "shell_run_tui::tests::live_final_state_matches_fresh_process_finalized_replay",
                )
                .arg("--nocapture")
                .env(CHILD_PATH_ENV, &path)
                .env(EXPECTED_PATH_ENV, &expected_path)
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
