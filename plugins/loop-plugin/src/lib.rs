#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Deterministic, manually steerable prompt loops for Bcode sessions.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use bcode_client::{BcodeClient, SessionWatchEvent};
use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandOwner,
    CommandSurface, InvokeCommandRequest, InvokeCommandResponse, OP_INVOKE_COMMAND,
};
use bcode_plugin_sdk::prelude::*;
use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiAction, PluginTuiHost, PluginTuiRegistry, PluginTuiSurface,
    PluginTuiSurfaceFactory, PluginTuiSurfaceFuture, PluginTuiSurfaceOpenRequest,
};
use bcode_session_models::{ModelTurnOutcome, SessionEventKind, SessionId};
use bmux_keyboard::KeyCode;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::event::{Event, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Color;
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing, ModalTheme};
use bmux_tui_components::text_input::{TextInputPolicy, TextInputState};
use bmux_tui_components::text_input_box::{TextInputBox, TextInputBoxOutcome, TextInputBoxPolicy};
use serde::{Deserialize, Serialize};

const PLUGIN_ID: &str = "bcode.loop";
const START_COMMAND: &str = "loop";
const STATUS_COMMAND: &str = "loop.status";
const STOP_COMMAND: &str = "loop.stop";
const RESUME_COMMAND: &str = "loop.resume";
const SURFACE_KIND: &str = "loop.start";
const DEFAULT_MAX_ITERATIONS: u64 = 20;
const HARD_MAX_ITERATIONS: u64 = 1_000;
const STATE_SCHEMA_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 1_048_576;
const MAX_PROMPT_BYTES: usize = 262_144;

#[derive(Default)]
struct LoopPlugin;

impl RustPlugin for LoopPlugin {
    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        for command in commands() {
            registrar
                .register(&command)
                .map_err(|error| PluginError::failed(error.to_string()))?;
        }
        Ok(())
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != COMMAND_INTERFACE_ID
            || context.request.operation != OP_INVOKE_COMMAND
        {
            return ServiceResponse::error("unsupported_operation", "unsupported loop operation");
        }
        let request = match context.request.payload_json::<InvokeCommandRequest>() {
            Ok(request) => request,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
        command_response(&request)
    }
}

fn commands() -> Vec<CommandContribution> {
    vec![
        command(
            START_COMMAND,
            "Loop",
            "Start a deterministic prompt loop",
            true,
        ),
        command(
            STATUS_COMMAND,
            "Loop Status",
            "Show prompt loop status",
            true,
        ),
        command(
            STOP_COMMAND,
            "Stop Loop",
            "Stop the active prompt loop",
            true,
        ),
        command(
            RESUME_COMMAND,
            "Resume Loop",
            "Resume a paused prompt loop",
            true,
        ),
    ]
}

fn command(id: &str, title: &str, description: &str, slash: bool) -> CommandContribution {
    let mut surfaces = BTreeSet::from([CommandSurface::Palette]);
    if slash {
        surfaces.insert(CommandSurface::Slash);
    }
    CommandContribution {
        id: id.to_owned(),
        title: title.to_owned(),
        description: Some(description.to_owned()),
        category: Some("automation".to_owned()),
        surfaces,
        owner: CommandOwner::Plugin {
            plugin_id: PLUGIN_ID.to_owned(),
        },
        action: CommandAction::Plugin {
            plugin_id: PLUGIN_ID.to_owned(),
            command_id: id.to_owned(),
        },
    }
}

fn status_for_session(session_id: SessionId) -> InvokeCommandResponse {
    match load_state_result(session_id) {
        Ok(state) => status_response(&format_status(state.as_ref())),
        Err(error) => status_response(&format!("loop state unavailable: {error}")),
    }
}

fn command_response(request: &InvokeCommandRequest) -> ServiceResponse {
    let session_id = request
        .args
        .get("session_id")
        .and_then(|value| SessionId::from_str(value).ok());
    let arguments = request.args.get("arguments").map_or("", String::as_str);
    let response = match request.command_id.as_str() {
        START_COMMAND if arguments == "status" => session_id.map_or_else(
            || status_response("/loop status requires an active session"),
            status_for_session,
        ),
        START_COMMAND if arguments == "stop" => session_id.map_or_else(
            || status_response("/loop stop requires an active session"),
            stop_loop,
        ),
        START_COMMAND if arguments == "resume" => session_id.map_or_else(
            || status_response("/loop resume requires an active session"),
            resume_loop,
        ),
        START_COMMAND if arguments.is_empty() => InvokeCommandResponse {
            success: true,
            message: None,
            updated_model: None,
            updated_provider: None,
            updated_thinking: None,
            effects: vec![CommandEffect::OpenPluginSurface {
                surface_kind: SURFACE_KIND.to_owned(),
                instance_id: "loop-start".to_owned(),
                options: serde_json::json!({}),
            }],
        },
        STATUS_COMMAND => session_id.map_or_else(
            || status_response("/loop status requires an active session"),
            status_for_session,
        ),
        STOP_COMMAND => session_id.map_or_else(
            || status_response("/loop stop requires an active session"),
            stop_loop,
        ),
        RESUME_COMMAND => session_id.map_or_else(
            || status_response("/loop resume requires an active session"),
            resume_loop,
        ),
        START_COMMAND => status_response("unknown /loop action; use status, stop, or resume"),
        _ => status_response("unsupported loop command"),
    };
    json_response(&response)
}

fn status_response(message: &str) -> InvokeCommandResponse {
    InvokeCommandResponse {
        success: true,
        message: Some(message.to_owned()),
        updated_model: None,
        updated_provider: None,
        updated_thinking: None,
        effects: vec![CommandEffect::AppendText {
            text: message.to_owned(),
        }],
    }
}

fn stop_loop(session_id: SessionId) -> InvokeCommandResponse {
    let mut state = match load_state_result(session_id) {
        Ok(Some(state)) => state,
        Ok(None) => return status_response("no loop found for this session"),
        Err(error) => return status_response(&format!("loop state unavailable: {error}")),
    };
    if state.state.is_terminal() {
        return status_response(&format_status(Some(&state)));
    }
    state.cancel_requested = true;
    state.state = RunState::Canceled;
    state.stop_reason = Some("stopped by user".to_owned());
    let cancel_session_id = state
        .pending_operation
        .as_ref()
        .map_or(session_id, |operation| operation.target_session_id);
    let message = match save_state(&state) {
        Ok(()) => {
            let client = BcodeClient::default_endpoint();
            tokio::spawn(async move {
                let _cancelled = client.cancel_session_turn(cancel_session_id).await;
            });
            "loop stopped".to_owned()
        }
        Err(error) => format!("failed to stop loop: {error}"),
    };
    status_response(&message)
}

fn resume_loop(session_id: SessionId) -> InvokeCommandResponse {
    let mut state = match load_state_result(session_id) {
        Ok(Some(state)) => state,
        Ok(None) => return status_response("no loop found for this session"),
        Err(error) => return status_response(&format!("loop state unavailable: {error}")),
    };
    if matches!(
        state.state,
        RunState::Completed | RunState::LimitReached | RunState::Canceled
    ) {
        return status_response("this loop is terminal and cannot be resumed");
    }
    if !matches!(state.state, RunState::Paused | RunState::Failed) {
        return status_response("only paused or failed loops can be resumed");
    }
    state.cancel_requested = false;
    state.state = RunState::Running;
    state.stop_reason = None;
    if let Err(error) = save_state(&state) {
        return status_response(&format!("failed to resume loop: {error}"));
    }
    tokio::spawn(run_loop(state));
    status_response("loop resumed")
}

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value)
        .unwrap_or_else(|error| ServiceResponse::error("encode_failed", error.to_string()))
}

#[must_use]
pub fn static_plugin() -> StaticPluginVtable {
    let mut vtable = static_plugin_vtable!(LoopPlugin, include_str!("../bcode-plugin.toml"));
    vtable.tui_registry = Some(tui_registry);
    vtable
}

fn tui_registry() -> PluginTuiRegistry {
    let mut registry = PluginTuiRegistry::default();
    registry.register_factory(Box::new(LoopSurfaceFactory));
    registry
}

struct LoopSurfaceFactory;

impl PluginTuiSurfaceFactory for LoopSurfaceFactory {
    fn surface_kind(&self) -> &'static str {
        SURFACE_KIND
    }

    fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        Box::pin(async move {
            let session_id = request
                .options
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| SessionId::from_str(value).ok());
            Ok(Box::new(LoopSurface::new(session_id)) as BoxedPluginTuiSurface)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Prompt,
    Condition,
    Limit,
}

impl Field {
    const fn next(self) -> Self {
        match self {
            Self::Prompt => Self::Condition,
            Self::Condition => Self::Limit,
            Self::Limit => Self::Prompt,
        }
    }

    const fn previous(self) -> Self {
        match self {
            Self::Prompt => Self::Limit,
            Self::Condition => Self::Prompt,
            Self::Limit => Self::Condition,
        }
    }
}

struct LoopSurface {
    session_id: Option<SessionId>,
    prompt: TextInputState,
    condition: TextInputState,
    limit: TextInputState,
    field: Field,
    status: String,
    prompt_area: Rect,
    condition_area: Rect,
    limit_area: Rect,
}

impl LoopSurface {
    fn new(session_id: Option<SessionId>) -> Self {
        Self {
            session_id,
            prompt: text_state(""),
            condition: text_state(""),
            limit: text_state(&DEFAULT_MAX_ITERATIONS.to_string()),
            field: Field::Prompt,
            status: "Tab changes field · Ctrl+Enter starts · Esc cancels".to_owned(),
            prompt_area: Rect::new(0, 0, 0, 0),
            condition_area: Rect::new(0, 0, 0, 0),
            limit_area: Rect::new(0, 0, 0, 0),
        }
    }

    const fn active_state_mut(&mut self) -> &mut TextInputState {
        match self.field {
            Field::Prompt => &mut self.prompt,
            Field::Condition => &mut self.condition,
            Field::Limit => &mut self.limit,
        }
    }

    const fn active_area(&self) -> Rect {
        match self.field {
            Field::Prompt => self.prompt_area,
            Field::Condition => self.condition_area,
            Field::Limit => self.limit_area,
        }
    }

    const fn focus_from_click(&mut self, event: &Event) {
        if event_click_in(event, self.prompt_area) {
            self.field = Field::Prompt;
        } else if event_click_in(event, self.condition_area) {
            self.field = Field::Condition;
        } else if event_click_in(event, self.limit_area) {
            self.field = Field::Limit;
        }
    }

    fn start(&mut self, host: &dyn PluginTuiHost) -> PluginTuiAction {
        let Some(session_id) = self.session_id else {
            "an active persisted session is required".clone_into(&mut self.status);
            return PluginTuiAction::Redraw;
        };
        let prompt = input_text(&self.prompt);
        let condition = input_text(&self.condition);
        let limit = input_text(&self.limit);
        let Ok(max_iterations) = limit.parse::<u64>() else {
            self.field = Field::Limit;
            "maximum iterations must be a number".clone_into(&mut self.status);
            return PluginTuiAction::Redraw;
        };
        if prompt.is_empty() {
            self.field = Field::Prompt;
            "iteration prompt is required".clone_into(&mut self.status);
            return PluginTuiAction::Redraw;
        }
        if condition.is_empty() {
            self.field = Field::Condition;
            "stop condition is required".clone_into(&mut self.status);
            return PluginTuiAction::Redraw;
        }
        if !(1..=HARD_MAX_ITERATIONS).contains(&max_iterations) {
            self.field = Field::Limit;
            self.status = format!("maximum iterations must be 1..={HARD_MAX_ITERATIONS}");
            return PluginTuiAction::Redraw;
        }
        match load_state_result(session_id) {
            Ok(Some(state)) if !state.state.is_terminal() => {
                "this session already has an active loop".clone_into(&mut self.status);
                return PluginTuiAction::Redraw;
            }
            Err(error) => {
                self.status = format!("existing loop state unavailable: {error}");
                return PluginTuiAction::Redraw;
            }
            Ok(Some(_) | None) => {}
        }
        let state = LoopState::new(session_id, prompt, condition, max_iterations);
        if let Err(error) = save_state(&state) {
            self.status = format!("failed to save loop: {error}");
            return PluginTuiAction::Redraw;
        }
        host.spawn(Box::pin(run_loop(state)));
        PluginTuiAction::Close {
            outcome: Some(serde_json::json!({
                "status": "loop started; normal messages will steer before the next iteration",
                "append_text": "Loop started. Normal messages remain available for steering. Use /loop status or /loop stop."
            })),
        }
    }

    fn render_input(
        area: Rect,
        frame: &mut Frame<'_>,
        label: &'static str,
        state: &mut TextInputState,
        focused: bool,
        rows: u16,
    ) {
        TextInputBox::new(TextInputPolicy::chat_composer())
            .label(label)
            .policy(TextInputBoxPolicy {
                field_chrome: true,
                panel_chrome: true,
                background: true,
                cursor: true,
                focused,
                disabled: false,
                min_rows: rows,
                max_rows: Some(rows),
            })
            .render(area, state, frame);
    }
}

impl PluginTuiSurface for LoopSurface {
    fn id(&self) -> &'static str {
        SURFACE_KIND
    }

    fn title(&self) -> &'static str {
        "Start Loop"
    }

    fn preferred_height(&mut self, _width: u16) -> u16 {
        24
    }

    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        let modal = ModalFrame::new(
            ModalSizing::new(Size::new(64, 22), Size::new(100, 32), Insets::all(2)),
            ModalTheme::dark(Color::Cyan),
        )
        .title(" Start deterministic loop ")
        .padding(Insets::new(1, 2, 1, 2))
        .placement(ModalPlacement::Centered);
        modal.render(area, frame);
        let content = modal.content_area(area);
        let available = content.height.saturating_sub(7);
        let prompt_rows = available.saturating_mul(3) / 5;
        let condition_rows = available.saturating_sub(prompt_rows).max(3);
        self.prompt_area = Rect::new(content.x, content.y, content.width, prompt_rows.max(4));
        self.condition_area = Rect::new(
            content.x,
            self.prompt_area.bottom().saturating_add(1),
            content.width,
            condition_rows,
        );
        self.limit_area = Rect::new(
            content.x,
            self.condition_area.bottom().saturating_add(1),
            content.width.min(36),
            3,
        );
        Self::render_input(
            self.prompt_area,
            frame,
            "Iteration prompt",
            &mut self.prompt,
            self.field == Field::Prompt,
            self.prompt_area.height.saturating_sub(2).max(2),
        );
        Self::render_input(
            self.condition_area,
            frame,
            "Stop condition",
            &mut self.condition,
            self.field == Field::Condition,
            self.condition_area.height.saturating_sub(2).max(2),
        );
        Self::render_input(
            self.limit_area,
            frame,
            "Maximum iterations",
            &mut self.limit,
            self.field == Field::Limit,
            1,
        );
        let status_y = self.limit_area.bottom().saturating_add(1);
        if status_y < content.bottom() {
            modal.render_line(
                Rect::new(content.x, status_y, content.width, 1),
                &Line::from_spans(vec![Span::styled(
                    self.status.clone(),
                    Style::new().fg(Color::BrightBlack).bg(Color::Black),
                )]),
                frame,
            );
        }
    }

    fn handle_event(&mut self, event: &Event, host: &dyn PluginTuiHost) -> PluginTuiAction {
        if let Event::Key(stroke) = event {
            if stroke.key == KeyCode::Escape && stroke.modifiers.is_empty() {
                return PluginTuiAction::Close { outcome: None };
            }
            if stroke.key == KeyCode::Tab && stroke.modifiers.shift {
                self.field = self.field.previous();
                return PluginTuiAction::Redraw;
            }
            if stroke.key == KeyCode::Tab && stroke.modifiers.is_empty() {
                self.field = self.field.next();
                return PluginTuiAction::Redraw;
            }
            if stroke.key == KeyCode::Enter && stroke.modifiers.ctrl {
                return self.start(host);
            }
            if stroke.key == KeyCode::Enter && self.field != Field::Limit {
                self.active_state_mut().buffer_mut().insert_char('\n');
                return PluginTuiAction::Redraw;
            }
            if stroke.key == KeyCode::Enter && self.field == Field::Limit {
                return self.start(host);
            }
            if self.field == Field::Limit
                && matches!(stroke.key, KeyCode::Char(value) if !value.is_ascii_digit())
            {
                return PluginTuiAction::None;
            }
        }
        if self.field == Field::Limit
            && matches!(event, Event::Paste(text) if !text.chars().all(|value| value.is_ascii_digit()))
        {
            return PluginTuiAction::None;
        }
        self.focus_from_click(event);
        let area = self.active_area();
        let state = self.active_state_mut();
        match TextInputBox::new(TextInputPolicy::chat_composer())
            .label("")
            .policy(TextInputBoxPolicy::labeled_field())
            .handle_event(area, state, event)
        {
            TextInputBoxOutcome::Edited | TextInputBoxOutcome::Redraw => PluginTuiAction::Redraw,
            TextInputBoxOutcome::Submitted
            | TextInputBoxOutcome::Ignored
            | TextInputBoxOutcome::EdgeUp
            | TextInputBoxOutcome::EdgeDown => PluginTuiAction::None,
        }
    }
}

fn text_state(value: &str) -> TextInputState {
    TextInputState::new(TextEditBuffer::from_text(value.to_owned()))
}

fn input_text(state: &TextInputState) -> String {
    state.buffer().text().trim().to_owned()
}

const fn event_click_in(event: &Event, area: Rect) -> bool {
    matches!(
        event,
        Event::Mouse(mouse)
            if matches!(mouse.kind, MouseEventKind::Down(_))
                && mouse.position.x >= area.x
                && mouse.position.x < area.right()
                && mouse.position.y >= area.y
                && mouse.position.y < area.bottom()
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RunState {
    Running,
    Steering,
    Evaluating,
    Completed,
    LimitReached,
    Paused,
    Canceled,
    Failed,
}

impl RunState {
    const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::LimitReached | Self::Paused | Self::Canceled | Self::Failed
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Evaluation {
    condition_met: bool,
    #[serde(default)]
    evidence: Vec<String>,
    summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OperationKind {
    Iteration { iteration: u64 },
    Evaluation { source_generation: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OperationStatus {
    Prepared,
    Accepted,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingOperation {
    operation_id: String,
    kind: OperationKind,
    target_session_id: SessionId,
    expected_generation: u64,
    status: OperationStatus,
    #[serde(default)]
    accepted_turn_id: Option<String>,
    #[serde(default)]
    accepted_sequence: Option<u64>,
    #[serde(default)]
    completion: Option<bcode_ipc::PluginAutomationTurnCompletion>,
}

const fn state_schema_version() -> u32 {
    STATE_SCHEMA_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LoopState {
    #[serde(default = "state_schema_version")]
    schema_version: u32,
    run_id: String,
    session_id: SessionId,
    iteration_prompt: String,
    stop_condition: String,
    max_iterations: u64,
    current_iteration: u64,
    state: RunState,
    #[serde(default)]
    latest_evaluation: Option<Evaluation>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    cancel_requested: bool,
    #[serde(default)]
    pending_operation: Option<PendingOperation>,
    #[serde(default)]
    last_completed_operation: Option<PendingOperation>,
}

impl LoopState {
    fn new(
        session_id: SessionId,
        iteration_prompt: String,
        stop_condition: String,
        max_iterations: u64,
    ) -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            run_id: uuid::Uuid::new_v4().to_string(),
            session_id,
            iteration_prompt,
            stop_condition,
            max_iterations,
            current_iteration: 0,
            state: RunState::Running,
            latest_evaluation: None,
            stop_reason: None,
            cancel_requested: false,
            pending_operation: None,
            last_completed_operation: None,
        }
    }
}

async fn reconcile_pending_operation(state: &mut LoopState) -> Result<(), String> {
    let Some(pending) = state.pending_operation.clone() else {
        return Ok(());
    };
    let operation = BcodeClient::default_endpoint()
        .lookup_plugin_automation_operation(bcode_ipc::PluginAutomationOperationLookupRequest {
            session_id: pending.target_session_id,
            plugin_id: PLUGIN_ID.to_owned(),
            operation_id: pending.operation_id.clone(),
        })
        .await
        .map_err(|error| error.to_string())?;
    match (pending.status, operation) {
        (OperationStatus::Prepared, None) => {
            state.pending_operation = None;
            save_state(state)
        }
        (_, None) => Err(format!(
            "automation operation {} is missing after it was recorded as accepted",
            pending.operation_id
        )),
        (_, Some(operation)) => {
            let Some(completion) = operation.completion else {
                return Err(format!(
                    "automation operation {} may still be in flight; explicit resume is required after it settles",
                    pending.operation_id
                ));
            };
            if completion.outcome != ModelTurnOutcome::Completed {
                return Err(format!(
                    "reconciled automation operation ended with {:?}",
                    completion.outcome
                ));
            }
            match pending.kind {
                OperationKind::Iteration { iteration } => {
                    state.current_iteration = state.current_iteration.max(iteration);
                    state.pending_operation = None;
                    save_state(state)
                }
                OperationKind::Evaluation { .. } => {
                    state.pending_operation = None;
                    state.state = RunState::Evaluating;
                    state.latest_evaluation = None;
                    save_state(state)
                }
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn run_loop(mut state: LoopState) {
    if let Err(reason) = reconcile_pending_operation(&mut state).await {
        pause_run(&mut state, reason);
        return;
    }
    loop {
        if refresh_cancel(&mut state) {
            return;
        }
        if state.state != RunState::Evaluating {
            if state.current_iteration >= state.max_iterations {
                state.state = RunState::LimitReached;
                state.stop_reason = Some("maximum iterations reached".to_owned());
                let _saved = save_state(&state);
                return;
            }
            let iteration_number = state.current_iteration.saturating_add(1);
            state.state = RunState::Running;
            let _saved = save_state(&state);
            let completion = loop {
                let generation = match wait_until_automation_ready(state.session_id).await {
                    Ok(generation) => generation,
                    Err(error) => {
                        fail_run(&mut state, error);
                        return;
                    }
                };
                let session_id = state.session_id;
                let operation_id = format!("{}:iteration:{iteration_number}", state.run_id);
                let iteration_prompt = state.iteration_prompt.clone();
                let result = run_automation_turn(
                    &mut state,
                    session_id,
                    OperationKind::Iteration {
                        iteration: iteration_number,
                    },
                    operation_id,
                    format!("Loop iteration {iteration_number}"),
                    iteration_prompt,
                    generation,
                    bcode_ipc::PluginAutomationExecutionPolicy::Normal,
                )
                .await;
                match result {
                    Ok(completion) => break completion,
                    Err(AutomationTurnError::Retry) => {}
                    Err(AutomationTurnError::Fatal(error)) => {
                        if refresh_cancel(&mut state) {
                            return;
                        }
                        fail_run(&mut state, error);
                        return;
                    }
                }
            };
            state.current_iteration = iteration_number;
            if completion.outcome != ModelTurnOutcome::Completed {
                if refresh_cancel(&mut state) {
                    return;
                }
                if completion.outcome == ModelTurnOutcome::Cancelled {
                    pause_for_steering(&mut state, "iteration cancelled".to_owned());
                    return;
                }
                pause_run(
                    &mut state,
                    format!("iteration ended with {:?}", completion.outcome),
                );
                return;
            }
            if refresh_cancel(&mut state) {
                return;
            }
        }
        let mut source_generation = match wait_until_automation_ready(state.session_id).await {
            Ok(generation) => generation,
            Err(error) => {
                fail_run(&mut state, error);
                return;
            }
        };
        loop {
            state.state = RunState::Evaluating;
            let _saved = save_state(&state);
            let operation_id = format!(
                "{}:evaluation:{}:{source_generation}",
                state.run_id, state.current_iteration
            );
            let stop_condition = state.stop_condition.clone();
            let evaluation = run_evaluation_turn(
                &mut state,
                OperationKind::Evaluation { source_generation },
                operation_id,
                evaluator_prompt(&stop_condition),
            )
            .await;
            let evaluation = match evaluation {
                Ok(completion) if completion.outcome == ModelTurnOutcome::Completed => {
                    match parse_evaluation(&completion.assistant_text) {
                        Ok(evaluation) => evaluation,
                        Err(error) => {
                            if refresh_cancel(&mut state) {
                                return;
                            }
                            pause_run(&mut state, error);
                            return;
                        }
                    }
                }
                Ok(completion) => {
                    if refresh_cancel(&mut state) {
                        return;
                    }
                    if completion.outcome == ModelTurnOutcome::Cancelled {
                        pause_for_steering(&mut state, "evaluation cancelled".to_owned());
                        return;
                    }
                    pause_run(
                        &mut state,
                        format!("evaluation ended with {:?}", completion.outcome),
                    );
                    return;
                }
                Err(error) => {
                    if refresh_cancel(&mut state) {
                        return;
                    }
                    pause_run(&mut state, error);
                    return;
                }
            };
            let current_generation = match wait_until_automation_ready(state.session_id).await {
                Ok(generation) => generation,
                Err(error) => {
                    fail_run(&mut state, error);
                    return;
                }
            };
            if current_generation != source_generation {
                state.state = RunState::Steering;
                state.latest_evaluation = None;
                let _saved = save_state(&state);
                source_generation = current_generation;
                continue;
            }
            state.latest_evaluation = Some(evaluation.clone());
            if evaluation.condition_met {
                state.state = RunState::Completed;
                state.stop_reason = Some(evaluation.summary);
                let _saved = save_state(&state);
                return;
            }
            let _saved = save_state(&state);
            break;
        }
    }
}

fn pause_for_steering(state: &mut LoopState, reason: String) {
    pause_run(state, reason);
    let state = state.clone();
    tokio::spawn(async move {
        let _resumed = resume_after_manual_steering(state).await;
    });
}

async fn resume_after_manual_steering(state: LoopState) -> Result<(), String> {
    let baseline_sequence = state
        .last_completed_operation
        .as_ref()
        .and_then(|operation| operation.completion.as_ref())
        .map_or(0, |completion| completion.event_sequence);
    let mut watcher = BcodeClient::default_endpoint()
        .watch_session(state.session_id, 64)
        .await
        .map_err(|error| error.to_string())?;
    let initial_has_manual = watcher.take_initial().is_some_and(|attached| {
        attached.history.iter().any(|event| {
            event.sequence > baseline_sequence
                && matches!(event.kind, SessionEventKind::UserMessage { .. })
        })
    });
    if initial_has_manual {
        return resume_after_steering_settles(state).await;
    }
    loop {
        let event = watcher
            .next_event()
            .await
            .map_err(|error| error.to_string())?;
        let SessionWatchEvent::Durable(event) = event else {
            continue;
        };
        if event.sequence <= baseline_sequence
            || !matches!(event.kind, SessionEventKind::UserMessage { .. })
        {
            continue;
        }
        return resume_after_steering_settles(state).await;
    }
}

async fn resume_after_steering_settles(state: LoopState) -> Result<(), String> {
    let Some(mut saved) = load_state_result(state.session_id)? else {
        return Ok(());
    };
    if saved.run_id != state.run_id || saved.cancel_requested || saved.state != RunState::Paused {
        return Ok(());
    }
    let _generation = wait_until_automation_ready(saved.session_id).await?;
    saved.state = RunState::Evaluating;
    saved.stop_reason = None;
    save_state(&saved)?;
    run_loop(saved).await;
    Ok(())
}

fn pause_run(state: &mut LoopState, reason: String) {
    state.state = RunState::Paused;
    state.stop_reason = Some(reason);
    let _saved = save_state(state);
}

fn fail_run(state: &mut LoopState, reason: String) {
    state.state = RunState::Failed;
    state.stop_reason = Some(reason);
    let _saved = save_state(state);
}

struct TurnCompletion {
    outcome: ModelTurnOutcome,
    assistant_text: String,
}

async fn wait_until_automation_ready(session_id: SessionId) -> Result<u64, String> {
    let client = BcodeClient::default_endpoint();
    let mut watcher = client
        .watch_session(session_id, 1)
        .await
        .map_err(|error| error.to_string())?;
    let _initial = watcher.take_initial();
    loop {
        let snapshot = client
            .plugin_automation_snapshot(session_id)
            .await
            .map_err(|error| error.to_string())?;
        if (!snapshot.session_busy || snapshot.plugin_automation_active)
            && snapshot.pending_manual_messages == 0
            && !snapshot.automation_held
        {
            return Ok(snapshot.generation);
        }
        let _event = watcher
            .next_event()
            .await
            .map_err(|error| error.to_string())?;
    }
}

enum AutomationTurnError {
    Retry,
    Fatal(String),
}

async fn run_evaluation_turn(
    state: &mut LoopState,
    kind: OperationKind,
    operation_id: String,
    prompt: String,
) -> Result<TurnCompletion, String> {
    let source_session_id = state.session_id;
    let client = BcodeClient::default_endpoint();
    let clone = client
        .clone_session(
            source_session_id,
            Some(format!("loop-evaluator-{}", uuid::Uuid::new_v4())),
        )
        .await
        .map_err(|error| format!("failed to create evaluator session: {error}"))?;
    let evaluator_session_id = clone.session.id;
    let generation = wait_until_automation_ready(evaluator_session_id).await?;
    let result = run_automation_turn(
        state,
        evaluator_session_id,
        kind,
        operation_id,
        "Loop evaluator".to_owned(),
        prompt,
        generation,
        bcode_ipc::PluginAutomationExecutionPolicy::ReadOnlyInspection,
    )
    .await;
    let cleanup = client.delete_session(evaluator_session_id).await;
    match (result, cleanup) {
        (Ok(completion), Ok(_deleted)) => Ok(completion),
        (Ok(_completion), Err(error)) => {
            Err(format!("failed to remove evaluator session: {error}"))
        }
        (Err(AutomationTurnError::Fatal(error)), _) => Err(error),
        (Err(AutomationTurnError::Retry), _) => {
            Err("evaluator automation was preempted before submission".to_owned())
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_automation_turn(
    state: &mut LoopState,
    session_id: SessionId,
    kind: OperationKind,
    operation_id: String,
    display_label: String,
    text: String,
    expected_generation: u64,
    execution_policy: bcode_ipc::PluginAutomationExecutionPolicy,
) -> Result<TurnCompletion, AutomationTurnError> {
    let client = BcodeClient::default_endpoint();
    let mut watcher = client
        .watch_session(session_id, 32)
        .await
        .map_err(|error| AutomationTurnError::Fatal(error.to_string()))?;
    let initial = watcher.take_initial();
    state.pending_operation = Some(PendingOperation {
        operation_id: operation_id.clone(),
        kind,
        target_session_id: session_id,
        expected_generation,
        status: OperationStatus::Prepared,
        accepted_turn_id: None,
        accepted_sequence: None,
        completion: None,
    });
    save_state(state).map_err(AutomationTurnError::Fatal)?;
    let result = client
        .submit_plugin_automation_turn(bcode_ipc::PluginAutomationTurnRequest {
            session_id,
            origin: bcode_ipc::PluginAutomationOrigin {
                plugin_id: PLUGIN_ID.to_owned(),
                run_id: state.run_id.clone(),
                operation_id: operation_id.clone(),
                display_label,
            },
            text,
            expected_generation,
            execution_policy,
        })
        .await
        .map_err(|error| AutomationTurnError::Fatal(error.to_string()))?;
    let operation = match result {
        bcode_ipc::PluginAutomationTurnDisposition::Accepted { operation }
        | bcode_ipc::PluginAutomationTurnDisposition::AlreadyAccepted { operation } => operation,
        bcode_ipc::PluginAutomationTurnDisposition::SessionChanged { .. }
        | bcode_ipc::PluginAutomationTurnDisposition::ManualInputPending { .. }
        | bcode_ipc::PluginAutomationTurnDisposition::SessionBusy
        | bcode_ipc::PluginAutomationTurnDisposition::AutomationHeld => {
            state.pending_operation = None;
            save_state(state).map_err(AutomationTurnError::Fatal)?;
            return Err(AutomationTurnError::Retry);
        }
    };
    if let Some(pending) = state.pending_operation.as_mut() {
        pending.status = OperationStatus::Accepted;
        pending.accepted_turn_id = Some(operation.turn_id.clone());
        pending.accepted_sequence = Some(operation.user_event_sequence);
        pending.completion.clone_from(&operation.completion);
    }
    save_state(state).map_err(AutomationTurnError::Fatal)?;
    let mut assistant_text = initial
        .as_ref()
        .and_then(|attached| {
            assistant_text_for_operation(
                &attached.history,
                operation.user_event_sequence,
                operation
                    .completion
                    .as_ref()
                    .map(|value| value.event_sequence),
            )
        })
        .unwrap_or_default();
    if let Some(completion) = operation.completion {
        complete_pending_operation(state, completion.clone())?;
        return Ok(TurnCompletion {
            outcome: completion.outcome,
            assistant_text,
        });
    }
    loop {
        if let SessionWatchEvent::Durable(event) = watcher
            .next_event()
            .await
            .map_err(|error| AutomationTurnError::Fatal(error.to_string()))?
        {
            match &event.kind {
                SessionEventKind::AssistantMessage { text } => assistant_text.clone_from(text),
                SessionEventKind::PluginAutomationTurnFinished {
                    plugin_id,
                    operation_id: event_operation_id,
                    outcome,
                    ..
                } if plugin_id == PLUGIN_ID && event_operation_id == &operation_id => {
                    let completion = bcode_ipc::PluginAutomationTurnCompletion {
                        outcome: *outcome,
                        message: match &event.kind {
                            SessionEventKind::PluginAutomationTurnFinished { message, .. } => {
                                message.clone()
                            }
                            _ => None,
                        },
                        event_sequence: event.sequence,
                    };
                    complete_pending_operation(state, completion)?;
                    return Ok(TurnCompletion {
                        outcome: *outcome,
                        assistant_text,
                    });
                }
                _ => {}
            }
        }
    }
}

fn complete_pending_operation(
    state: &mut LoopState,
    completion: bcode_ipc::PluginAutomationTurnCompletion,
) -> Result<(), AutomationTurnError> {
    if let Some(pending) = state.pending_operation.as_mut() {
        pending.status = OperationStatus::Completed;
        pending.completion = Some(completion);
    }
    save_state(state).map_err(AutomationTurnError::Fatal)?;
    state.last_completed_operation = state.pending_operation.take();
    save_state(state).map_err(AutomationTurnError::Fatal)
}

fn assistant_text_for_operation(
    events: &[bcode_session_models::SessionEvent],
    start_sequence: u64,
    end_sequence: Option<u64>,
) -> Option<String> {
    events
        .iter()
        .filter(|event| {
            event.sequence > start_sequence
                && end_sequence.is_none_or(|end_sequence| event.sequence < end_sequence)
        })
        .filter_map(|event| match &event.kind {
            SessionEventKind::AssistantMessage { text } => Some(text.clone()),
            _ => None,
        })
        .next_back()
}

fn evaluator_prompt(condition: &str) -> String {
    format!(
        "Read-only loop completion evaluation. Inspect the current repository and conversation state. Do not modify files, implement work, or invoke mutating tools. Evaluate this stop condition:\n\n{condition}\n\nReturn ONLY one JSON object with exactly this shape: {{\"condition_met\":false,\"evidence\":[\"concrete observation\"],\"summary\":\"concise result\"}}. Set condition_met to true only when the condition is positively and completely verified. Ambiguity, missing evidence, unchecked work, tool failure, or inability to inspect means false."
    )
}

fn parse_evaluation(text: &str) -> Result<Evaluation, String> {
    let trimmed = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let evaluation: Evaluation = serde_json::from_str(trimmed)
        .map_err(|error| format!("invalid loop evaluation JSON: {error}"))?;
    if evaluation.summary.trim().is_empty() || evaluation.evidence.is_empty() {
        return Err("loop evaluation omitted its summary or evidence".to_owned());
    }
    Ok(evaluation)
}

fn refresh_cancel(state: &mut LoopState) -> bool {
    let cancelled = load_state(state.session_id)
        .is_some_and(|saved| saved.run_id == state.run_id && saved.cancel_requested);
    if cancelled {
        state.cancel_requested = true;
        state.state = RunState::Canceled;
        state.stop_reason = Some("stopped by user".to_owned());
        let _saved = save_state(state);
    }
    cancelled
}

fn format_status(state: Option<&LoopState>) -> String {
    state.map_or_else(
        || "no loop found for this session".to_owned(),
        |state| {
            let reason = state.stop_reason.as_deref().unwrap_or("none");
            format!(
                "loop {} · {:?} · iteration {}/{} · reason: {reason}",
                state.run_id, state.state, state.current_iteration, state.max_iterations
            )
        },
    )
}

fn state_path(session_id: SessionId) -> PathBuf {
    state_root().join(format!("{session_id}.json"))
}

fn state_root() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME").map_or_else(
        || {
            std::env::var_os("HOME").map_or_else(
                || PathBuf::from(".bcode-loop"),
                |home| PathBuf::from(home).join(".local/state/bcode/loop"),
            )
        },
        |root| PathBuf::from(root).join("bcode/loop"),
    )
}

fn validate_state(state: &LoopState) -> Result<(), String> {
    if state.schema_version != STATE_SCHEMA_VERSION {
        return Err(format!(
            "unsupported loop state schema version {}; expected {STATE_SCHEMA_VERSION}",
            state.schema_version
        ));
    }
    if state.iteration_prompt.len() > MAX_PROMPT_BYTES
        || state.stop_condition.len() > MAX_PROMPT_BYTES
    {
        return Err("loop prompt or stop condition exceeds the persisted size limit".to_owned());
    }
    if !(1..=HARD_MAX_ITERATIONS).contains(&state.max_iterations) {
        return Err("persisted loop maximum iterations is invalid".to_owned());
    }
    if state.current_iteration > state.max_iterations {
        return Err("persisted loop iteration count exceeds its maximum".to_owned());
    }
    Ok(())
}

fn load_state_result(session_id: SessionId) -> Result<Option<LoopState>, String> {
    let path = state_path(session_id);
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if metadata.len() > MAX_STATE_BYTES {
        return Err(format!(
            "loop state exceeds the {MAX_STATE_BYTES}-byte safety limit"
        ));
    }
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    let state: LoopState =
        serde_json::from_slice(&bytes).map_err(|error| format!("corrupt loop state: {error}"))?;
    validate_state(&state)?;
    Ok(Some(state))
}

fn load_state(session_id: SessionId) -> Option<LoopState> {
    load_state_result(session_id).ok().flatten()
}

fn save_state(state: &LoopState) -> Result<(), String> {
    validate_state(state)?;
    let root = state_root();
    fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    let path = state_path(state.session_id);
    let temporary = path.with_extension(format!("{}.tmp", state.run_id));
    let bytes = serde_json::to_vec_pretty(state).map_err(|error| error.to_string())?;
    fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    fs::rename(temporary, path).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct TestHost;

    impl PluginTuiHost for TestHost {
        fn spawn(&self, _task: bcode_plugin_sdk::tui::PluginTask) {}
        fn spawn_blocking(&self, _task: Box<dyn FnOnce() + Send + 'static>) {}
        fn request_redraw(&self) {}
    }

    fn key(key: KeyCode) -> Event {
        Event::Key(bmux_keyboard::KeyStroke {
            key,
            modifiers: bmux_keyboard::Modifiers::NONE,
        })
    }

    fn modified_key(key: KeyCode, ctrl: bool, shift: bool) -> Event {
        Event::Key(bmux_keyboard::KeyStroke {
            key,
            modifiers: bmux_keyboard::Modifiers {
                ctrl,
                shift,
                ..bmux_keyboard::Modifiers::NONE
            },
        })
    }

    #[test]
    fn ctrl_enter_starts_instead_of_inserting_newline() {
        let host = TestHost;
        let mut surface = LoopSurface::new(None);
        surface.prompt = text_state("work");
        surface.condition = text_state("done");
        let before = surface.prompt.buffer().text().to_owned();
        assert_eq!(
            surface.handle_event(&modified_key(KeyCode::Enter, true, false), &host),
            PluginTuiAction::Redraw
        );
        assert_eq!(surface.prompt.buffer().text(), before);
        assert!(surface.status.contains("active persisted session"));
    }

    #[test]
    fn shift_tab_traverses_fields_backward() {
        let host = TestHost;
        let mut surface = LoopSurface::new(Some(SessionId::new()));
        assert_eq!(
            surface.handle_event(&modified_key(KeyCode::Tab, false, true), &host),
            PluginTuiAction::Redraw
        );
        assert_eq!(surface.field, Field::Limit);
    }

    #[test]
    fn modal_multiline_and_field_navigation_preserve_text() {
        let host = TestHost;
        let mut surface = LoopSurface::new(Some(SessionId::new()));
        assert_eq!(surface.field, Field::Prompt);
        assert_eq!(
            surface.handle_event(&key(KeyCode::Char('a')), &host),
            PluginTuiAction::Redraw
        );
        assert_eq!(
            surface.handle_event(&key(KeyCode::Enter), &host),
            PluginTuiAction::Redraw
        );
        assert_eq!(
            surface.handle_event(&Event::Paste("line two".to_owned()), &host),
            PluginTuiAction::Redraw
        );
        assert_eq!(surface.prompt.buffer().text(), "a\nline two");

        assert_eq!(
            surface.handle_event(&key(KeyCode::Tab), &host),
            PluginTuiAction::Redraw
        );
        assert_eq!(surface.field, Field::Condition);
    }

    #[test]
    fn modal_validation_focuses_first_invalid_field() {
        let host = TestHost;
        let mut surface = LoopSurface::new(Some(SessionId::new()));
        assert_eq!(surface.start(&host), PluginTuiAction::Redraw);
        assert_eq!(surface.field, Field::Prompt);

        surface.prompt = text_state("do work");
        assert_eq!(surface.start(&host), PluginTuiAction::Redraw);
        assert_eq!(surface.field, Field::Condition);

        surface.condition = text_state("done");
        surface.limit = text_state("invalid");
        assert_eq!(surface.start(&host), PluginTuiAction::Redraw);
        assert_eq!(surface.field, Field::Limit);
    }

    #[test]
    fn numeric_field_rejects_non_digit_input() {
        let host = TestHost;
        let mut surface = LoopSurface::new(Some(SessionId::new()));
        surface.field = Field::Limit;
        let before = surface.limit.buffer().text().to_owned();
        assert_eq!(
            surface.handle_event(&key(KeyCode::Char('x')), &host),
            PluginTuiAction::None
        );
        assert_eq!(surface.limit.buffer().text(), before);
    }

    #[test]
    fn terminal_loop_states_cannot_be_resumed() {
        for terminal in [
            RunState::Completed,
            RunState::LimitReached,
            RunState::Canceled,
        ] {
            assert!(terminal.is_terminal());
            assert!(!matches!(terminal, RunState::Paused | RunState::Failed));
        }
    }

    #[test]
    fn loop_state_round_trip_preserves_pending_operation_journal() {
        let mut state = LoopState::new(
            SessionId::new(),
            "iterate".to_owned(),
            "complete".to_owned(),
            20,
        );
        state.pending_operation = Some(PendingOperation {
            operation_id: "operation-1".to_owned(),
            kind: OperationKind::Iteration { iteration: 1 },
            target_session_id: state.session_id,
            expected_generation: 7,
            status: OperationStatus::Accepted,
            accepted_turn_id: Some("turn-1".to_owned()),
            accepted_sequence: Some(8),
            completion: None,
        });

        let encoded = serde_json::to_vec(&state).expect("encode state");
        let decoded: LoopState = serde_json::from_slice(&encoded).expect("decode state");
        validate_state(&decoded).expect("valid state");
        let pending = decoded.pending_operation.expect("pending operation");
        assert_eq!(pending.operation_id, "operation-1");
        assert_eq!(pending.status, OperationStatus::Accepted);
        assert_eq!(pending.accepted_sequence, Some(8));
    }

    #[test]
    fn state_validation_rejects_incompatible_and_invalid_state() {
        let mut state = LoopState::new(
            SessionId::new(),
            "iterate".to_owned(),
            "complete".to_owned(),
            20,
        );
        state.schema_version = STATE_SCHEMA_VERSION.saturating_add(1);
        assert!(validate_state(&state).is_err());
        state.schema_version = STATE_SCHEMA_VERSION;
        state.current_iteration = 21;
        assert!(validate_state(&state).is_err());
    }

    #[test]
    fn evaluation_requires_valid_json_and_evidence() {
        let evaluation = parse_evaluation(
            r#"{"condition_met":false,"evidence":["one unchecked item"],"summary":"not done"}"#,
        )
        .expect("valid evaluation");
        assert!(!evaluation.condition_met);
        assert_eq!(evaluation.summary, "not done");

        assert!(
            parse_evaluation(r#"{"condition_met":true,"evidence":[],"summary":"done"}"#).is_err()
        );
        assert!(parse_evaluation("not json").is_err());
    }

    #[test]
    fn evaluator_prompt_is_conservative_and_read_only() {
        let prompt = evaluator_prompt("all checkboxes complete");
        assert!(prompt.contains("Read-only"));
        assert!(prompt.contains("positively and completely verified"));
        assert!(prompt.contains("all checkboxes complete"));
    }

    #[test]
    fn loop_command_is_available_on_slash_and_palette_surfaces() {
        let loop_command = commands()
            .into_iter()
            .find(|command| command.id == START_COMMAND)
            .expect("loop command");
        assert!(loop_command.supports_surface(&CommandSurface::Slash));
        assert!(loop_command.supports_surface(&CommandSurface::Palette));
    }
}
