#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Workflow-native deterministic prompt loops for Bcode sessions.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use bcode_client::{BcodeClient, ClientError};
use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandOwner,
    CommandSurface, InvokeCommandRequest, InvokeCommandResponse, OP_INVOKE_COMMAND,
    SlashCommandContribution,
};
use bcode_plugin_sdk::prelude::*;
use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiAction, PluginTuiHost, PluginTuiRegistry, PluginTuiSurface,
    PluginTuiSurfaceFactory, PluginTuiSurfaceFuture, PluginTuiSurfaceOpenRequest, PluginTuiTheme,
    PluginWorkflowBinding, PluginWorkflowStartRequest, PluginWorkflowStatus,
};
use bcode_session_models::SessionId;
use bmux_keyboard::KeyCode;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::event::{Event, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::prelude::Style;
use bmux_tui::style::Color;
use bmux_tui_components::key_hint_bar::{KeyHint, KeyHintBar, KeyHintBarStyles};
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing, ModalTheme};
use bmux_tui_components::status_bar::{StatusBar, StatusBarStyles, StatusSegment, StatusSeverity};
use bmux_tui_components::text_input::{TextInputPolicy, TextInputState};
use bmux_tui_components::text_input_box::{
    TextInputBox, TextInputBoxOutcome, TextInputBoxPolicy, TextInputBoxStyles,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const PLUGIN_ID: &str = "bcode.loop";
const WORKFLOW_KIND: &str = "bcode.loop";
const START_COMMAND: &str = "loop";
const STATUS_COMMAND: &str = "loop.status";
const PAUSE_COMMAND: &str = "loop.pause";
const STOP_COMMAND: &str = "loop.stop";
const RESUME_COMMAND: &str = "loop.resume";
const SURFACE_KIND: &str = "loop.start";
const DEFAULT_MAX_ITERATIONS: u64 = 20;
const HARD_MAX_ITERATIONS: u64 = 1_000;
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
        if context.request.interface_id == bcode_plugin_sdk::SESSION_STATUS_INTERFACE_ID
            && context.request.operation == bcode_plugin_sdk::OP_SESSION_STATUS
        {
            let request = match context
                .request
                .payload_json::<bcode_plugin_sdk::SessionStatusRequest>()
            {
                Ok(request) => request,
                Err(error) => {
                    return ServiceResponse::error("invalid_request", error.to_string());
                }
            };
            return json_response(&session_status_response(request.session_id));
        }
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
        command(START_COMMAND, "Loop", "Start a deterministic prompt loop"),
        command(STATUS_COMMAND, "Loop Status", "Show prompt loop status"),
        command(PAUSE_COMMAND, "Pause Loop", "Pause the active prompt loop"),
        command(STOP_COMMAND, "Stop Loop", "Stop the active prompt loop"),
        command(RESUME_COMMAND, "Resume Loop", "Resume a paused prompt loop"),
    ]
}

fn command(id: &str, title: &str, description: &str) -> CommandContribution {
    CommandContribution {
        id: id.to_owned(),
        title: title.to_owned(),
        description: Some(description.to_owned()),
        category: Some("automation".to_owned()),
        surfaces: BTreeSet::from([CommandSurface::Palette, CommandSurface::Slash]),
        slash: Some(SlashCommandContribution {
            name: id.to_owned(),
            aliases: BTreeSet::new(),
        }),
        arguments: Vec::new(),
        session: bcode_command::CommandSessionRequirement::Optional,
        execution: bcode_command::CommandExecution::Immediate,
        owner: CommandOwner::Plugin {
            plugin_id: PLUGIN_ID.to_owned(),
        },
        action: CommandAction::Plugin {
            plugin_id: PLUGIN_ID.to_owned(),
            command_id: id.to_owned(),
        },
    }
}

fn workflow_binding_key(session_id: SessionId) -> bcode_ipc::WorkflowRunBindingLookup {
    bcode_ipc::WorkflowRunBindingLookup {
        owner_plugin_id: PLUGIN_ID.to_string(),
        workflow_kind: WORKFLOW_KIND.to_string(),
        scope_key: session_id.to_string(),
    }
}

#[derive(Debug)]
enum LoopIpcError {
    Client(ClientError),
    Worker(String),
}

impl std::fmt::Display for LoopIpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(error) => error.fmt(formatter),
            Self::Worker(error) => formatter.write_str(error),
        }
    }
}

fn associated_workflow_run(
    session_id: SessionId,
) -> Result<Option<bcode_workflow_store::WorkflowRunSummary>, LoopIpcError> {
    run_async(async move {
        BcodeClient::default_endpoint()
            .associated_workflow_run(workflow_binding_key(session_id))
            .await
    })
}

fn associated_workflow_inspection(
    session_id: SessionId,
) -> Result<Option<bcode_ipc::WorkflowRunInspection>, LoopIpcError> {
    run_async(async move {
        BcodeClient::default_endpoint()
            .inspect_associated_workflow_run(workflow_binding_key(session_id), 100)
            .await
    })
}

fn control_associated_workflow_run(
    session_id: SessionId,
    action: bcode_ipc::WorkflowRunControlAction,
) -> Result<(Option<bcode_workflow_store::WorkflowRunSummary>, bool), LoopIpcError> {
    run_async(async move {
        BcodeClient::default_endpoint()
            .control_associated_workflow_run(workflow_binding_key(session_id), action)
            .await
    })
}

fn terminal_workflow_control_message(
    run: &bcode_workflow_store::WorkflowRunSummary,
) -> Option<String> {
    matches!(
        run.status,
        bcode_workflow_store::RunStatus::Completed
            | bcode_workflow_store::RunStatus::Failed
            | bcode_workflow_store::RunStatus::Cancelled
    )
    .then(|| {
        format!(
            "loop workflow is already {}; start a new loop to run it again",
            format!("{:?}", run.status).to_ascii_lowercase()
        )
    })
}

fn format_workflow_status(run: &bcode_workflow_store::WorkflowRunSummary) -> String {
    if run.status == bcode_workflow_store::RunStatus::RepairRequired {
        return format!(
            "loop workflow {} · repair required because recovery could not prove the previous operation outcome · definition {} v{}",
            run.run_id, run.definition_id, run.definition_version
        );
    }
    let status = if run.cancellation_requested_at_ms.is_some()
        && matches!(
            run.status,
            bcode_workflow_store::RunStatus::Running | bcode_workflow_store::RunStatus::Paused
        ) {
        "Cancelling".to_owned()
    } else {
        format!("{:?}", run.status)
    };
    format!(
        "loop workflow {} · status {status} · definition {} v{}",
        run.run_id, run.definition_id, run.definition_version
    )
}

fn format_workflow_inspection_status(inspection: &bcode_ipc::WorkflowRunInspection) -> String {
    let mut status = format_workflow_status(&inspection.run);
    if !inspection.mutation_approvals.is_empty() {
        let _ = write!(
            status,
            " · {} mutation approval(s) waiting",
            inspection.mutation_approvals.len()
        );
    }
    status
}

fn format_plugin_workflow_status(run: &bcode_plugin_sdk::tui::PluginWorkflowSummary) -> String {
    if run.status == bcode_plugin_sdk::tui::PluginWorkflowStatus::RepairRequired {
        return format!(
            "loop workflow {} · repair required because recovery could not prove the previous operation outcome · definition {} v{}",
            run.run_id, run.definition_id, run.definition_version
        );
    }
    let status = if run.cancellation_requested
        && matches!(
            run.status,
            bcode_plugin_sdk::tui::PluginWorkflowStatus::Running
                | bcode_plugin_sdk::tui::PluginWorkflowStatus::Paused
        ) {
        "Cancelling".to_owned()
    } else {
        format!("{:?}", run.status)
    };
    format!(
        "loop workflow {} · status {status} · definition {} v{}",
        run.run_id, run.definition_id, run.definition_version
    )
}

const fn unsupported_legacy_message() -> &'static str {
    "legacy loop state is unsupported by this daemon; use the older daemon that created it"
}

fn status_for_session(session_id: SessionId) -> InvokeCommandResponse {
    match associated_workflow_inspection(session_id) {
        Ok(Some(inspection)) => status_response(&format_workflow_inspection_status(&inspection)),
        Ok(None) if legacy_state_exists(session_id) => {
            status_response(unsupported_legacy_message())
        }
        Ok(None) => status_response("no loop found for this session"),
        Err(error) => status_response(&format!("workflow status unavailable: {error}")),
    }
}

fn control_loop(
    session_id: SessionId,
    action: bcode_ipc::WorkflowRunControlAction,
) -> InvokeCommandResponse {
    let verb = match action {
        bcode_ipc::WorkflowRunControlAction::Pause => "paused",
        bcode_ipc::WorkflowRunControlAction::Resume => "resumed",
        bcode_ipc::WorkflowRunControlAction::Cancel => "cancellation requested",
    };
    match control_associated_workflow_run(session_id, action) {
        Ok((Some(run), true)) => status_response(&format!("loop workflow {} {verb}", run.run_id)),
        Ok((Some(run), false)) => status_response(&format_workflow_status(&run)),
        Ok((None, _)) if legacy_state_exists(session_id) => {
            status_response(unsupported_legacy_message())
        }
        Ok((None, _)) => status_response("no loop found for this session"),
        Err(LoopIpcError::Client(ClientError::Server { code, .. }))
            if code == "workflow_invalid_transition" =>
        {
            match associated_workflow_run(session_id) {
                Ok(Some(run)) => terminal_workflow_control_message(&run).map_or_else(
                    || status_response(&format_workflow_status(&run)),
                    |message| status_response(&message),
                ),
                Ok(None) => status_response("no loop found for this session"),
                Err(error) => status_response(&format!("workflow lifecycle unavailable: {error}")),
            }
        }
        Err(LoopIpcError::Client(ClientError::Server { message, .. })) => status_response(&message),
        Err(error) => status_response(&format!("workflow lifecycle unavailable: {error}")),
    }
}

fn session_status_response(session_id: SessionId) -> bcode_plugin_sdk::SessionStatusResponse {
    let contribution = match associated_workflow_run(session_id) {
        Ok(run) => run,
        Err(LoopIpcError::Client(ClientError::Server { code, .. }))
            if code == "workflow_capability_unavailable" =>
        {
            None
        }
        Err(_) => None,
    }
    .filter(|run| {
        matches!(
            run.status,
            bcode_workflow_store::RunStatus::Running
                | bcode_workflow_store::RunStatus::Paused
                | bcode_workflow_store::RunStatus::RepairRequired
        )
    })
    .map(|run| bcode_plugin_sdk::SessionStatusContribution {
        contribution_id: "active-loop".to_owned(),
        text: format_workflow_status(&run),
        priority: 20,
        metadata: std::collections::BTreeMap::from([
            ("run_id".to_owned(), serde_json::json!(run.run_id)),
            (
                "status".to_owned(),
                serde_json::json!(format!("{:?}", run.status)),
            ),
        ]),
    });
    bcode_plugin_sdk::SessionStatusResponse { contribution }
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
        START_COMMAND if arguments == "pause" => session_id.map_or_else(
            || status_response("/loop pause requires an active session"),
            |session_id| control_loop(session_id, bcode_ipc::WorkflowRunControlAction::Pause),
        ),
        START_COMMAND if arguments == "stop" => session_id.map_or_else(
            || status_response("/loop stop requires an active session"),
            |session_id| control_loop(session_id, bcode_ipc::WorkflowRunControlAction::Cancel),
        ),
        START_COMMAND if arguments == "resume" => session_id.map_or_else(
            || status_response("/loop resume requires an active session"),
            |session_id| control_loop(session_id, bcode_ipc::WorkflowRunControlAction::Resume),
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
        PAUSE_COMMAND => session_id.map_or_else(
            || status_response("/loop pause requires an active session"),
            |session_id| control_loop(session_id, bcode_ipc::WorkflowRunControlAction::Pause),
        ),
        STOP_COMMAND => session_id.map_or_else(
            || status_response("/loop stop requires an active session"),
            |session_id| control_loop(session_id, bcode_ipc::WorkflowRunControlAction::Cancel),
        ),
        RESUME_COMMAND => session_id.map_or_else(
            || status_response("/loop resume requires an active session"),
            |session_id| control_loop(session_id, bcode_ipc::WorkflowRunControlAction::Resume),
        ),
        START_COMMAND => {
            status_response("unknown /loop action; use status, pause, stop, or resume")
        }
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
            format: bcode_command::CommandTextFormat::PlainText,
        }],
    }
}

fn run_async<F, T>(future: F) -> Result<T, LoopIpcError>
where
    F: std::future::Future<Output = Result<T, ClientError>> + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| LoopIpcError::Worker(error.to_string()))?
            .block_on(future)
            .map_err(LoopIpcError::Client)
    })
    .join()
    .map_err(|_| LoopIpcError::Worker("loop plugin async worker panicked".to_string()))?
}

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value)
        .unwrap_or_else(|error| ServiceResponse::error("encode_failed", error.to_string()))
}

#[must_use]
pub fn static_plugin() -> StaticPluginVtable {
    static_plugin_vtable!(LoopPlugin, include_str!("../bcode-plugin.toml"))
}

#[must_use]
pub fn tui_registry() -> PluginTuiRegistry {
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

enum LoopSurfaceCompletion {
    WorkflowLookup(
        Result<
            Option<bcode_plugin_sdk::tui::PluginWorkflowSummary>,
            bcode_plugin_sdk::tui::PluginTuiHostError,
        >,
    ),
    WorkflowStart {
        request: Box<PluginWorkflowStartRequest>,
        result: Result<
            bcode_plugin_sdk::tui::PluginWorkflowStartResponse,
            bcode_plugin_sdk::tui::PluginTuiHostError,
        >,
    },
}

struct LoopSurface {
    session_id: Option<SessionId>,
    prompt: TextInputState,
    condition: TextInputState,
    limit: TextInputState,
    field: Field,
    pending_workflow_start: Option<PluginWorkflowStartRequest>,
    failed_workflow_start: Option<PluginWorkflowStartRequest>,
    pending_workflow_lookup: bool,
    active_workflow: Option<bcode_plugin_sdk::tui::PluginWorkflowSummary>,
    completions: Arc<Mutex<Vec<LoopSurfaceCompletion>>>,
    status: String,
    prompt_area: Rect,
    condition_area: Rect,
    limit_area: Rect,
    theme: Option<PluginTuiTheme>,
}

impl LoopSurface {
    fn new(session_id: Option<SessionId>) -> Self {
        Self {
            session_id,
            prompt: text_state(""),
            condition: text_state(""),
            limit: text_state(&DEFAULT_MAX_ITERATIONS.to_string()),
            field: Field::Prompt,
            pending_workflow_start: None,
            failed_workflow_start: None,
            pending_workflow_lookup: false,
            active_workflow: None,
            completions: Arc::new(Mutex::new(Vec::new())),
            status: "checking for an active loop…".to_owned(),
            prompt_area: Rect::new(0, 0, 0, 0),
            condition_area: Rect::new(0, 0, 0, 0),
            limit_area: Rect::new(0, 0, 0, 0),
            theme: None,
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

    fn start(&mut self) -> PluginTuiAction {
        if self.pending_workflow_start.is_some() {
            "a durable loop start is already in progress".clone_into(&mut self.status);
            return PluginTuiAction::Redraw;
        }
        if let Some(request) = self.failed_workflow_start.take() {
            self.pending_workflow_start = Some(request);
            "retrying durable loop workflow start".clone_into(&mut self.status);
            return PluginTuiAction::Redraw;
        }
        let Some(session_id) = self.session_id else {
            "an active persisted session is required".clone_into(&mut self.status);
            return PluginTuiAction::Redraw;
        };
        if self.active_workflow.as_ref().is_some_and(|run| {
            matches!(
                run.status,
                PluginWorkflowStatus::Running
                    | PluginWorkflowStatus::Paused
                    | PluginWorkflowStatus::RepairRequired
            )
        }) {
            "this session already has an active loop".clone_into(&mut self.status);
            return PluginTuiAction::Redraw;
        }
        if legacy_state_exists(session_id) {
            unsupported_legacy_message().clone_into(&mut self.status);
            return PluginTuiAction::Redraw;
        }
        let prompt = input_text(&self.prompt);
        let condition = input_text(&self.condition);
        let limit = input_text(&self.limit);
        let Ok(max_iterations) = limit.parse::<u64>() else {
            self.field = Field::Limit;
            "maximum iterations must be a number".clone_into(&mut self.status);
            return PluginTuiAction::Redraw;
        };
        let input = match LoopWorkflowInput::new(prompt, condition, max_iterations) {
            Ok(input) => input,
            Err(error) => {
                self.status = error;
                return PluginTuiAction::Redraw;
            }
        };
        let spec = match loop_workflow_spec(&input) {
            Ok(spec) => spec,
            Err(error) => {
                self.status = error;
                return PluginTuiAction::Redraw;
            }
        };
        let initial = loop_workflow_initial_value(&input);
        let request = match PluginWorkflowStartRequest::typed(
            &spec,
            &initial,
            session_id,
            PluginWorkflowBinding {
                owner_plugin_id: PLUGIN_ID.to_string(),
                workflow_kind: WORKFLOW_KIND.to_string(),
                scope_key: session_id.to_string(),
                display_label: Some("Loop".to_string()),
                single_active: true,
            },
            Some(uuid::Uuid::new_v4().to_string()),
        ) {
            Ok(request) => request,
            Err(error) => {
                self.status = format!("invalid durable loop request: {error}");
                return PluginTuiAction::Redraw;
            }
        };
        self.pending_workflow_start = Some(request);
        "starting durable loop workflow".clone_into(&mut self.status);
        PluginTuiAction::Redraw
    }

    fn begin_workflow_lookup(&mut self, host: &dyn PluginTuiHost) {
        if self.pending_workflow_lookup {
            return;
        }
        self.pending_workflow_lookup = true;
        let lookup = PluginWorkflowBinding {
            owner_plugin_id: PLUGIN_ID.to_string(),
            workflow_kind: WORKFLOW_KIND.to_string(),
            scope_key: self
                .session_id
                .map_or_else(String::new, |id| id.to_string()),
            display_label: Some("Loop".to_string()),
            single_active: true,
        }
        .lookup();
        let completion = Arc::clone(&self.completions);
        let future = host.associated_workflow(lookup);
        host.spawn(Box::pin(async move {
            let result = future.await;
            completion
                .lock()
                .expect("loop surface completion lock")
                .push(LoopSurfaceCompletion::WorkflowLookup(result));
        }));
    }

    fn begin_workflow_start(&mut self, host: &dyn PluginTuiHost) {
        let Some(request) = self.pending_workflow_start.take() else {
            return;
        };
        let completion = Arc::clone(&self.completions);
        let future = host.start_workflow(request.clone());
        host.spawn(Box::pin(async move {
            let result = future.await;
            completion
                .lock()
                .expect("loop surface completion lock")
                .push(LoopSurfaceCompletion::WorkflowStart {
                    request: Box::new(request),
                    result,
                });
        }));
    }

    fn apply_completions(&mut self) -> PluginTuiAction {
        let completions = {
            let mut pending = self
                .completions
                .lock()
                .expect("loop surface completion lock");
            std::mem::take(&mut *pending)
        };
        let mut action = PluginTuiAction::None;
        for completion in completions {
            match completion {
                LoopSurfaceCompletion::WorkflowLookup(result) => {
                    self.active_workflow = result.unwrap_or_else(|error| {
                        self.status = format!("failed to inspect active loop: {error}");
                        None
                    });
                    if !self.status.starts_with("failed to inspect") {
                        self.status = self.active_workflow.as_ref().map_or_else(
                            || "Tab changes field · Ctrl+Enter starts · Esc cancels".to_owned(),
                            format_plugin_workflow_status,
                        );
                    }
                    action = PluginTuiAction::Redraw;
                }
                LoopSurfaceCompletion::WorkflowStart { request, result } => match result {
                    Ok(started) => {
                        return PluginTuiAction::Close {
                            outcome: Some(serde_json::json!({
                                "status": "loop started through durable workflow runtime",
                                "append_text": "Loop started. Use /loop status, /loop stop, or /loop resume.",
                                "run_id": started.run_id,
                                "runtime_work_id": started.runtime_work_id,
                            })),
                        };
                    }
                    Err(error) => {
                        self.failed_workflow_start = Some(*request);
                        self.status = format!(
                            "failed to start durable loop workflow: {error}; submit again to retry"
                        );
                        action = PluginTuiAction::Redraw;
                    }
                },
            }
        }
        action
    }

    fn render_input(
        area: Rect,
        frame: &mut Frame<'_>,
        label: &'static str,
        state: &mut TextInputState,
        focused: bool,
        rows: u16,
        theme: Option<PluginTuiTheme>,
    ) {
        let styles = theme.map_or_else(TextInputBoxStyles::default, |theme| TextInputBoxStyles {
            text: theme.text,
            focused_text: theme.focused,
            disabled_text: theme.muted,
            placeholder: theme.muted,
            selection: theme.selection,
            border: theme.border,
            focused_border: theme.focused,
            background: theme.canvas,
            focused_background: theme.canvas,
            disabled_background: theme.canvas,
        });
        TextInputBox::new(TextInputPolicy::chat_composer())
            .styles(styles)
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
        let modal_theme = self.theme.map_or_else(
            || ModalTheme::dark(Color::Cyan),
            |theme| {
                ModalTheme::new(
                    theme.canvas,
                    theme.border.patch(theme.canvas),
                    theme.focused.patch(theme.canvas),
                    theme.text.patch(theme.canvas),
                    theme.muted.patch(theme.canvas),
                    theme.focused.patch(theme.canvas),
                )
            },
        );
        let modal = ModalFrame::new(
            ModalSizing::new(Size::new(64, 22), Size::new(100, 32), Insets::all(2)),
            modal_theme,
        )
        .title(" Start deterministic loop ")
        .padding(Insets::new(1, 2, 1, 2))
        .placement(ModalPlacement::Centered);
        modal.render(area, frame);
        let content = modal.content_area(area);
        let available = content.height.saturating_sub(8);
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
            4,
        );
        Self::render_input(
            self.prompt_area,
            frame,
            "Iteration prompt",
            &mut self.prompt,
            self.field == Field::Prompt,
            self.prompt_area.height,
            self.theme,
        );
        Self::render_input(
            self.condition_area,
            frame,
            "Stop condition",
            &mut self.condition,
            self.field == Field::Condition,
            self.condition_area.height,
            self.theme,
        );
        Self::render_input(
            self.limit_area,
            frame,
            "Maximum iterations",
            &mut self.limit,
            self.field == Field::Limit,
            1,
            self.theme,
        );
        let status_y = self.limit_area.bottom().saturating_add(1);
        if status_y < content.bottom() {
            let status = [StatusSegment::new(&self.status).severity(StatusSeverity::Muted)];
            StatusBar::new()
                .left(&status)
                .styles(loop_status_styles(self.theme))
                .render(Rect::new(content.x, status_y, content.width, 1), frame);
        }
        let hints_y = status_y.saturating_add(1);
        if hints_y < content.bottom() {
            let hints = [
                KeyHint::new("Tab/Shift-Tab", "field"),
                KeyHint::new("Ctrl-Enter", "start"),
                KeyHint::new("Esc", "close"),
            ];
            KeyHintBar::new(&hints)
                .styles(loop_hint_styles(self.theme))
                .render(Rect::new(content.x, hints_y, content.width, 1), frame);
        }
    }

    fn render_with_theme(
        &mut self,
        area: Rect,
        frame: &mut Frame<'_>,
        theme: Option<PluginTuiTheme>,
    ) {
        self.theme = theme;
        self.render(area, frame);
    }

    fn poll(&mut self, host: &dyn PluginTuiHost) -> PluginTuiAction {
        self.begin_workflow_lookup(host);
        self.begin_workflow_start(host);
        self.apply_completions()
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
                let action = self.start();
                self.begin_workflow_start(host);
                return action;
            }
            if stroke.key == KeyCode::Enter && self.field != Field::Limit {
                self.active_state_mut().buffer_mut().insert_char('\n');
                return PluginTuiAction::Redraw;
            }
            if stroke.key == KeyCode::Enter && self.field == Field::Limit {
                let action = self.start();
                self.begin_workflow_start(host);
                return action;
            }
        }
        self.begin_workflow_start(host);
        self.focus_from_click(event);
        if self.field != Field::Limit
            && matches!(event, Event::Mouse(mouse) if mouse.position.x >= self.active_area().x && mouse.position.x < self.active_area().right() && mouse.position.y >= self.active_area().y && mouse.position.y < self.active_area().bottom() && matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown))
        {
            let motion = match event {
                Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollUp => {
                    bmux_text_edit::TextMotion::VisualUp
                }
                _ => bmux_text_edit::TextMotion::VisualDown,
            };
            let state = self.active_state_mut();
            for _ in 0..3 {
                state
                    .buffer_mut()
                    .move_cursor_with_selection(motion, bmux_text_edit::SelectionMode::Move);
            }
            state.sync_scroll_to_cursor(&TextInputPolicy::chat_composer());
            return PluginTuiAction::Redraw;
        }
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

const fn loop_status_styles(theme: Option<PluginTuiTheme>) -> StatusBarStyles {
    match theme {
        Some(theme) => StatusBarStyles {
            default: theme.text,
            muted: theme.muted,
            info: theme.focused,
            success: theme.focused,
            warning: theme.focused,
            error: theme.focused,
            separator: theme.border,
            background: theme.canvas,
        },
        None => StatusBarStyles {
            default: Style::new().fg(Color::White),
            muted: Style::new().fg(Color::BrightBlack),
            info: Style::new().fg(Color::Cyan),
            success: Style::new().fg(Color::Green),
            warning: Style::new().fg(Color::Yellow),
            error: Style::new().fg(Color::Red),
            separator: Style::new().fg(Color::BrightBlack),
            background: Style::new().bg(Color::Black),
        },
    }
}

const fn loop_hint_styles(theme: Option<PluginTuiTheme>) -> KeyHintBarStyles {
    match theme {
        Some(theme) => KeyHintBarStyles {
            key: theme.focused,
            label: theme.text,
            separator: theme.muted,
            disabled: theme.muted,
            background: theme.canvas,
        },
        None => KeyHintBarStyles {
            key: Style::new().fg(Color::Cyan),
            label: Style::new().fg(Color::White),
            separator: Style::new().fg(Color::BrightBlack),
            disabled: Style::new().fg(Color::BrightBlack),
            background: Style::new().bg(Color::Black),
        },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ReferenceWorkflowOutcome {
    Implementing,
    VerificationFailed,
    VerificationInfrastructureFailed,
    CommitDisabled,
    NoChanges,
    ApprovalDenied,
    Committed,
    Completed,
    IterationLimitExhausted,
}

const REFERENCE_WORKFLOW_STATE_VERSION: u32 = 1;

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReferenceWorkflowState {
    version: u32,
    implementation_prompt: String,
    stop_condition: String,
    iteration_limit: u32,
    iteration: u32,
    condition_met: bool,
    verification_passed: Option<bool>,
    commit_enabled: bool,
    committed_head: Option<String>,
    outcome: ReferenceWorkflowOutcome,
}

#[allow(dead_code)]
impl ReferenceWorkflowState {
    fn validate(&self) -> Result<(), String> {
        if self.version != REFERENCE_WORKFLOW_STATE_VERSION
            || self.implementation_prompt.trim().is_empty()
            || self.stop_condition.trim().is_empty()
            || self.implementation_prompt.len() > MAX_PROMPT_BYTES
            || self.stop_condition.len() > MAX_PROMPT_BYTES
            || self.iteration == 0
            || self.iteration > self.iteration_limit
            || self.iteration_limit > u32::try_from(HARD_MAX_ITERATIONS).unwrap_or(u32::MAX)
        {
            return Err("reference workflow state is invalid or unbounded".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
struct LoopWorkflowInput {
    implementation_prompt: String,
    stop_condition: String,
    max_iterations: u32,
}

impl LoopWorkflowInput {
    fn new(
        implementation_prompt: String,
        stop_condition: String,
        max_iterations: u64,
    ) -> Result<Self, String> {
        if implementation_prompt.trim().is_empty() {
            return Err("iteration prompt is required".to_string());
        }
        if stop_condition.trim().is_empty() {
            return Err("stop condition is required".to_string());
        }
        if implementation_prompt.len() > MAX_PROMPT_BYTES || stop_condition.len() > MAX_PROMPT_BYTES
        {
            return Err(format!(
                "loop prompts must not exceed {MAX_PROMPT_BYTES} bytes"
            ));
        }
        let max_iterations = u32::try_from(max_iterations)
            .map_err(|_| "maximum iterations exceed the workflow limit".to_string())?;
        if !(1..=u32::try_from(HARD_MAX_ITERATIONS).unwrap_or(u32::MAX)).contains(&max_iterations) {
            return Err(format!(
                "maximum iterations must be 1..={HARD_MAX_ITERATIONS}"
            ));
        }
        Ok(Self {
            implementation_prompt,
            stop_condition,
            max_iterations,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
struct LoopWorkflowIteration {
    implementation_prompt: String,
    stop_condition: String,
    max_iterations: u32,
    iteration: u32,
    condition_met: bool,
    evidence: Vec<String>,
    summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
struct LoopWorkflowEvaluation {
    implementation_prompt: String,
    stop_condition: String,
    max_iterations: u32,
    iteration: u32,
    condition_met: bool,
    #[schemars(length(min = 1), inner(length(min = 1)))]
    evidence: Vec<String>,
    #[schemars(length(min = 1))]
    summary: String,
}

#[allow(dead_code)]
const WORKFLOW_COMMIT_MESSAGE_CONTRACT_VERSION: u32 = 1;
#[allow(dead_code)]
const MAX_COMMIT_MESSAGE_TITLE_BYTES: usize = 72;
#[allow(dead_code)]
const MAX_COMMIT_MESSAGE_DESCRIPTION_BYTES: usize = 4_096;

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CommitMessageRequest {
    version: u32,
    repository_root: String,
    expected_head: String,
    paths: Vec<String>,
}

#[allow(dead_code)]
impl CommitMessageRequest {
    fn validate(&self) -> Result<(), String> {
        if self.version != WORKFLOW_COMMIT_MESSAGE_CONTRACT_VERSION
            || self.repository_root.trim().is_empty()
            || self.repository_root.len() > 4_096
            || self.expected_head.trim().is_empty()
            || self.expected_head.len() > 256
            || self.paths.is_empty()
            || self.paths.len() > 1_024
        {
            return Err("commit-message request identity or bounds are invalid".to_string());
        }
        let paths = self.paths.iter().collect::<std::collections::BTreeSet<_>>();
        if paths.len() != self.paths.len()
            || self
                .paths
                .iter()
                .any(|path| path.trim().is_empty() || path.len() > 4_096)
        {
            return Err(
                "commit-message request paths must be unique, non-empty, and bounded".to_string(),
            );
        }
        Ok(())
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CommitMessageResult {
    version: u32,
    title: String,
    description: String,
}

#[allow(dead_code)]
impl CommitMessageResult {
    fn validate(&self) -> Result<(), String> {
        if self.version != WORKFLOW_COMMIT_MESSAGE_CONTRACT_VERSION {
            return Err("unsupported commit-message result version".to_string());
        }
        let title = self.title.trim();
        let description = self.description.trim();
        if title.is_empty()
            || title.len() > MAX_COMMIT_MESSAGE_TITLE_BYTES
            || title.contains(['\r', '\n'])
            || description.is_empty()
            || description.len() > MAX_COMMIT_MESSAGE_DESCRIPTION_BYTES
            || description.contains('\r')
        {
            return Err(
                "commit-message title/description content is invalid or unbounded".to_string(),
            );
        }
        Ok(())
    }

    fn commit_message(&self) -> Result<String, String> {
        self.validate()?;
        Ok(format!(
            "{}\n\n{}",
            self.title.trim(),
            self.description.trim()
        ))
    }
}

#[allow(dead_code)]
fn commit_message_agent_configuration(
    skill_id: &str,
) -> Result<bcode_workflow::WorkflowPromptConfiguration, String> {
    if skill_id.trim().is_empty() || skill_id.len() > 256 {
        return Err("commit-message skill ID is invalid".to_string());
    }
    Ok(bcode_workflow::WorkflowPromptConfiguration {
        version: bcode_workflow::WORKFLOW_PROMPT_CONFIGURATION_VERSION,
        execution_target: bcode_workflow::PromptContextTarget::FreshIsolated,
        agent_profile: "plan".to_string(),
        provider: None,
        model: None,
        structured_output: bcode_workflow::PromptStructuredOutputPolicy {
            schema: bcode_workflow::ValueSchema::of::<CommitMessageResult>(),
            strict: true,
        },
        read_only: true,
        tool_capability: bcode_workflow::WorkflowToolCapability::ReadOnly,
        tool_allowlist: vec!["git.diff".to_string()],
        timeout_ms: 120_000,
        prompt_mode: "json_input".to_string(),
        system_prompt: format!(
            "Use the `{skill_id}` skill when available. Generate only the typed commit-message result for the exact repository snapshot and changed paths. Remain read-only and never commit or mutate Git state."
        ),
    })
}

#[allow(dead_code)]
fn commit_message_agent_step(
    skill_id: &str,
) -> Result<bcode_workflow::Step<CommitMessageRequest, CommitMessageResult>, String> {
    let configuration = commit_message_agent_configuration(skill_id)?;
    configuration
        .validate()
        .map_err(|error| error.to_string())?;
    Ok(bcode_workflow::Step::configured_task(
        "loop.commit-message",
        bcode_workflow::NodeKind::Agent,
        serde_json::to_value(configuration)
            .map_err(|error| format!("commit-message agent configuration failed: {error}"))?,
        |request: CommitMessageRequest, _context| async move {
            request.validate().map_err(|error| {
                bcode_workflow::WorkflowError::step("loop.commit-message", error)
            })?;
            Err(bcode_workflow::WorkflowError::step(
                "loop.commit-message",
                "durable host execution is required for the skill-backed commit-message node",
            ))
        },
    ))
}

fn loop_agent_configuration<O: JsonSchema>(
    system_prompt: &str,
    agent_profile: &str,
    read_only: bool,
) -> bcode_workflow::WorkflowPromptConfiguration {
    bcode_workflow::WorkflowPromptConfiguration {
        version: bcode_workflow::WORKFLOW_PROMPT_CONFIGURATION_VERSION,
        execution_target: bcode_workflow::PromptContextTarget::SharedParentSequential,
        agent_profile: agent_profile.to_string(),
        provider: None,
        model: None,
        structured_output: bcode_workflow::PromptStructuredOutputPolicy {
            schema: bcode_workflow::ValueSchema::of::<O>(),
            strict: true,
        },
        read_only,
        tool_capability: if read_only {
            bcode_workflow::WorkflowToolCapability::ReadOnly
        } else {
            bcode_workflow::WorkflowToolCapability::Mutating
        },
        tool_allowlist: Vec::new(),
        timeout_ms: 3_600_000,
        prompt_mode: "json_input".to_string(),
        system_prompt: system_prompt.to_string(),
    }
}

fn loop_workflow_spec(
    input: &LoopWorkflowInput,
) -> Result<bcode_workflow::WorkflowSpec<LoopWorkflowIteration>, String> {
    let implementation = bcode_workflow::Step::configured_task(
        "loop.implementation",
        bcode_workflow::NodeKind::Agent,
        serde_json::to_value(loop_agent_configuration::<LoopWorkflowIteration>(
            "Implement the requested work. Preserve the workflow envelope fields including iteration, set condition_met false, and provide no evaluation evidence.",
            "build",
            false,
        ))
        .expect("loop agent configuration should serialize"),
        |state: LoopWorkflowIteration, _context| async move { Ok(state) },
    );
    let evaluation = bcode_workflow::Step::configured_task(
        "loop.evaluation",
        bcode_workflow::NodeKind::Agent,
        serde_json::to_value(loop_agent_configuration::<LoopWorkflowEvaluation>(
            "Read-only loop completion evaluation. Inspect repository/session state against stop_condition. Preserve implementation_prompt, stop_condition, max_iterations, and iteration. Return condition_met, non-empty concrete evidence, and a concise non-empty summary in the exact structured schema.",
            "plan",
            true,
        ))
        .expect("loop evaluation configuration should serialize"),
        |state: LoopWorkflowIteration, _context| async move { Ok(state) },
    );
    let cycle =
        implementation
            .agent_execution_target(bcode_workflow::PromptContextTarget::SharedParentSequential)
            .then(evaluation.agent_execution_target(
                bcode_workflow::PromptContextTarget::SharedParentSequential,
            ))
            .repeat_while(
                "loop.repeat",
                bcode_workflow::field::<LoopWorkflowIteration>("condition_met").eq(false),
                input.max_iterations,
            );
    let workflow = bcode_workflow::WorkflowBuilder::new(WORKFLOW_KIND, cycle)
        .build()
        .map_err(|error| error.to_string())?;
    bcode_workflow::WorkflowSpec::new(WORKFLOW_KIND, &workflow).map_err(|error| error.to_string())
}

fn loop_workflow_initial_value(input: &LoopWorkflowInput) -> LoopWorkflowIteration {
    LoopWorkflowIteration {
        implementation_prompt: input.implementation_prompt.clone(),
        stop_condition: input.stop_condition.clone(),
        max_iterations: input.max_iterations,
        iteration: 1,
        condition_met: false,
        evidence: Vec::new(),
        summary: String::new(),
    }
}

fn legacy_state_exists(session_id: SessionId) -> bool {
    legacy_state_exists_at(&legacy_state_path(session_id))
}

fn legacy_state_exists_at(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

fn legacy_state_path(session_id: SessionId) -> PathBuf {
    legacy_state_root().join(format!("{session_id}.json"))
}

fn legacy_state_root() -> PathBuf {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn bedrock_schema_dialect_for_test() -> bcode_model_schema::SchemaDialect {
        use bcode_model_schema::{ObjectPropertyPolicy, SchemaDialect, UnsupportedKeywordPolicy};
        SchemaDialect {
            object_properties: ObjectPropertyPolicy::RequireAllAndClose,
            unsupported_keywords: std::collections::BTreeMap::from([
                ("minimum".to_string(), UnsupportedKeywordPolicy::Remove),
                ("maximum".to_string(), UnsupportedKeywordPolicy::Remove),
                ("multipleOf".to_string(), UnsupportedKeywordPolicy::Remove),
                ("minLength".to_string(), UnsupportedKeywordPolicy::Remove),
                ("maxLength".to_string(), UnsupportedKeywordPolicy::Remove),
            ]),
            accepted_min_items: std::collections::BTreeSet::from([0, 1]),
            ..SchemaDialect::default()
        }
    }

    #[derive(Debug, Default)]
    struct TestHost {
        request: std::sync::Mutex<Option<PluginWorkflowStartRequest>>,
    }

    impl PluginTuiHost for TestHost {
        fn spawn(&self, task: bcode_plugin_sdk::tui::PluginTask) {
            drop(tokio::spawn(task));
        }
        fn spawn_blocking(&self, _task: Box<dyn FnOnce() + Send + 'static>) {}
        fn request_redraw(&self) {}

        fn associated_workflow(
            &self,
            _lookup: bcode_plugin_sdk::tui::PluginWorkflowLookup,
        ) -> bcode_plugin_sdk::tui::PluginWorkflowLookupFuture {
            Box::pin(async { Ok(None) })
        }

        fn start_workflow(
            &self,
            request: PluginWorkflowStartRequest,
        ) -> bcode_plugin_sdk::tui::PluginWorkflowStartFuture {
            *self.request.lock().expect("request") = Some(request);
            Box::pin(async {
                Ok(bcode_plugin_sdk::tui::PluginWorkflowStartResponse {
                    run_id: "durable-loop-run".to_string(),
                    runtime_work_id: "workflow:durable-loop-run".to_string(),
                })
            })
        }
    }

    #[derive(Debug, Default)]
    struct FailingHost {
        attempts: std::sync::atomic::AtomicUsize,
    }

    impl PluginTuiHost for FailingHost {
        fn spawn(&self, task: bcode_plugin_sdk::tui::PluginTask) {
            drop(tokio::spawn(task));
        }
        fn spawn_blocking(&self, _task: Box<dyn FnOnce() + Send + 'static>) {}
        fn request_redraw(&self) {}

        fn associated_workflow(
            &self,
            _lookup: bcode_plugin_sdk::tui::PluginWorkflowLookup,
        ) -> bcode_plugin_sdk::tui::PluginWorkflowLookupFuture {
            Box::pin(async { Ok(None) })
        }

        fn start_workflow(
            &self,
            _request: PluginWorkflowStartRequest,
        ) -> bcode_plugin_sdk::tui::PluginWorkflowStartFuture {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async {
                Err(bcode_plugin_sdk::tui::PluginTuiHostError::Internal(
                    "rejected".to_string(),
                ))
            })
        }
    }

    #[test]
    fn commit_message_contract_is_typed_bounded_and_read_only() {
        let request = CommitMessageRequest {
            version: WORKFLOW_COMMIT_MESSAGE_CONTRACT_VERSION,
            repository_root: "/repo".to_string(),
            expected_head: "0123456789abcdef".to_string(),
            paths: vec!["src/lib.rs".to_string()],
        };
        request.validate().expect("request");
        let result = CommitMessageResult {
            version: WORKFLOW_COMMIT_MESSAGE_CONTRACT_VERSION,
            title: "Add durable workflow skills".to_string(),
            description: "Resolve exact skill context before dispatch.".to_string(),
        };
        assert_eq!(
            result.commit_message().expect("message"),
            "Add durable workflow skills\n\nResolve exact skill context before dispatch."
        );
        assert!(
            CommitMessageResult {
                title: "line one\nline two".to_string(),
                ..result
            }
            .validate()
            .is_err()
        );
        assert!(
            CommitMessageRequest {
                paths: vec!["src/lib.rs".to_string(), "src/lib.rs".to_string()],
                ..request
            }
            .validate()
            .is_err()
        );

        let step = commit_message_agent_step("commit-message").expect("step");
        let workflow = bcode_workflow::WorkflowBuilder::new("commit-message", step)
            .build()
            .expect("workflow");
        let node = &workflow.definition().nodes["loop.commit-message"];
        let configuration: bcode_workflow::WorkflowPromptConfiguration =
            serde_json::from_value(node.configuration.clone()).expect("configuration");
        assert!(configuration.read_only);
        assert_eq!(
            configuration.tool_capability,
            bcode_workflow::WorkflowToolCapability::ReadOnly
        );
        assert_eq!(configuration.tool_allowlist, ["git.diff"]);
        assert!(configuration.system_prompt.contains("commit-message"));
        assert_eq!(
            configuration.structured_output.schema.type_name,
            std::any::type_name::<CommitMessageResult>()
        );
    }

    #[test]
    fn commit_message_contract_rejects_unbounded_content_and_invalid_skill() {
        assert!(commit_message_agent_configuration("").is_err());
        assert!(
            CommitMessageResult {
                version: WORKFLOW_COMMIT_MESSAGE_CONTRACT_VERSION,
                title: "x".repeat(MAX_COMMIT_MESSAGE_TITLE_BYTES + 1),
                description: "body".to_string(),
            }
            .validate()
            .is_err()
        );
        assert!(
            CommitMessageResult {
                version: WORKFLOW_COMMIT_MESSAGE_CONTRACT_VERSION,
                title: "title".to_string(),
                description: "x".repeat(MAX_COMMIT_MESSAGE_DESCRIPTION_BYTES + 1),
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn terminal_workflow_control_message_explains_completed_runs() {
        let run = bcode_workflow_store::WorkflowRunSummary {
            run_id: "completed-loop".to_string(),
            definition_id: "loop".to_string(),
            definition_version: 1,
            workspace_snapshot: ".".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            terminal_output_id: None,
            terminal_output_checksum_sha256: None,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            status: bcode_workflow_store::RunStatus::Completed,
            cancellation_requested_at_ms: None,
            created_at_ms: 1,
            updated_at_ms: 2,
        };

        assert_eq!(
            terminal_workflow_control_message(&run).as_deref(),
            Some("loop workflow is already completed; start a new loop to run it again")
        );
    }

    #[test]
    fn cancellation_requested_status_is_presented_as_cancelling() {
        let run = bcode_workflow_store::WorkflowRunSummary {
            run_id: "run-1".to_string(),
            definition_id: "definition".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: Some(bcode_workflow_store::WorkflowRunBinding {
                owner_plugin_id: PLUGIN_ID.to_string(),
                workflow_kind: WORKFLOW_KIND.to_string(),
                scope_key: "session".to_string(),
                display_label: None,
                single_active: true,
            }),
            authored_provenance: None,
            terminal_output_id: None,
            terminal_output_checksum_sha256: None,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            status: bcode_workflow_store::RunStatus::Running,
            cancellation_requested_at_ms: Some(3),
            created_at_ms: 1,
            updated_at_ms: 3,
        };

        assert!(format_workflow_status(&run).contains("status Cancelling"));
    }

    #[test]
    fn repair_required_status_explains_ambiguous_recovery() {
        let run = bcode_workflow_store::WorkflowRunSummary {
            run_id: "run-1".to_string(),
            definition_id: "definition".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            terminal_output_id: None,
            terminal_output_checksum_sha256: None,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            status: bcode_workflow_store::RunStatus::RepairRequired,
            cancellation_requested_at_ms: Some(3),
            created_at_ms: 1,
            updated_at_ms: 3,
        };

        assert!(
            format_workflow_status(&run)
                .contains("repair required because recovery could not prove")
        );
    }

    #[test]
    fn commands_cover_the_loop_lifecycle() {
        let commands = commands();
        assert_eq!(commands.len(), 5);
        assert!(commands.iter().all(|command| {
            command.execution == bcode_command::CommandExecution::Immediate
                && command.surfaces.contains(&CommandSurface::Slash)
        }));
    }

    #[test]
    fn reference_workflow_state_envelope_is_versioned_bounded_and_explicit() {
        let state = ReferenceWorkflowState {
            version: REFERENCE_WORKFLOW_STATE_VERSION,
            implementation_prompt: "implement".to_string(),
            stop_condition: "tests pass".to_string(),
            iteration_limit: 3,
            iteration: 1,
            condition_met: false,
            verification_passed: None,
            commit_enabled: true,
            committed_head: None,
            outcome: ReferenceWorkflowOutcome::Implementing,
        };
        state.validate().expect("valid state");
        assert!(
            ReferenceWorkflowState {
                iteration: 4,
                ..state.clone()
            }
            .validate()
            .is_err()
        );
        let encoded = serde_json::to_value(&state).expect("state serializes");
        assert_eq!(encoded["outcome"], "implementing");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn workflow_definition_is_typed_read_only_and_bounded() {
        let input =
            LoopWorkflowInput::new("implement".to_string(), "done".to_string(), 3).expect("input");
        let spec = loop_workflow_spec(&input).expect("spec");
        let definition = spec.definition();
        assert_eq!(
            definition.nodes["loop.implementation"].configuration["execution_target"],
            "shared_parent_sequential"
        );
        assert_eq!(
            definition.nodes["loop.evaluation"].configuration["execution_target"],
            "shared_parent_sequential"
        );
        assert_ne!(
            spec.identity().definition_id,
            bcode_workflow::WorkflowSpec::new(
                WORKFLOW_KIND,
                &bcode_workflow::WorkflowBuilder::new(
                    WORKFLOW_KIND,
                    bcode_workflow::Step::configured_task(
                        "loop.implementation",
                        bcode_workflow::NodeKind::Agent,
                        serde_json::json!({
                            "prompt_mode": "json_input",
                            "read_only": false,
                        }),
                        |state: LoopWorkflowIteration, _context| async move { Ok(state) },
                    )
                    .then(bcode_workflow::Step::configured_task(
                        "loop.evaluation",
                        bcode_workflow::NodeKind::Agent,
                        serde_json::json!({
                            "prompt_mode": "json_input",
                            "read_only": true,
                        }),
                        |state: LoopWorkflowIteration, _context| async move { Ok(state) },
                    ))
                    .repeat_while(
                        "loop.repeat",
                        bcode_workflow::field::<LoopWorkflowIteration>("condition_met").eq(false),
                        input.max_iterations,
                    ),
                )
                .build()
                .expect("isolated workflow"),
            )
            .expect("isolated spec")
            .identity()
            .definition_id,
            "execution target must participate in exact definition identity"
        );
        assert_eq!(
            definition.nodes["loop.implementation"].kind,
            bcode_workflow::NodeKind::Agent
        );
        assert_eq!(
            definition.nodes["loop.evaluation"].kind,
            bcode_workflow::NodeKind::Agent
        );
        assert_eq!(
            definition.nodes["loop.repeat"].kind,
            bcode_workflow::NodeKind::Repeat
        );
        assert_eq!(
            definition.nodes["loop.implementation"].configuration["read_only"],
            serde_json::json!(false)
        );
        assert_eq!(
            definition.nodes["loop.evaluation"].configuration["read_only"],
            serde_json::json!(true)
        );
        assert_eq!(
            definition.nodes["loop.evaluation"].configuration["structured_output"]["schema"]["schema"]
                ["properties"]["evidence"]["minItems"],
            serde_json::json!(1)
        );
        assert_eq!(
            definition.nodes["loop.evaluation"].configuration["structured_output"]["schema"]["schema"]
                ["properties"]["summary"]["minLength"],
            serde_json::json!(1)
        );
        assert!(definition.nodes["loop.implementation"].configuration["structured_output"]
            ["schema"]["schema"]["properties"]["evidence"]["minItems"]
            .is_null());
        for node_id in ["loop.implementation", "loop.evaluation"] {
            let schema =
                &definition.nodes[node_id].configuration["structured_output"]["schema"]["schema"];
            bcode_model_schema::normalize(schema, &bedrock_schema_dialect_for_test())
                .unwrap_or_else(|error| panic!("{node_id} schema must fit Bedrock: {error}"));
        }
        assert!(
            definition.nodes["loop.implementation"].configuration["tools"].is_null(),
            "implementation keeps the current session's unrestricted tool policy"
        );
        let admission = definition
            .production_admission(&bcode_workflow::WorkflowProductionCapabilities::current())
            .expect("valid loop definition");
        assert!(
            admission.is_supported(),
            "loop definition must pass production admission: {:?}",
            admission.diagnostics
        );
        assert!(definition.edges.iter().any(|edge| matches!(
            edge.kind,
            bcode_workflow::EdgeKind::Back {
                max_iterations: 3,
                ..
            }
        )));
    }

    #[test]
    fn loop_implementation_allows_empty_evidence_but_evaluation_requires_it() {
        let envelope = serde_json::json!({
            "implementation_prompt": "continue implementation",
            "stop_condition": "all work complete",
            "max_iterations": 20,
            "iteration": 3,
            "condition_met": false,
            "evidence": [],
            "summary": "implementation remains in progress"
        });
        let implementation: LoopWorkflowIteration = serde_json::from_value(envelope.clone())
            .expect("implementation envelope may carry no evaluation evidence");
        assert_eq!(implementation.iteration, 3);
        assert!(!implementation.condition_met);
        assert!(implementation.evidence.is_empty());

        let input = LoopWorkflowInput {
            implementation_prompt: "continue implementation".to_string(),
            stop_condition: "all work complete".to_string(),
            max_iterations: 20,
        };
        let spec = loop_workflow_spec(&input).expect("loop workflow");
        let definition = spec.definition();
        let implementation_schema = &definition.nodes["loop.implementation"].configuration["structured_output"]
            ["schema"]["schema"];
        let evaluation_schema = &definition.nodes["loop.evaluation"].configuration["structured_output"]
            ["schema"]["schema"];
        let implementation_validator =
            jsonschema::validator_for(implementation_schema).expect("implementation schema");
        let evaluation_validator =
            jsonschema::validator_for(evaluation_schema).expect("evaluation schema");

        assert!(implementation_validator.is_valid(&envelope));
        assert!(!evaluation_validator.is_valid(&envelope));
    }

    async fn poll_surface_until_action(
        surface: &mut LoopSurface,
        host: &dyn PluginTuiHost,
    ) -> PluginTuiAction {
        for _ in 0..100 {
            let action = surface.poll(host);
            if !matches!(action, PluginTuiAction::None) {
                return action;
            }
            tokio::task::yield_now().await;
        }
        panic!("loop surface did not produce an action");
    }

    #[test]
    fn start_surface_accepts_typing_in_each_input_field() {
        let host = TestHost::default();
        let mut surface = LoopSurface::new(Some(SessionId::new()));
        surface.limit = text_state("");
        let key = |key| {
            Event::Key(bmux_keyboard::KeyStroke {
                key,
                modifiers: bmux_keyboard::Modifiers::default(),
            })
        };

        assert_eq!(
            surface.handle_event(&key(KeyCode::Char('p')), &host),
            PluginTuiAction::Redraw
        );
        assert_eq!(surface.prompt.buffer().text(), "p");

        assert_eq!(
            surface.handle_event(&key(KeyCode::Tab), &host),
            PluginTuiAction::Redraw
        );
        assert_eq!(
            surface.handle_event(&key(KeyCode::Char('c')), &host),
            PluginTuiAction::Redraw
        );
        assert_eq!(surface.condition.buffer().text(), "c");

        assert_eq!(
            surface.handle_event(&key(KeyCode::Tab), &host),
            PluginTuiAction::Redraw
        );
        assert_eq!(
            surface.handle_event(&key(KeyCode::Char('4')), &host),
            PluginTuiAction::Redraw
        );
        assert_eq!(surface.limit.buffer().text(), "4");
    }

    #[test]
    fn start_surface_consumes_renderer_owned_theme_presentation() {
        let mut surface = LoopSurface::new(Some(SessionId::new()));
        let canvas = Style::new().fg(Color::White).bg(Color::Blue);
        let focused = Style::new().fg(Color::BrightYellow).bg(Color::Blue);
        let muted = Style::new().fg(Color::BrightBlack).bg(Color::Blue);
        let theme = PluginTuiTheme {
            component_theme_version: bcode_plugin_sdk::tui::PLUGIN_TUI_COMPONENT_THEME_VERSION,
            canvas,
            text: Style::new().fg(Color::White),
            muted,
            border: Style::new().fg(Color::Cyan),
            focused,
            selection: Style::new().fg(Color::Black).bg(Color::BrightYellow),
            source: bcode_plugin_sdk::tui::PluginTuiSourceTheme {
                source: Style::new(),
                border: Style::new(),
                gutter: Style::new(),
                truncated: Style::new(),
            },
            diff: bcode_plugin_sdk::tui::PluginTuiDiffTheme {
                text: Style::new(),
                muted: Style::new(),
                title: Style::new(),
                label: Style::new(),
                added: Style::new(),
                removed: Style::new(),
                hunk: Style::new(),
                added_row: Style::new(),
                removed_row: Style::new(),
                added_emphasis: Style::new(),
                removed_emphasis: Style::new(),
            },
            syntax: bcode_plugin_sdk::tui::PluginTuiSyntaxTheme {
                text: bcode_plugin_sdk::tui::PluginTuiSyntaxColor::from_tui(Color::Default),
                comment: bcode_plugin_sdk::tui::PluginTuiSyntaxColor::from_tui(Color::Default),
                keyword: bcode_plugin_sdk::tui::PluginTuiSyntaxColor::from_tui(Color::Default),
                function: bcode_plugin_sdk::tui::PluginTuiSyntaxColor::from_tui(Color::Default),
                variable: bcode_plugin_sdk::tui::PluginTuiSyntaxColor::from_tui(Color::Default),
                string: bcode_plugin_sdk::tui::PluginTuiSyntaxColor::from_tui(Color::Default),
                number: bcode_plugin_sdk::tui::PluginTuiSyntaxColor::from_tui(Color::Default),
                type_name: bcode_plugin_sdk::tui::PluginTuiSyntaxColor::from_tui(Color::Default),
                operator: bcode_plugin_sdk::tui::PluginTuiSyntaxColor::from_tui(Color::Default),
                punctuation: bcode_plugin_sdk::tui::PluginTuiSyntaxColor::from_tui(Color::Default),
            },
        };
        let area = Rect::new(0, 0, 80, 28);
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        surface.render_with_theme(area, &mut Frame::new(&mut buffer), Some(theme));

        assert_eq!(surface.theme, Some(theme));
        assert!(buffer.cells().iter().any(|cell| cell.style.bg == canvas.bg));
        assert!(
            buffer
                .cells()
                .iter()
                .any(|cell| cell.style.fg == focused.fg)
        );
    }

    #[tokio::test]
    async fn start_surface_routes_one_typed_request_through_workflow_host() {
        let session_id = SessionId::new();
        let host = TestHost::default();
        let mut surface = LoopSurface::new(Some(session_id));
        surface.prompt = text_state("implement");
        surface.condition = text_state("done");
        surface.limit = text_state("2");
        assert!(matches!(
            poll_surface_until_action(&mut surface, &host).await,
            PluginTuiAction::Redraw
        ));
        assert_eq!(surface.start(), PluginTuiAction::Redraw);
        surface.begin_workflow_start(&host);
        assert!(matches!(
            poll_surface_until_action(&mut surface, &host).await,
            PluginTuiAction::Close { .. }
        ));
        let request = host
            .request
            .lock()
            .expect("request")
            .clone()
            .expect("start");
        assert_eq!(request.identity.kind, WORKFLOW_KIND);
        assert_eq!(request.parent_session_id, session_id);
        assert_eq!(request.input["implementation_prompt"], "implement");
        assert_eq!(request.input["max_iterations"], 2);
        assert_eq!(request.binding.scope_key, session_id.to_string());

        let ipc_request = bcode_ipc::Request::StartWorkflow(bcode_ipc::WorkflowStartRequest {
            identity: request.identity,
            definition: request.definition,
            run_id: request.run_id,
            workspace_snapshot: None,
            parent_session_id: request.parent_session_id,
            input: request.input,
            binding: bcode_workflow_store::WorkflowRunBinding {
                owner_plugin_id: request.binding.owner_plugin_id,
                workflow_kind: request.binding.workflow_kind,
                scope_key: request.binding.scope_key,
                display_label: request.binding.display_label,
                single_active: request.binding.single_active,
            },
            limits: bcode_workflow_store::WorkflowRunLimits::default(),
        });
        let encoded = bcode_ipc::encode_request(&ipc_request).expect("encode loop start request");
        assert_eq!(
            bcode_ipc::decode_request(&encoded).expect("decode loop start request"),
            ipc_request
        );
    }

    #[tokio::test]
    async fn failed_start_waits_for_explicit_retry() {
        let session_id = SessionId::new();
        let host = FailingHost::default();
        let mut surface = LoopSurface::new(Some(session_id));
        surface.prompt = text_state("implement");
        surface.condition = text_state("done");
        surface.limit = text_state("2");
        assert!(matches!(
            poll_surface_until_action(&mut surface, &host).await,
            PluginTuiAction::Redraw
        ));
        assert_eq!(surface.start(), PluginTuiAction::Redraw);
        surface.begin_workflow_start(&host);
        assert!(matches!(
            poll_surface_until_action(&mut surface, &host).await,
            PluginTuiAction::Redraw
        ));
        assert_eq!(host.attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(surface.pending_workflow_start.is_none());
        assert!(surface.failed_workflow_start.is_some());
        assert!(
            surface
                .status
                .contains("failed to start durable loop workflow")
        );
        assert_eq!(surface.poll(&host), PluginTuiAction::None);
        assert_eq!(
            host.attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "failed starts must not retry automatically"
        );
        assert_eq!(surface.start(), PluginTuiAction::Redraw);
        assert!(surface.failed_workflow_start.is_none());
        assert!(surface.pending_workflow_start.is_some());
        surface.begin_workflow_start(&host);
        assert!(matches!(
            poll_surface_until_action(&mut surface, &host).await,
            PluginTuiAction::Redraw
        ));
        assert_eq!(
            host.attempts.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "explicit retry should reuse the failed request"
        );
    }

    #[test]
    fn legacy_file_detection_is_read_only() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("legacy.json");
        let bytes = br#"{"run_id":"legacy","pending_operation":{}}"#;
        fs::write(&path, bytes).expect("write fixture");
        assert!(legacy_state_exists_at(&path));
        assert_eq!(fs::read(&path).expect("read fixture"), bytes);
        assert_eq!(
            unsupported_legacy_message(),
            "legacy loop state is unsupported by this daemon; use the older daemon that created it"
        );
    }
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(LoopPlugin, include_str!("../bcode-plugin.toml"));
