//! Goal setup is a tool-free prompt generator, not an execution engine.
use super::*;
use bcode_plugin_sdk::tui::PluginStructuredGenerationRequest;

pub const SURFACE_KIND: &str = "goal.start";

pub fn start_response(arguments: &str) -> InvokeCommandResponse {
    let tokens: Vec<_> = arguments.split_whitespace().collect();
    let allowance = match tokens.as_slice() {
        [] | ["--collaborate"] => Ok(None),
        ["--worker-attempts", count] | ["--collaborate", "--worker-attempts", count] => count
            .parse::<std::num::NonZeroU64>()
            .map(|count| Some(count.get()))
            .map_err(|_| ()),
        _ => Err(()),
    };
    let Ok(allowance) = allowance else {
        let mut response =
            status_response("Usage: /goal [--collaborate] [--worker-attempts <positive integer>]");
        response.success = false;
        return response;
    };
    InvokeCommandResponse {
        success: true,
        message: None,
        updated_model: None,
        updated_provider: None,
        updated_thinking: None,
        effects: vec![CommandEffect::OpenPluginSurface {
            surface_kind: SURFACE_KIND.into(),
            instance_id: "goal-start".into(),
            options: serde_json::json!({"collaboration":tokens.first() == Some(&"--collaborate"), "worker_attempts":allowance}),
        }],
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GeneratedGoalPrompts {
    implementation_prompt: String,
    stop_condition: String,
}

fn generation_request(
    objective: &str,
    guidance: &str,
    progress_document: bool,
    collaboration: CollaborationMode,
) -> PluginStructuredGenerationRequest {
    PluginStructuredGenerationRequest {
        source_session_id: None,
        session_name: "Goal prompt generation".into(),
        system_prompt: [
            if collaboration == CollaborationMode::Requested {
                "The user explicitly requested collaborating agents. Preserve this requirement in both generated prompts: use the existing goal/workflow delegation mechanisms, collect worker evidence, revise or delegate follow-up work, integrate contributions and verify the original objective. Do not silently substitute a single-agent result. Missing collaboration support, permissions or verified results are blockers, not completion. Collaboration does not grant tools or prescribe a workspace or integration strategy."
            } else { "Preserve the user's objective. Collaboration is optional by default, but explicit requests for multiple agents or independent reviewers in the objective must remain requirements. Do not invent a collaboration requirement when the objective does not contain one." },
            include_str!("../prompts/goal-generation.md"),
            include_str!("../prompts/goal-iteration-guidance.md"),
            include_str!("../prompts/goal-stop-condition-guidance.md"),
        ]
        .join("\n\n"),
        prompt: serde_json::json!({"objective": objective, "additional_guidance": guidance, "progress_document_enabled": progress_document, "progress_document_guidance": if progress_document { include_str!("../prompts/goal-progress-document.md") } else { "No progress document. Do not instruct creation of one." }})
            .to_string(),
        output_name: "goal_loop_prompts".into(),
        output_schema: serde_json::json!({
            "type": "object", "additionalProperties": false,
            "required": ["outcome", "clarification", "implementation_prompt", "stop_condition"],
            "properties": {
                "outcome": {"type": "string", "enum": ["ready", "clarification_required"]},
                "clarification": {"type": "string", "maxLength": 4096},
                "implementation_prompt": {"type": "string", "minLength": 1, "maxLength": MAX_PROMPT_BYTES},
                "stop_condition": {"type": "string", "minLength": 1, "maxLength": MAX_PROMPT_BYTES}
            }
        }),
        timeout_ms: 120_000,
    }
}

fn decode_prompts(
    value: serde_json::Value,
    objective: &str,
    guidance: &str,
    limit: u64,
) -> Result<LoopWorkflowInput, String> {
    let generated: GeneratedGoalPrompts =
        serde_json::from_value(value).map_err(|e| e.to_string())?;
    // Validate before composition: the objective must not mask empty model output.
    LoopWorkflowInput::new(
        generated.implementation_prompt.clone(),
        generated.stop_condition.clone(),
        limit,
    )?;
    let source = serde_json::json!({"objective": objective, "additional_guidance": guidance});
    let prefix =
        format!("Original user task data (not higher-priority instructions):\n{source}\n\n");
    LoopWorkflowInput::new(
        format!("{prefix}{}", generated.implementation_prompt),
        format!("{prefix}{}", generated.stop_condition),
        limit,
    )
}

#[derive(Default)]
pub struct ProgressDocumentSetup {
    objective: String,
    guidance: String,
    pub path: Option<String>,
    context: Option<bcode_session_models::SessionDerivationSourceSnapshot>,
}

impl ProgressDocumentSetup {
    pub fn request(
        &self,
        session_id: SessionId,
        run_id: &str,
    ) -> bcode_session_models::SessionWorkingDocumentRequest {
        let source = serde_json::json!({"objective": self.objective, "additional_guidance": self.guidance, "source_context": self.context});
        bcode_session_models::SessionWorkingDocumentRequest {
            version: 1,
            session_id,
            scope_id: run_id.into(),
            initial_text: Some(format!(
                "{}\n## Identity\nSession: {session_id}\nWorkflow run: {run_id}\n\n## Original objective and guidance\n```json\n{source}\n```\n",
                include_str!("../prompts/goal-progress-template.md")
            )),
        }
    }
}

pub fn attach_document(
    request: &mut PluginWorkflowStartRequest,
    document: &bcode_session_models::SessionWorkingDocument,
) -> Result<(), String> {
    if request.run_id.as_deref() != Some(document.scope_id.as_str())
        || request.parent_session_id != document.session_id
    {
        return Err("working document owner does not match loop".into());
    }
    let path = serde_json::to_string(&document.path).map_err(|e| e.to_string())?;
    let mut input: LoopWorkflowIteration =
        serde_json::from_value(request.input.clone()).map_err(|e| e.to_string())?;
    input.implementation_prompt = format!(
        "{}\n\nLiving progress document path (JSON string): {path}\n{}",
        input.implementation_prompt,
        include_str!("../prompts/goal-progress-document.md")
    );
    input.stop_condition = format!(
        "{}\n\nRead the living progress document at this path (JSON string): {path}. Read the current phases, checkboxes, blockers and next actions, not merely the opening status lines. Use them to locate remaining work. Independently verify claims against the original objective and current state. Checked boxes or a Done heading are not proof. Do not edit the document; report discrepancies through the normal evaluation result. Missing or unreadable notes are an explicit verification gap, not success.",
        input.stop_condition
    );
    LoopWorkflowInput::new(
        input.implementation_prompt.clone(),
        input.stop_condition.clone(),
        u64::from(input.max_iterations),
    )?;
    request.input = serde_json::to_value(input).map_err(|e| e.to_string())?;
    Ok(())
}

fn unblock_decision(arguments: &str) -> Result<(&str, bool), &'static str> {
    let tokens = arguments.split_whitespace().collect::<Vec<_>>();
    match tokens.as_slice() {
        [activation, "approve"] => Ok((activation, true)),
        [activation, "deny"] => Ok((activation, false)),
        _ => Err(
            "Usage: /goal.unblock <activation-id> approve|deny. Resolve the original permission/dependency first; this only decides whether the goal resumes.",
        ),
    }
}

pub fn unblock_response(session_id: SessionId, arguments: &str) -> InvokeCommandResponse {
    let (activation_id, approved) = match unblock_decision(arguments) {
        Ok(decision) => decision,
        Err(message) => return status_response(message),
    };
    let activation_id = activation_id.to_owned();
    let result = run_async(async move {
        let client = BcodeClient::default_endpoint();
        let Some(run) = client
            .associated_workflow_run(workflow_binding_key(session_id))
            .await?
        else {
            return Ok("No associated goal".to_string());
        };
        client
            .resolve_workflow_approval(run.run_id, "loop.blocked".into(), activation_id, approved)
            .await?;
        Ok(if approved {
            "Goal resume checkpoint approved. Original tool permissions remain enforced."
                .to_string()
        } else {
            "Goal resume checkpoint denied.".to_string()
        })
    });
    match result {
        Ok(message) => status_response(&message),
        Err(error) => status_response(&format!("Goal blocker resolution unavailable: {error}")),
    }
}

pub fn progress_status(session_id: SessionId) -> InvokeCommandResponse {
    let result = run_async(async move {
        let client = BcodeClient::default_endpoint();
        let Some(inspection) = client
            .inspect_associated_workflow_run(workflow_binding_key(session_id), 10)
            .await?
        else {
            return Ok("No associated loop".into());
        };
        let run = &inspection.run;
        let mut status = format_workflow_inspection_status(&inspection);
        if run.status == bcode_workflow_store::RunStatus::Failed
            && let Ok(source) = client
                .workflow_continuation_source(run.run_id.clone())
                .await
        {
            let _ = write!(
                status,
                "\nIteration allowance exhausted · {} iterations completed overall · /goal.continue <additional_iterations>",
                source.total_iterations_completed
            );
        }
        let document_scope = inspection.continuation.as_ref().map_or_else(
            || run.run_id.clone(),
            |lineage| lineage.document_scope_id.clone(),
        );
        match client
            .session_working_document(bcode_session_models::SessionWorkingDocumentRequest {
                version: 1,
                session_id,
                scope_id: document_scope,
                initial_text: None,
            })
            .await?
        {
            Some(document) => {
                let _ = write!(
                    status,
                    "\nProgress document: {} · /goal.progress to read",
                    document.path
                );
            }
            None => status.push_str("\nNo progress document"),
        }
        Ok(status)
    });
    match result {
        Ok(status) => status_response(&status),
        Err(error) => status_response(&format!("Goal status unavailable: {error}")),
    }
}

pub fn progress_response(session_id: SessionId) -> InvokeCommandResponse {
    InvokeCommandResponse {
        success: true,
        message: None,
        updated_model: None,
        updated_provider: None,
        updated_thinking: None,
        effects: vec![CommandEffect::OpenPluginSurface {
            surface_kind: crate::goal_document_view::SURFACE.into(),
            instance_id: "goal-progress".into(),
            options: serde_json::json!({"session_id":session_id}),
        }],
    }
}

pub struct GoalSurfaceFactory;
impl PluginTuiSurfaceFactory for GoalSurfaceFactory {
    fn surface_kind(&self) -> &'static str {
        SURFACE_KIND
    }
    fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        Box::pin(async move {
            let session = request
                .options
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|s| SessionId::from_str(s).ok());
            let mut surface = GoalSurface::new(session);
            surface.editor.collaboration = if request
                .options
                .get("collaboration")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                CollaborationMode::Requested
            } else {
                CollaborationMode::Disabled
            };
            surface.editor.worker_attempts = request
                .options
                .get("worker_attempts")
                .and_then(serde_json::Value::as_u64);
            Ok(Box::new(surface) as BoxedPluginTuiSurface)
        })
    }
}

type GenerationResult = Result<bcode_plugin_sdk::tui::PluginStructuredGenerationResult, String>;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalPhase {
    Draft,
    CheckingDelegation { review: bool },
    Generating { review: bool },
    Generated,
    Closed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GoalOption {
    Progress,
    Review,
}

pub const PROGRESS_LABEL: &str = "Maintain a progress document (Ctrl+P)";
pub const REVIEW_LABEL: &str = "Generate instructions… (Ctrl+R)";

struct GoalSurface {
    editor: LoopSurface,
    phase: GoalPhase,
    pending_review: Option<bool>,
    source: Option<(String, String, u64)>,
    completion: Arc<Mutex<Option<GenerationResult>>>,
    live: Option<crate::goal_live::GenerationView>,
    generation_preflight: Arc<Mutex<Option<Result<(), String>>>>,
    preflight_pending: bool,
    preflight_completion: Arc<Mutex<Option<String>>>,
}

fn preflight_setup_summary(result: &bcode_workflow::WorkflowDelegationPreflight) -> String {
    if delegation_preflight_message(result).is_err() {
        return "Delegation check incompatible; /goal.preflight for details. No authority changed."
            .into();
    }
    if result.plugin_loaded && result.staging_configured && result.publication_configured {
        "Delegation prerequisites present (not an execution grant); /goal.preflight for details."
            .into()
    } else {
        "Delegation prerequisites missing; /goal.preflight for remedies. No authority changed."
            .into()
    }
}

pub async fn require_delegation(
    future: bcode_plugin_sdk::tui::PluginWorkflowDelegationPreflightFuture,
) -> Result<(), String> {
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), future)
        .await
        .map_err(|_| "delegation prerequisite check timed out".to_string())?
        .map_err(|error| error.to_string())?;
    let message = delegation_preflight_message(&result)?;
    if result.plugin_loaded && result.staging_configured && result.publication_configured {
        Ok(())
    } else {
        Err(message)
    }
}

async fn preflight_message_with_timeout(
    future: bcode_plugin_sdk::tui::PluginWorkflowDelegationPreflightFuture,
    timeout: std::time::Duration,
) -> String {
    match tokio::time::timeout(timeout, future).await {
        Ok(Ok(result)) => preflight_setup_summary(&result),
        Ok(Err(error)) => format!(
            "Delegation prerequisites could not be verified: {error}. No authority changed."
        ),
        Err(_) => "Delegation prerequisites could not be verified: check timed out; retry with Ctrl-D. No authority changed.".into(),
    }
}

impl GoalSurface {
    fn cancelled_generation(&mut self, result: GenerationResult) -> PluginTuiAction {
        self.phase = GoalPhase::Draft;
        self.source = None;
        self.editor.status = match result {
            Ok(_) => {
                "Generation finished after cancellation; result discarded. Goal not started.".into()
            }
            Err(error) => format!("Generation stopped: {error}"),
        };
        PluginTuiAction::Redraw
    }

    fn new(session: Option<SessionId>) -> Self {
        let mut editor = LoopSurface::new(session);
        editor.setup_kind = SetupKind::Goal;
        editor.origin = SetupKind::Goal;
        editor.progress_document = Some(ProgressDocumentSetup::default());
        editor.limit = text_state("");
        Self {
            editor,
            phase: GoalPhase::Draft,
            pending_review: None,
            source: None,
            completion: Arc::default(),
            live: None,
            generation_preflight: Arc::default(),
            preflight_pending: false,
            preflight_completion: Arc::default(),
        }
    }

    fn prepare_controls(&mut self) {
        self.editor.goal_phase = self.phase;
        let disabled = !matches!(self.phase, GoalPhase::Draft | GoalPhase::Generated)
            || matches!(
                self.editor.fresh_session,
                FreshSessionState::Creating
                    | FreshSessionState::Configuring
                    | FreshSessionState::Attaching
            );
        let mut checkbox = self.editor.progress_checkbox.get();
        checkbox.set_disabled(disabled);
        self.editor.progress_checkbox.set(checkbox);
        let mut button = self.editor.review_button.get();
        button.set_disabled(disabled);
        self.editor.review_button.set(button);
    }

    fn handle_options(
        &mut self,
        event: &Event,
        host: &dyn PluginTuiHost,
    ) -> Option<PluginTuiAction> {
        use bmux_tui_components::button::{Button, ButtonOutcome, ButtonPolicy};
        use bmux_tui_components::checkbox::{Checkbox, CheckboxOutcome};

        self.prepare_controls();
        let editor = &mut self.editor;
        if let Event::Key(stroke) = event
            && stroke.key == KeyCode::Tab
        {
            editor.goal_option_focus = match (
                editor.goal_option_focus,
                editor.field,
                stroke.modifiers.shift,
            ) {
                (None, Field::Limit, false) | (Some(GoalOption::Review), _, true)
                    if editor.goal_phase == GoalPhase::Generated =>
                {
                    editor.field = Field::Evaluation;
                    None
                }
                (None, Field::Evaluation, false) if editor.goal_phase == GoalPhase::Generated => {
                    Some(GoalOption::Review)
                }
                (Some(GoalOption::Progress), _, false) | (None, Field::Prompt, true) => {
                    Some(GoalOption::Review)
                }
                (Some(GoalOption::Review), _, true) | (None, Field::Evaluation, false) => {
                    Some(GoalOption::Progress)
                }
                (Some(_), _, reverse) => {
                    editor.field = if reverse {
                        Field::Evaluation
                    } else {
                        Field::Prompt
                    };
                    None
                }

                (None, _, _) => return None,
            };
            editor.sync_goal_controls();
            return Some(PluginTuiAction::Redraw);
        }
        editor.sync_goal_controls();
        let mut checkbox = editor.progress_checkbox.get();
        let mut button = editor.review_button.get();
        let checkbox_outcome = if !matches!(event, Event::Key(_) | Event::Paste(_))
            || editor.goal_option_focus == Some(GoalOption::Progress)
        {
            Checkbox::new(PROGRESS_LABEL).handle_event(
                editor.progress_document_area,
                &mut checkbox,
                event,
            )
        } else {
            CheckboxOutcome::Ignored
        };
        let button_outcome = if !matches!(event, Event::Key(_) | Event::Paste(_))
            || editor.goal_option_focus == Some(GoalOption::Review)
        {
            Button::new(REVIEW_LABEL)
                .policy(ButtonPolicy::interactive())
                .handle_event(editor.review_area, &mut button, event)
        } else {
            ButtonOutcome::Ignored
        };
        if !editor.progress_checkbox.get().interaction().focused && checkbox.interaction().focused {
            editor.goal_option_focus = Some(GoalOption::Progress);
        }
        if !editor.review_button.get().focused() && button.focused() {
            editor.goal_option_focus = Some(GoalOption::Review);
        }
        editor.progress_checkbox.set(checkbox);
        editor.review_button.set(button);
        if let CheckboxOutcome::Toggled(checked) = checkbox_outcome {
            editor.progress_document = checked.then(ProgressDocumentSetup::default);
        }
        if button_outcome == ButtonOutcome::Pressed {
            if self.phase == GoalPhase::Generated {
                let action = self.editor.submit(host);
                self.editor.begin_pending_host_work(host);
                return Some(action);
            }
            return Some(self.generate(host, true));
        }
        if !matches!(checkbox_outcome, CheckboxOutcome::Ignored) || button_outcome.is_handled() {
            return Some(PluginTuiAction::Redraw);
        }
        if matches!(event, Event::Mouse(_)) {
            if event_click_in(event, editor.prompt_area)
                || event_click_in(event, editor.condition_area)
                || event_click_in(event, editor.limit_area)
                || event_click_in(event, editor.evaluation_area)
            {
                editor.goal_option_focus = None;
            }
        } else if editor.goal_option_focus.is_some()
            && matches!(event, Event::Key(_) | Event::Paste(_))
        {
            return Some(PluginTuiAction::None);
        }
        None
    }

    fn poll_generation_preflight(&mut self, host: &dyn PluginTuiHost) -> Option<PluginTuiAction> {
        let result = self
            .generation_preflight
            .lock()
            .expect("generation preflight completion")
            .take()?;
        let GoalPhase::CheckingDelegation { review } = self.phase else {
            return None;
        };
        self.phase = GoalPhase::Draft;
        Some(match result {
            Ok(()) => self.generate_checked(host, review, true),
            Err(error) => {
                self.editor.status =
                    format!("Collaboration generation blocked: {error}; submit to retry");
                PluginTuiAction::Redraw
            }
        })
    }

    fn resume_session_generation(&mut self, host: &dyn PluginTuiHost) -> Option<PluginTuiAction> {
        if self.editor.fresh_session != FreshSessionState::Resume {
            return None;
        }
        self.editor.fresh_session = FreshSessionState::Ready;
        self.pending_review
            .take()
            .map(|review| self.generate(host, review))
    }

    fn poll_preflight(&mut self) -> bool {
        let result = self
            .preflight_completion
            .lock()
            .expect("preflight completion")
            .take();
        if let Some(message) = result {
            self.preflight_pending = false;
            if matches!(self.phase, GoalPhase::Draft | GoalPhase::Generated) {
                self.editor.status = message;
                return true;
            }
        }
        false
    }

    fn check_delegation(&mut self, host: &dyn PluginTuiHost) -> PluginTuiAction {
        if self.preflight_pending {
            return PluginTuiAction::None;
        }
        self.preflight_pending = true;
        self.editor.status = "Checking delegation prerequisites… (advisory only)".into();
        let future = host.workflow_delegation_preflight("bcode.workflow".into());
        let completion = self.preflight_completion.clone();
        host.spawn(Box::pin(async move {
            let message =
                preflight_message_with_timeout(future, std::time::Duration::from_secs(10)).await;
            *completion.lock().expect("preflight completion") = Some(message);
        }));
        PluginTuiAction::Redraw
    }

    fn generate(&mut self, host: &dyn PluginTuiHost, review: bool) -> PluginTuiAction {
        self.generate_checked(host, review, false)
    }

    fn generate_checked(
        &mut self,
        host: &dyn PluginTuiHost,
        review: bool,
        checked: bool,
    ) -> PluginTuiAction {
        if self.phase != GoalPhase::Draft {
            return PluginTuiAction::None;
        }
        let objective = input_text(&self.editor.prompt);
        let guidance = input_text(&self.editor.condition);
        let limit = input_text(&self.editor.limit);
        let limit = if limit.is_empty() {
            Ok(DEFAULT_MAX_ITERATIONS)
        } else {
            limit
                .parse::<u64>()
                .map_err(|_| "maximum iterations must be a number".to_owned())
        };
        let validation = limit.and_then(|limit| {
            LoopWorkflowInput::new(objective.clone(), "validation".into(), limit)?;
            if guidance.len() > MAX_PROMPT_BYTES {
                return Err("additional guidance is too large".into());
            }
            Ok(limit)
        });
        let limit = match validation {
            Ok(limit) => limit,
            Err(error) => {
                self.editor.status = error;
                return PluginTuiAction::Redraw;
            }
        };
        if self.editor.collaboration == CollaborationMode::Requested && !checked {
            self.phase = GoalPhase::CheckingDelegation { review };
            self.editor.status = "Checking collaboration prerequisites before generation…".into();
            let future = host.workflow_delegation_preflight("bcode.workflow".into());
            let completion = self.generation_preflight.clone();
            host.spawn(Box::pin(async move {
                *completion.lock().expect("generation preflight completion") =
                    Some(require_delegation(future).await);
            }));
            return PluginTuiAction::Redraw;
        }
        if !self.editor.ensure_session(host) {
            self.pending_review = Some(review);
            return PluginTuiAction::Redraw;
        }
        if let Some(setup) = &mut self.editor.progress_document {
            setup.objective.clone_from(&objective);
            setup.guidance.clone_from(&guidance);
        }
        self.source = Some((objective.clone(), guidance.clone(), limit));
        self.phase = GoalPhase::Generating { review };
        self.editor.status = "Preparing source context… Esc cancels generation.".into();
        let mut request = generation_request(
            &objective,
            &guidance,
            self.editor.progress_document.is_some(),
            self.editor.collaboration,
        );
        request.source_session_id = self.editor.session_id;
        let live = crate::goal_live::GenerationView::default();
        let future = host.generate_observable_structured_output(request, live.control.clone());
        self.live = Some(live);
        let completion = Arc::clone(&self.completion);
        host.spawn(Box::pin(async move {
            *completion.lock().expect("goal generation completion") =
                Some(future.await.map_err(|e| e.to_string()));
        }));
        PluginTuiAction::Redraw
    }
}

pub fn paint_generation(
    live: &mut crate::goal_live::GenerationView,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
) {
    use bmux_tui_components::text_view::{TextViewComponent, TextViewPolicy};
    let modal = ModalFrame::new(
        ModalSizing::new(Size::new(40, 12), Size::new(100, 32), Insets::all(1)),
        ModalTheme::dark(Color::Cyan),
    )
    .title(live.title)
    .padding(Insets::all(1));
    let content = modal.content_area(area).intersection(area);
    let shell = ModalFrameComponent::new("goal.live", modal, TextBlock::new(""));
    let layout = shell.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    frame.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| shell.paint(&layout, cx),
    );
    let status = TextBlock::new(live.status());
    let status_area = Rect::new(content.x, content.y, content.width, content.height.min(4));
    let layout = status.layout(Constraints::tight(status_area.size()), &mut LayoutCx::new());
    frame.with_child(
        i32::from(status_area.x),
        i64::from(status_area.y),
        LocalRect::new(0, 0, status_area.width, status_area.height),
        |cx| status.paint(&layout, cx),
    );
    live.layout = None;
    if !live.collapsed {
        let body = Rect::new(
            content.x,
            content.y.saturating_add(status_area.height),
            content.width,
            content.height.saturating_sub(status_area.height),
        );
        let view = TextViewComponent::new("goal.live.output", &live.lines, &live.scroll)
            .policy(TextViewPolicy::scrollable());
        let layout = view.layout(Constraints::tight(body.size()), &mut LayoutCx::new());
        frame.with_child(
            i32::from(body.x),
            i64::from(body.y),
            LocalRect::new(0, 0, body.width, body.height),
            |cx| view.paint(&layout, cx),
        );
        live.layout = Some((body, layout));
    }
}

impl PluginTuiSurface for GoalSurface {
    fn session_navigation_finished(&mut self, session_id: SessionId, result: Result<(), String>) {
        if self
            .live
            .as_ref()
            .is_some_and(|live| live.control.session_id() == Some(session_id))
        {
            if let Err(error) = result {
                self.editor.status = error;
            }
            return;
        }
        self.editor.session_navigation_finished(session_id, result);
    }

    fn id(&self) -> &'static str {
        SURFACE_KIND
    }
    fn title(&self) -> &'static str {
        "Start Goal"
    }
    fn preferred_height(&mut self, width: u16) -> u16 {
        self.editor.preferred_height(width)
    }
    fn render(&mut self, area: Rect, frame: &mut PaintCx<'_, '_>) {
        self.prepare_controls();
        if let Some(live) = &mut self.live {
            paint_generation(live, area, frame);
        } else {
            self.editor.render(area, frame);
        }
    }
    fn render_with_theme(
        &mut self,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
        theme: Option<&PluginTuiTheme>,
    ) {
        self.editor.theme = theme.copied();
        self.render(area, frame);
    }
    fn poll(&mut self, host: &dyn PluginTuiHost) -> PluginTuiAction {
        if self.phase == GoalPhase::Closed {
            return PluginTuiAction::None;
        }
        if let Some(action) = self.poll_generation_preflight(host) {
            return action;
        }
        if let Some(action) = self.resume_session_generation(host) {
            return action;
        }
        if let Some(live) = &mut self.live {
            live.poll(host);
        }
        let action = self.editor.poll(host);
        let preflight_changed = self.poll_preflight();
        let result = self
            .completion
            .lock()
            .expect("goal generation completion")
            .take();
        let Some(result) = result else {
            return if (self.live.is_some() || preflight_changed)
                && matches!(action, PluginTuiAction::None | PluginTuiAction::Redraw)
            {
                PluginTuiAction::Redraw
            } else {
                action
            };
        };
        let cancelled = self
            .live
            .take()
            .is_some_and(|live| live.control.is_cancelled());
        if cancelled {
            return self.cancelled_generation(result);
        }
        let review = matches!(self.phase, GoalPhase::Generating { review: true });
        self.phase = GoalPhase::Draft;
        let Some((objective, guidance, limit)) = self.source.take() else {
            return action;
        };
        let result = result.and_then(|result| {
            let source = result
                .source
                .ok_or_else(|| "Generation returned no source context provenance".to_string())?;
            if Some(source.session_id) != self.editor.session_id {
                return Err("Generation context belongs to another session".into());
            }
            let provenance = format!(
                "Generation source session {} at sequence {} (generation {}).\n",
                source.session_id, source.latest_sequence, source.generation
            );
            if let Some(setup) = &mut self.editor.progress_document {
                setup.context = Some(source);
            }
            let mut value = result.output;
            if value.get("outcome").and_then(serde_json::Value::as_str)
                == Some("clarification_required")
            {
                return Err(format!(
                    "Clarify the goal: {}",
                    value
                        .get("clarification")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("Please specify the intended outcome")
                ));
            }
            if value.get("outcome").and_then(serde_json::Value::as_str) != Some("ready") {
                return Err("Unknown goal generation outcome".into());
            }
            if let Some(object) = value.as_object_mut() {
                object.remove("outcome");
                object.remove("clarification");
            }
            let input = decode_prompts(value, &objective, &guidance, limit)?;
            LoopWorkflowInput::new(
                format!("{provenance}{}", input.implementation_prompt),
                format!("{provenance}{}", input.stop_condition),
                limit,
            )
        });
        match result {
            Ok(input) => {
                self.editor.prompt = text_state(&input.implementation_prompt);
                self.editor.condition = text_state(&input.stop_condition);
                self.editor.limit = text_state(&limit.to_string());
                self.editor.setup_kind = SetupKind::Loop;
                self.editor.goal_option_focus = None;
                self.phase = GoalPhase::Generated;
                self.editor.status = "Review generated prompts · Ctrl+Enter starts the loop".into();
                if !review {
                    let action = self.editor.start();
                    self.editor.begin_pending_host_work(host);
                    return action;
                }
            }
            Err(error) => {
                self.editor.status = format!("Goal generation failed: {error}; submit to retry");
            }
        }
        PluginTuiAction::Redraw
    }
    fn handle_event(&mut self, event: &Event, host: &dyn PluginTuiHost) -> PluginTuiAction {
        if let Some(live) = &mut self.live {
            if let Event::Key(stroke) = event {
                if stroke.key == KeyCode::Escape {
                    live.control.cancel();
                } else if stroke.key == KeyCode::Char('h')
                    && let Some(session_id) = live.control.session_id()
                {
                    return PluginTuiAction::OpenSession { session_id };
                } else if stroke.key == KeyCode::Tab {
                    live.collapsed = !live.collapsed;
                    live.layout = None;
                }
            }
            if !live.collapsed
                && let Some((area, layout)) = &live.layout
            {
                use bmux_tui_components::text_view::{TextViewComponent, TextViewPolicy};
                let _ = TextViewComponent::new("goal.live.output", &live.lines, &live.scroll)
                    .policy(TextViewPolicy::scrollable())
                    .handle_event(*area, layout, event);
            }
            return PluginTuiAction::Redraw;
        }
        if let Event::Key(stroke) = event {
            if matches!(self.phase, GoalPhase::Draft | GoalPhase::Generated)
                && stroke.key == KeyCode::Char('d')
                && stroke.modifiers.ctrl
            {
                return self.check_delegation(host);
            }
            if stroke.key == KeyCode::Escape && stroke.modifiers.is_empty() {
                self.phase = GoalPhase::Closed;
                return PluginTuiAction::Close { outcome: None };
            }
            if self.phase == GoalPhase::Draft
                && !matches!(
                    self.editor.fresh_session,
                    FreshSessionState::Creating
                        | FreshSessionState::Configuring
                        | FreshSessionState::NeedsConfiguration
                        | FreshSessionState::Attaching
                )
            {
                if stroke.modifiers.ctrl && stroke.key == KeyCode::Char('p') {
                    self.editor.progress_document = if self.editor.progress_document.is_some() {
                        None
                    } else {
                        Some(ProgressDocumentSetup::default())
                    };
                    return PluginTuiAction::Redraw;
                }
                if stroke.modifiers.ctrl && stroke.key == KeyCode::Char('r') {
                    return self.generate(host, true);
                }
                if stroke.key == KeyCode::Enter
                    && (stroke.modifiers.ctrl
                        || (self.editor.field == Field::Limit
                            && self.editor.goal_option_focus.is_none()))
                {
                    return self.generate(host, false);
                }
            }
        }
        if matches!(self.phase, GoalPhase::Draft | GoalPhase::Generated)
            && !matches!(
                self.editor.fresh_session,
                FreshSessionState::Creating
                    | FreshSessionState::Configuring
                    | FreshSessionState::Attaching
            )
            && let Some(action) = self.handle_options(event, host)
        {
            return action;
        }
        // Freeze source inputs during generation; no stale response can replace newer edits.
        if matches!(self.phase, GoalPhase::Generating { .. } | GoalPhase::Closed) {
            return PluginTuiAction::None;
        }
        self.editor.handle_event(event, host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unblock_requires_exact_activation_and_explicit_decision() {
        assert_eq!(
            unblock_decision("activation-1 approve"),
            Ok(("activation-1", true))
        );
        assert_eq!(
            unblock_decision("activation-1 deny"),
            Ok(("activation-1", false))
        );
        for input in [
            "",
            "approve",
            "activation-1",
            "activation-1 yes",
            "activation-1 approve extra",
        ] {
            assert!(unblock_decision(input).is_err(), "{input}");
        }
    }

    #[test]
    fn generation_modal_keeps_top_border_after_short_title() {
        use bmux_tui::buffer::Buffer;
        use bmux_tui::frame::Frame;
        use bmux_tui::geometry::Point;

        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);
        let mut live = crate::goal_live::GenerationView::default();
        paint_generation(
            &mut live,
            area,
            &mut PaintCx::new(&mut Frame::new(&mut buffer)),
        );
        let panel = ModalFrame::new(
            ModalSizing::new(Size::new(40, 12), Size::new(100, 32), Insets::all(1)),
            ModalTheme::dark(Color::Cyan),
        )
        .panel_area(area);
        let row = buffer.row_symbols(panel.y).expect("modal top row");
        assert!(row.contains(live.title), "missing title: {row}");
        assert!(row.contains("─╮"), "title erased top border: {row}");
        assert_eq!(
            buffer
                .get(Point::new(panel.x, panel.y))
                .expect("left corner")
                .symbol,
            "╭"
        );
        assert_eq!(
            buffer
                .get(Point::new(panel.right() - 1, panel.y))
                .expect("right corner")
                .symbol,
            "╮"
        );
    }

    #[derive(Default)]
    struct Host {
        prerequisites: bool,
        tasks: Mutex<Vec<bcode_plugin_sdk::tui::PluginTask>>,
        starts: Mutex<Vec<PluginWorkflowStartRequest>>,
        documents: Mutex<Vec<bcode_session_models::SessionWorkingDocumentRequest>>,
        created_sessions: Mutex<usize>,
        configured_sessions: Mutex<Vec<SessionId>>,
        generations: Mutex<usize>,
    }
    impl PluginTuiHost for Host {
        fn workflow_delegation_preflight(
            &self,
            plugin_id: String,
        ) -> bcode_plugin_sdk::tui::PluginWorkflowDelegationPreflightFuture {
            let ready = self.prerequisites;
            Box::pin(async move {
                Ok(bcode_workflow::WorkflowDelegationPreflight {
                    version: 1,
                    plugin_id,
                    plugin_loaded: ready,
                    staging_configured: ready,
                    publication_configured: ready,
                })
            })
        }
        fn prepare_fresh_session(
            &self,
            existing: Option<SessionId>,
        ) -> bcode_plugin_sdk::tui::PluginCreateSessionFuture {
            let id = existing.map_or_else(
                || {
                    *self.created_sessions.lock().unwrap() += 1;
                    SessionId::new()
                },
                |id| {
                    self.configured_sessions.lock().unwrap().push(id);
                    id
                },
            );
            Box::pin(async move { Ok(id) })
        }
        fn spawn(&self, task: bcode_plugin_sdk::tui::PluginTask) {
            self.tasks.lock().unwrap().push(task);
        }
        fn spawn_blocking(&self, _: Box<dyn FnOnce() + Send + 'static>) {}
        fn request_redraw(&self) {}
        fn generate_observable_structured_output(
            &self,
            request: PluginStructuredGenerationRequest,
            _control: bcode_plugin_sdk::tui::PluginStructuredGenerationControl,
        ) -> bcode_plugin_sdk::tui::PluginStructuredGenerationFuture {
            self.generate_structured_output(request)
        }

        fn generate_structured_output(
            &self,
            request: PluginStructuredGenerationRequest,
        ) -> bcode_plugin_sdk::tui::PluginStructuredGenerationFuture {
            *self.generations.lock().unwrap() += 1;
            Box::pin(async move {
                Ok(bcode_plugin_sdk::tui::PluginStructuredGenerationResult {
                    output: serde_json::json!({"outcome":"ready", "clarification":"", "implementation_prompt":"Implement", "stop_condition":"Verify"}),
                    source: Some(bcode_session_models::SessionDerivationSourceSnapshot {
                        version: 1,
                        session_id: request.source_session_id.unwrap(),
                        generation: 42,
                        latest_sequence: 42,
                        title: None,
                        working_directory: "/repo".into(),
                    }),
                })
            })
        }
        fn session_working_document(
            &self,
            request: bcode_session_models::SessionWorkingDocumentRequest,
        ) -> bcode_plugin_sdk::tui::PluginWorkingDocumentFuture {
            self.documents.lock().unwrap().push(request.clone());
            Box::pin(async move {
                Ok(Some(bcode_session_models::SessionWorkingDocument {
                    session_id: request.session_id,
                    scope_id: request.scope_id,
                    path: "/state/session/progress.md".into(),
                    text: request.initial_text.unwrap_or_default(),
                }))
            })
        }
        fn associated_workflow(
            &self,
            _: bcode_plugin_sdk::tui::PluginWorkflowLookup,
        ) -> bcode_plugin_sdk::tui::PluginWorkflowLookupFuture {
            Box::pin(async { Ok(None) })
        }
        fn start_workflow(
            &self,
            request: PluginWorkflowStartRequest,
        ) -> bcode_plugin_sdk::tui::PluginWorkflowStartFuture {
            self.starts.lock().unwrap().push(request);
            Box::pin(async {
                Ok(bcode_plugin_sdk::tui::PluginWorkflowStartResponse {
                    run_id: "run".into(),
                    runtime_work_id: "workflow:run".into(),
                })
            })
        }
    }
    impl Host {
        async fn finish(&self) {
            let tasks = std::mem::take(&mut *self.tasks.lock().unwrap());
            for task in tasks {
                task.await;
            }
        }
    }

    #[test]
    fn goal_command_requires_explicit_positive_worker_allowance() {
        for args in [
            "--worker-attempts 0",
            "--worker-attempts -1",
            "--worker-attempts nope",
            "--collaborate --worker-attempts 0",
            "--collaborate --worker-attempts -1",
            "--collaborate --worker-attempts nope",
            "--collaborate extra",
        ] {
            assert!(!start_response(args).success);
        }
        for (args, expected) in [
            ("", None),
            ("--collaborate", None),
            ("--worker-attempts 4", Some(4)),
            ("--collaborate --worker-attempts 17", Some(17)),
        ] {
            let response = start_response(args);
            assert!(response.success);
            let CommandEffect::OpenPluginSurface { options, .. } = &response.effects[0] else {
                panic!("surface");
            };
            assert_eq!(options["worker_attempts"].as_u64(), expected);
            assert_eq!(options["collaboration"], args.starts_with("--collaborate"));
        }
    }

    #[test]
    fn setup_preflight_summary_points_to_remedies_without_claiming_execution() {
        for loaded in [false, true] {
            for staging in [false, true] {
                for publication in [false, true] {
                    let result = bcode_workflow::WorkflowDelegationPreflight {
                        version: 1,
                        plugin_id: "bcode.workflow".into(),
                        plugin_loaded: loaded,
                        staging_configured: staging,
                        publication_configured: publication,
                    };
                    let summary = preflight_setup_summary(&result);
                    assert!(!summary.contains('\n'));
                    assert!(summary.contains("/goal.preflight"));
                    assert_eq!(
                        summary.contains("present"),
                        loaded && staging && publication
                    );
                    assert_eq!(
                        summary.contains("missing"),
                        !(loaded && staging && publication)
                    );
                    let mut incompatible = result;
                    incompatible.version = 2;
                    assert!(preflight_setup_summary(&incompatible).contains("incompatible"));
                }
            }
        }
    }

    #[tokio::test]
    async fn setup_preflight_times_out_without_claiming_readiness() {
        let message = preflight_message_with_timeout(
            Box::pin(std::future::pending()),
            std::time::Duration::ZERO,
        )
        .await;
        assert!(message.contains("timed out"));
        assert!(message.contains("No authority changed"));
        let mut surface = GoalSurface::new(None);
        surface.preflight_pending = true;
        *surface.preflight_completion.lock().unwrap() = Some(message);
        assert!(surface.poll_preflight());
        assert!(!surface.preflight_pending);
        let host = Host::default();
        surface.check_delegation(&host);
        assert_eq!(host.tasks.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn setup_preflight_is_nonblocking_deduplicated_and_advisory() {
        let host = Host::default();
        let mut surface = GoalSurface::new(None);
        surface.check_delegation(&host);
        surface.check_delegation(&host);
        assert_eq!(host.tasks.lock().unwrap().len(), 1);
        assert!(surface.preflight_pending);
        host.finish().await;
        surface.poll(&host);
        assert!(!surface.preflight_pending);
        assert!(surface.editor.status.contains("missing"));
        assert_eq!(*host.generations.lock().unwrap(), 0);
        assert_eq!(*host.created_sessions.lock().unwrap(), 0);
        assert!(surface.phase == GoalPhase::Draft);
    }

    #[tokio::test]
    async fn collaboration_denial_precedes_session_and_generation_effects() {
        let host = Host::default();
        let mut surface = GoalSurface::new(None);
        surface.editor.collaboration = CollaborationMode::Requested;
        surface.editor.prompt = text_state("Coordinate a review");
        surface.generate(&host, false);
        surface.generate(&host, false);
        assert_eq!(host.tasks.lock().unwrap().len(), 1);
        assert_eq!(*host.created_sessions.lock().unwrap(), 0);
        assert_eq!(*host.generations.lock().unwrap(), 0);
        host.finish().await;
        surface.poll(&host);
        assert!(surface.phase == GoalPhase::Draft);
        assert!(surface.editor.status.contains("generation blocked"));
        assert_eq!(*host.created_sessions.lock().unwrap(), 0);
        assert_eq!(*host.generations.lock().unwrap(), 0);
        assert!(host.starts.lock().unwrap().is_empty());
        assert!(host.documents.lock().unwrap().is_empty());
        surface.generate(&host, true);
        assert_eq!(host.tasks.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn closed_collaboration_setup_discards_late_preflight() {
        let host = Host::default();
        let mut surface = GoalSurface::new(None);
        surface.editor.collaboration = CollaborationMode::Requested;
        surface.editor.prompt = text_state("Coordinate a review");
        surface.generate(&host, false);
        surface.phase = GoalPhase::Closed;
        *surface.generation_preflight.lock().unwrap() = Some(Ok(()));
        surface.poll(&host);
        assert!(surface.phase == GoalPhase::Closed);
        assert_eq!(*host.created_sessions.lock().unwrap(), 0);
        assert_eq!(*host.generations.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn checked_collaboration_can_generate_without_launching_plain_loop() {
        let host = Host::default();
        let mut surface = GoalSurface::new(Some(SessionId::new()));
        surface.editor.collaboration = CollaborationMode::Requested;
        surface.editor.prompt = text_state("Coordinate a review");
        surface.phase = GoalPhase::CheckingDelegation { review: true };
        *surface.generation_preflight.lock().unwrap() = Some(Ok(()));
        surface.poll(&host);
        assert_eq!(*host.generations.lock().unwrap(), 1);
        host.finish().await;
        surface.poll(&host);
        assert!(surface.phase == GoalPhase::Generated);
        assert!(host.starts.lock().unwrap().is_empty());
        assert!(host.documents.lock().unwrap().is_empty());
    }

    #[test]
    fn generation_preserves_explicit_collaboration_intent() {
        let request = generation_request("Review", "", true, CollaborationMode::Requested);
        assert!(
            request
                .system_prompt
                .contains("explicitly requested collaborating agents")
        );
        assert!(request.system_prompt.contains("does not grant tools"));
        let plain = generation_request("Review", "", true, CollaborationMode::Disabled);
        assert!(
            plain
                .system_prompt
                .contains("explicit requests for multiple agents or independent reviewers")
        );
        assert!(
            !plain
                .system_prompt
                .contains("explicitly requested collaborating agents")
        );
    }

    #[tokio::test]
    async fn collaboration_launch_is_gated_and_dispatches_coordinator_once() {
        for ready in [false, true] {
            let host = Host {
                prerequisites: ready,
                ..Host::default()
            };
            let mut surface = GoalSurface::new(Some(SessionId::new()));
            surface.editor.collaboration = CollaborationMode::Requested;
            surface.editor.worker_attempts = Some(12);
            surface.editor.prompt = text_state("Coordinate review");
            surface.editor.condition = text_state("Verified evidence");
            surface.editor.limit = text_state("2");
            surface.phase = GoalPhase::Generated;
            surface.editor.start();
            surface.editor.begin_pending_host_work(&host);
            assert!(host.starts.lock().unwrap().is_empty());
            assert!(host.documents.lock().unwrap().is_empty());
            for _ in 0..5 {
                host.finish().await;
                surface.poll(&host);
            }
            let starts = host.starts.lock().unwrap();
            assert_eq!(starts.len(), usize::from(ready));
            if let Some(start) = starts.first() {
                let configuration: bcode_workflow::WorkflowPromptConfiguration =
                    serde_json::from_value(
                        start.definition.nodes["loop.implementation"]
                            .configuration
                            .clone(),
                    )
                    .unwrap();
                assert_eq!(
                    configuration.execution_target,
                    bcode_workflow::PromptContextTarget::FreshIsolated
                );
                assert!(
                    configuration
                        .system_prompt
                        .contains("workflow.stage_task_group")
                );
                assert!(start.limits.node_execution_cap >= 12);
            }
            drop(starts);
            assert_eq!(host.documents.lock().unwrap().len(), usize::from(ready));
        }
    }

    #[tokio::test]
    async fn generated_collaboration_rechecks_prerequisites_before_launch() {
        for revoke in [false, true] {
            let mut host = Host {
                prerequisites: true,
                ..Host::default()
            };
            let mut surface = GoalSurface::new(Some(SessionId::new()));
            surface.editor.collaboration = CollaborationMode::Requested;
            surface.editor.worker_attempts = Some(12);
            surface.editor.prompt = text_state("Coordinate a verified review");
            surface.editor.limit = text_state("2");
            surface.generate(&host, false);
            assert_eq!(*host.generations.lock().unwrap(), 0);
            host.finish().await;
            surface.poll(&host);
            assert_eq!(*host.generations.lock().unwrap(), 1);
            // Change host policy after generation admission, before its completion
            // triggers the independent launch check.
            host.prerequisites = !revoke;
            host.finish().await;
            surface.poll(&host);
            assert!(host.starts.lock().unwrap().is_empty());
            assert!(host.documents.lock().unwrap().is_empty());
            for _ in 0..5 {
                host.finish().await;
                surface.poll(&host);
            }
            let starts = host.starts.lock().unwrap();
            assert_eq!(starts.len(), usize::from(!revoke));
            if let Some(start) = starts.first() {
                assert!(
                    start.definition.nodes["loop.implementation"].configuration["system_prompt"]
                        .as_str()
                        .unwrap()
                        .contains("workflow.stage_task_group")
                );
                assert_eq!(start.limits.cycle_cap, 2);
                assert!(start.limits.node_execution_cap > 12);
            }
            drop(starts);
            if revoke {
                assert!(surface.editor.status.contains("launch blocked"));
                assert!(surface.editor.failed_workflow_start.is_some());
            }
        }
    }

    #[tokio::test]
    async fn fresh_goal_creates_once_and_waits_for_attachment() {
        let host = Host::default();
        let mut surface = GoalSurface::new(None);
        surface.generate(&host, false);
        assert_eq!(*host.created_sessions.lock().unwrap(), 0);
        surface.editor.prompt = text_state("Implement a new feature");
        surface.generate(&host, false);
        surface.generate(&host, false);
        assert_eq!(*host.created_sessions.lock().unwrap(), 1);
        host.finish().await;
        surface.poll(&host);
        surface.poll(&host);
        host.finish().await;
        let PluginTuiAction::OpenSession { session_id } = surface.poll(&host) else {
            panic!("must attach first");
        };
        assert_eq!(*host.generations.lock().unwrap(), 0);
        surface.session_navigation_finished(session_id, Err("attach failed".into()));
        surface.generate(&host, false);
        host.finish().await;
        assert_eq!(
            surface.poll(&host),
            PluginTuiAction::OpenSession { session_id }
        );
        surface.session_navigation_finished(session_id, Ok(()));
        surface.poll(&host);
        assert_eq!(*host.created_sessions.lock().unwrap(), 1);
        assert_eq!(*host.generations.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn preflight_completion_preserves_pending_session_navigation() {
        let host = Host::default();
        let mut surface = GoalSurface::new(None);
        surface.editor.prompt = text_state("Implement a feature");
        surface.generate(&host, false);
        host.finish().await;
        surface.poll(&host);
        surface.poll(&host);
        surface.check_delegation(&host);
        host.finish().await;
        let PluginTuiAction::OpenSession { session_id } = surface.poll(&host) else {
            panic!("preflight redraw must not consume navigation");
        };
        assert!(!surface.preflight_pending);
        assert_eq!(*host.generations.lock().unwrap(), 0);
        surface.session_navigation_finished(session_id, Ok(()));
        surface.poll(&host);
        assert_eq!(*host.generations.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn fresh_loop_validates_before_creation_and_waits_for_attachment() {
        let host = Host::default();
        let mut surface = LoopSurface::new(None);
        surface.submit(&host);
        assert_eq!(*host.created_sessions.lock().unwrap(), 0);
        surface.prompt = text_state("implement");
        surface.condition = text_state("verified");
        surface.submit(&host);
        host.finish().await;
        surface.poll(&host);
        surface.poll(&host);
        host.finish().await;
        let PluginTuiAction::OpenSession { session_id } = surface.poll(&host) else {
            panic!("must attach");
        };
        assert!(host.starts.lock().unwrap().is_empty());
        surface.session_navigation_finished(session_id, Ok(()));
        surface.poll(&host);
        surface.poll(&host);
        assert_eq!(*host.created_sessions.lock().unwrap(), 1);
        assert_eq!(host.starts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn generation_starts_existing_loop_once_with_default_limit() {
        let host = Host::default();
        let mut surface = GoalSurface::new(Some(SessionId::new()));
        surface.editor.prompt = text_state("日本語 👩‍💻 e\u{301}");
        surface.generate(&host, false);
        surface.generate(&host, false);
        assert_eq!(*host.generations.lock().unwrap(), 1);
        host.finish().await;
        surface.poll(&host);
        surface.editor.start();
        surface.editor.begin_pending_host_work(&host);
        host.finish().await;
        surface.poll(&host);
        surface.poll(&host);
        let starts = host.starts.lock().unwrap();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].limits.cycle_cap, 20);
        let document_requests = host.documents.lock().unwrap();
        assert_eq!(document_requests.len(), 1);
        assert_eq!(
            starts[0].run_id.as_deref(),
            Some(document_requests[0].scope_id.as_str())
        );
        assert!(
            document_requests[0]
                .initial_text
                .as_ref()
                .unwrap()
                .contains("- [ ]")
        );
        assert!(
            starts[0].input["implementation_prompt"]
                .as_str()
                .unwrap()
                .contains("/state/session/progress.md")
        );
        drop(document_requests);
        drop(starts);
        assert_eq!(input_text(&surface.editor.limit), "20");
    }

    #[tokio::test]
    async fn generated_goal_failed_admission_preserves_prompts_and_retries_exact_request() {
        let host = Host::default();
        let mut surface = GoalSurface::new(Some(SessionId::new()));
        surface.editor.prompt = text_state("complete the goal");
        assert_eq!(surface.generate(&host, true), PluginTuiAction::Redraw);
        host.finish().await;
        surface.poll(&host);
        assert_eq!(surface.phase, GoalPhase::Generated);
        assert!(host.starts.lock().unwrap().is_empty());
        let implementation = input_text(&surface.editor.prompt);
        let condition = input_text(&surface.editor.condition);
        assert_eq!(surface.editor.submit(&host), PluginTuiAction::Redraw);
        surface.editor.begin_pending_host_work(&host);
        host.finish().await;
        surface.poll(&host);
        surface.editor.begin_pending_host_work(&host);
        host.finish().await;
        let first = host.starts.lock().unwrap()[0].clone();
        {
            let mut completions = surface.editor.completions.lock().unwrap();
            for completion in completions.iter_mut() {
                if let LoopSurfaceCompletion::WorkflowStart { result, .. } = completion {
                    *result = Err(bcode_plugin_sdk::tui::PluginTuiHostError::Internal(
                        "workflow storage blocked by another owner".into(),
                    ));
                }
            }
        }
        assert_eq!(surface.poll(&host), PluginTuiAction::Redraw);
        assert_eq!(surface.phase, GoalPhase::Generated);
        assert!(surface.editor.status.contains("another owner"));
        assert_eq!(input_text(&surface.editor.prompt), implementation);
        assert_eq!(input_text(&surface.editor.condition), condition);
        assert_eq!(surface.editor.submit(&host), PluginTuiAction::Redraw);
        surface.editor.begin_pending_host_work(&host);
        host.finish().await;
        assert!(matches!(surface.poll(&host), PluginTuiAction::Close { .. }));
        let starts = host.starts.lock().unwrap();
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[0], first);
        assert_eq!(starts[1], first);
        drop(starts);
    }

    #[tokio::test]
    async fn failed_preparation_retries_same_run_without_launching() {
        let host = Host::default();
        let mut surface = GoalSurface::new(Some(SessionId::new()));
        surface.editor.prompt = text_state("goal");
        surface.generate(&host, false);
        host.finish().await;
        surface.poll(&host);
        host.finish().await;
        {
            let mut completions = surface.editor.completions.lock().unwrap();
            for completion in completions.iter_mut() {
                if let LoopSurfaceCompletion::DocumentPrepared { result, .. } = completion {
                    *result = Err(bcode_plugin_sdk::tui::PluginTuiHostError::Internal(
                        "unavailable".into(),
                    ));
                }
            }
        }
        surface.poll(&host);
        assert!(host.starts.lock().unwrap().is_empty());
        surface.editor.start();
        surface.editor.begin_pending_host_work(&host);
        host.finish().await;
        surface.poll(&host);
        surface.poll(&host);
        let requests = host.documents.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].scope_id, requests[1].scope_id);
        drop(requests);
        assert_eq!(host.starts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn disabled_document_does_not_prepare_storage() {
        let host = Host::default();
        let mut surface = GoalSurface::new(Some(SessionId::new()));
        surface.editor.progress_document = None;
        surface.editor.prompt = text_state("goal");
        surface.generate(&host, false);
        host.finish().await;
        surface.poll(&host);
        assert!(host.documents.lock().unwrap().is_empty());
        assert_eq!(host.starts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn closing_during_document_preparation_never_launches() {
        let host = Host::default();
        let mut surface = GoalSurface::new(Some(SessionId::new()));
        surface.editor.prompt = text_state("goal");
        surface.generate(&host, false);
        host.finish().await;
        surface.poll(&host);
        surface.phase = GoalPhase::Closed;
        host.finish().await;
        surface.poll(&host);
        assert!(host.starts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn review_requires_explicit_launch_and_close_ignores_late_generation() {
        let host = Host::default();
        let mut surface = GoalSurface::new(Some(SessionId::new()));
        surface.editor.prompt = text_state("goal");
        surface.generate(&host, true);
        host.finish().await;
        surface.poll(&host);
        assert!(host.starts.lock().unwrap().is_empty());
        surface.editor.prompt = text_state("edited implementation");
        let area = Rect::new(0, 0, 100, 32);
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        surface.render(area, &mut PaintCx::new(&mut Frame::new(&mut buffer)));
        assert!(surface.editor.review_area.height > 0);
        let position = bmux_tui::geometry::Point::new(
            surface.editor.review_area.x,
            surface.editor.review_area.y,
        );
        for kind in [
            MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
            MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
        ] {
            surface.handle_event(
                &Event::Mouse(bmux_tui::event::MouseEvent::new(kind, position)),
                &host,
            );
        }
        host.finish().await;
        surface.poll(&host);
        surface.poll(&host);
        assert_eq!(host.starts.lock().unwrap().len(), 1);

        let host = Host::default();
        let mut surface = GoalSurface::new(Some(SessionId::new()));
        surface.editor.prompt = text_state("goal");
        surface.generate(&host, false);
        surface.phase = GoalPhase::Closed;
        host.finish().await;
        surface.poll(&host);
        assert!(host.starts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn clarification_never_launches_or_creates_document() {
        let host = Host::default();
        let mut surface = GoalSurface::new(Some(SessionId::new()));
        surface.editor.prompt = text_state("finish that");
        surface.generate(&host, false);
        host.finish().await;
        {
            let mut completion = surface.completion.lock().unwrap();
            completion.as_mut().unwrap().as_mut().unwrap().output = serde_json::json!({"outcome":"clarification_required", "clarification":"Which migration?", "implementation_prompt":"Not ready", "stop_condition":"Not ready"});
        }
        surface.poll(&host);
        assert!(surface.editor.status.contains("Which migration?"));
        assert!(host.starts.lock().unwrap().is_empty());
        assert!(host.documents.lock().unwrap().is_empty());
        assert_eq!(input_text(&surface.editor.prompt), "finish that");
    }

    #[tokio::test]
    async fn invalid_generation_preserves_draft_and_can_retry() {
        let host = Host::default();
        let mut surface = GoalSurface::new(Some(SessionId::new()));
        surface.editor.prompt = text_state("original goal");
        surface.generate(&host, false);
        host.finish().await;
        *surface.completion.lock().unwrap() = Some(Ok(
            bcode_plugin_sdk::tui::PluginStructuredGenerationResult {
                output: serde_json::json!({}),
                source: None,
            },
        ));
        surface.poll(&host);
        assert_eq!(input_text(&surface.editor.prompt), "original goal");
        assert!(host.starts.lock().unwrap().is_empty());
        surface.generate(&host, true);
        assert_eq!(*host.generations.lock().unwrap(), 2);
    }

    #[test]
    fn goal_layout_clips_inputs_when_resized() {
        let mut surface = GoalSurface::new(Some(SessionId::new()));
        surface.editor.prompt = text_state("日本語 👩‍💻 e\u{301}");
        for (width, height) in [(100, 32), (1, 1), (0, 0), (8, 5), (80, 28)] {
            let area = Rect::new(0, 0, width, height);
            let mut buffer = bmux_tui::buffer::Buffer::empty(area);
            surface.render(area, &mut PaintCx::new(&mut Frame::new(&mut buffer)));
            for hit in [
                surface.editor.prompt_area,
                surface.editor.condition_area,
                surface.editor.limit_area,
                surface.editor.progress_document_area,
                surface.editor.review_area,
            ] {
                assert_eq!(hit, hit.intersection(area));
            }
        }
    }

    #[test]
    fn live_generation_survives_resize_and_hiding_does_not_cancel() {
        let key = |key| {
            Event::Key(bmux_keyboard::KeyStroke {
                key,
                modifiers: bmux_keyboard::Modifiers::default(),
            })
        };
        let host = Host::default();
        let mut surface = GoalSurface::new(Some(SessionId::new()));
        let mut live = crate::goal_live::GenerationView::default();
        live.lines = vec![bmux_tui::prelude::Line::from(
            "日本語 👩‍💻 e\u{301}".repeat(40),
        )];
        surface.live = Some(live);
        for (width, height) in [(100, 32), (1, 1), (0, 0), (8, 5), (80, 28)] {
            let area = Rect::new(0, 0, width, height);
            let mut buffer = bmux_tui::buffer::Buffer::empty(area);
            surface.render(area, &mut PaintCx::new(&mut Frame::new(&mut buffer)));
            if let Some((hit, _)) = &surface.live.as_ref().unwrap().layout {
                assert_eq!(*hit, hit.intersection(area));
            }
        }
        surface.handle_event(&key(KeyCode::Tab), &host);
        assert!(!surface.live.as_ref().unwrap().control.is_cancelled());
        surface.handle_event(&key(KeyCode::Escape), &host);
        assert!(surface.live.as_ref().unwrap().control.is_cancelled());
    }

    #[test]
    fn goal_body_controls_accept_clicks() {
        use bmux_tui::event::{MouseButton, MouseEvent};
        use bmux_tui::geometry::Point;

        let host = Host::default();
        let mut surface = GoalSurface::new(Some(SessionId::new()));
        surface.editor.prompt = text_state("Implement the goal");
        let area = Rect::new(0, 0, 100, 32);
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        surface.render(area, &mut PaintCx::new(&mut Frame::new(&mut buffer)));
        let click = |rect: Rect| {
            Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(rect.x, rect.y),
            ))
        };
        let release = |rect: Rect| {
            Event::Mouse(MouseEvent::new(
                MouseEventKind::Up(MouseButton::Left),
                Point::new(rect.x, rect.y),
            ))
        };
        let checkbox = surface.editor.progress_document_area;
        assert!(checkbox.height > 0);
        let outside = Rect::new(0, 0, 1, 1);
        surface.handle_event(&click(checkbox), &host);
        surface.handle_event(&release(outside), &host);
        assert!(surface.editor.progress_document.is_some());
        let right_click = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Right),
            Point::new(checkbox.x, checkbox.y),
        ));
        surface.handle_event(&right_click, &host);
        surface.handle_event(&release(checkbox), &host);
        assert!(surface.editor.progress_document.is_some());
        surface.handle_event(&click(checkbox), &host);
        assert!(surface.editor.progress_document.is_some());
        surface.handle_event(&release(checkbox), &host);
        assert!(surface.editor.progress_document.is_none());
        surface.handle_event(&click(checkbox), &host);
        surface.handle_event(&release(checkbox), &host);
        assert!(surface.editor.progress_document.is_some());
        surface.handle_event(&click(surface.editor.review_area), &host);
        surface.handle_event(&release(surface.editor.review_area), &host);
        assert!(matches!(
            surface.phase,
            GoalPhase::Generating { review: true }
        ));
        assert!(host.starts.lock().unwrap().is_empty());
        surface.handle_event(&click(checkbox), &host);
        assert!(surface.editor.progress_document.is_some());
    }

    #[test]
    fn goal_options_follow_keyboard_focus() {
        let host = Host::default();
        let mut surface = GoalSurface::new(Some(SessionId::new()));
        surface.editor.prompt = text_state("Implement the goal");
        let key = |key, shift| {
            Event::Key(bmux_keyboard::KeyStroke {
                key,
                modifiers: bmux_keyboard::Modifiers {
                    shift,
                    ..Default::default()
                },
            })
        };
        surface.handle_event(&key(KeyCode::Tab, true), &host);
        assert!(surface.editor.goal_option_focus == Some(GoalOption::Review));
        surface.handle_event(&key(KeyCode::Tab, true), &host);
        assert!(surface.editor.goal_option_focus == Some(GoalOption::Progress));
        surface.handle_event(&key(KeyCode::Char(' '), false), &host);
        assert!(surface.editor.progress_document.is_none());
        surface.handle_event(&key(KeyCode::Tab, true), &host);
        assert!(surface.editor.goal_option_focus.is_none());
        assert_eq!(surface.editor.field, Field::Evaluation);
        surface.handle_event(&key(KeyCode::Tab, false), &host);
        surface.handle_event(&key(KeyCode::Tab, false), &host);
        surface.handle_event(&key(KeyCode::Enter, false), &host);
        assert!(matches!(
            surface.phase,
            GoalPhase::Generating { review: true }
        ));
    }

    #[test]
    fn goal_controls_share_loop_handlers_and_require_sessions() {
        for (goal, ordinary) in [
            ("goal.status", STATUS_COMMAND),
            ("goal.pause", PAUSE_COMMAND),
            ("goal.resume", RESUME_COMMAND),
            ("goal.stop", STOP_COMMAND),
            ("goal.detach", DETACH_COMMAND),
        ] {
            let request = InvokeCommandRequest {
                command_id: goal.into(),
                args: std::collections::BTreeMap::new(),
                context: None,
            };
            let expected = InvokeCommandRequest {
                command_id: ordinary.into(),
                ..request.clone()
            };
            let actual = serde_json::to_value(command_response(&request)).unwrap();
            assert_eq!(
                actual,
                serde_json::to_value(command_response(&expected)).unwrap()
            );
            assert!(commands().iter().any(|command| command.id == goal
                && command.session == bcode_command::CommandSessionRequirement::Required));
        }
    }

    #[test]
    fn generation_preserves_source_and_validates_output() {
        let value =
            serde_json::json!({"implementation_prompt":"Implement", "stop_condition":"Verify"});
        let input = decode_prompts(value.clone(), "日本語 👩‍💻", "Keep scope", 20).unwrap();
        assert!(input.implementation_prompt.contains("日本語 👩‍💻"));
        assert!(input.stop_condition.contains("Keep scope"));
        assert_eq!(input.max_iterations, 20);
        assert!(decode_prompts(value.clone(), "goal", "", 0).is_err());
        assert!(decode_prompts(value, &"x".repeat(MAX_PROMPT_BYTES), "", 20).is_err());
        assert!(
            decode_prompts(
                serde_json::json!({"implementation_prompt":" ", "stop_condition":"yes"}),
                "goal",
                "",
                20
            )
            .is_err()
        );
        assert!(decode_prompts(serde_json::json!({"implementation_prompt":"x", "stop_condition":"y", "permissions":"all"}), "goal", "", 20).is_err());
    }
}
