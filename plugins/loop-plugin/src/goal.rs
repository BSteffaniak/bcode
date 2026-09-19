//! Goal setup is a tool-free prompt generator, not an execution engine.
use super::*;
use bcode_plugin_sdk::tui::PluginStructuredGenerationRequest;

pub const SURFACE_KIND: &str = "goal.start";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GeneratedGoalPrompts {
    implementation_prompt: String,
    stop_condition: String,
}

fn generation_request(objective: &str, guidance: &str) -> PluginStructuredGenerationRequest {
    PluginStructuredGenerationRequest {
        session_name: "Goal prompt generation".into(),
        system_prompt: [
            include_str!("../prompts/goal-generation.md"),
            include_str!("../prompts/goal-iteration-guidance.md"),
            include_str!("../prompts/goal-stop-condition-guidance.md"),
        ]
        .join("\n\n"),
        prompt: serde_json::json!({"objective": objective, "additional_guidance": guidance})
            .to_string(),
        output_name: "goal_loop_prompts".into(),
        output_schema: serde_json::json!({
            "type": "object", "additionalProperties": false,
            "required": ["implementation_prompt", "stop_condition"],
            "properties": {
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
            Ok(Box::new(GoalSurface::new(session)) as BoxedPluginTuiSurface)
        })
    }
}

type GenerationResult = Result<serde_json::Value, String>;
#[derive(Clone, Copy, PartialEq, Eq)]
enum GoalPhase {
    Draft,
    Generating { review: bool },
    Generated,
    Closed,
}

struct GoalSurface {
    editor: LoopSurface,
    phase: GoalPhase,
    source: Option<(String, String, u64)>,
    completion: Arc<Mutex<Option<GenerationResult>>>,
}

impl GoalSurface {
    fn new(session: Option<SessionId>) -> Self {
        let mut editor = LoopSurface::new(session);
        editor.setup_kind = SetupKind::Goal;
        editor.limit = text_state("");
        Self {
            editor,
            phase: GoalPhase::Draft,
            source: None,
            completion: Arc::default(),
        }
    }

    fn generate(&mut self, host: &dyn PluginTuiHost, review: bool) -> PluginTuiAction {
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
            if self.editor.session_id.is_none() {
                return Err("an active persisted session is required".into());
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
        self.source = Some((objective.clone(), guidance.clone(), limit));
        self.phase = GoalPhase::Generating { review };
        self.editor.status = "Generating loop prompts… Esc closes without launching".into();
        let future = host.generate_structured_output(generation_request(&objective, &guidance));
        let completion = Arc::clone(&self.completion);
        host.spawn(Box::pin(async move {
            *completion.lock().expect("goal generation completion") =
                Some(future.await.map_err(|e| e.to_string()));
        }));
        PluginTuiAction::Redraw
    }
}

impl PluginTuiSurface for GoalSurface {
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
        self.editor.render(area, frame);
    }
    fn render_with_theme(
        &mut self,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
        theme: Option<&PluginTuiTheme>,
    ) {
        self.editor.render_with_theme(area, frame, theme);
    }
    fn poll(&mut self, host: &dyn PluginTuiHost) -> PluginTuiAction {
        if self.phase == GoalPhase::Closed {
            return PluginTuiAction::None;
        }
        let action = self.editor.poll(host);
        let result = self
            .completion
            .lock()
            .expect("goal generation completion")
            .take();
        let Some(result) = result else {
            return action;
        };
        let review = matches!(self.phase, GoalPhase::Generating { review: true });
        self.phase = GoalPhase::Draft;
        let Some((objective, guidance, limit)) = self.source.take() else {
            return action;
        };
        match result.and_then(|value| decode_prompts(value, &objective, &guidance, limit)) {
            Ok(input) => {
                self.editor.prompt = text_state(&input.implementation_prompt);
                self.editor.condition = text_state(&input.stop_condition);
                self.editor.limit = text_state(&limit.to_string());
                self.editor.setup_kind = SetupKind::Loop;
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
        if let Event::Key(stroke) = event {
            if stroke.key == KeyCode::Escape && stroke.modifiers.is_empty() {
                self.phase = GoalPhase::Closed;
                return PluginTuiAction::Close { outcome: None };
            }
            if self.phase == GoalPhase::Draft {
                if stroke.modifiers.ctrl && stroke.key == KeyCode::Char('r') {
                    return self.generate(host, true);
                }
                if stroke.key == KeyCode::Enter
                    && (stroke.modifiers.ctrl || self.editor.field == Field::Limit)
                {
                    return self.generate(host, false);
                }
            }
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

    #[derive(Default)]
    struct Host {
        tasks: Mutex<Vec<bcode_plugin_sdk::tui::PluginTask>>,
        starts: Mutex<Vec<PluginWorkflowStartRequest>>,
        generations: Mutex<usize>,
    }
    impl PluginTuiHost for Host {
        fn spawn(&self, task: bcode_plugin_sdk::tui::PluginTask) {
            self.tasks.lock().unwrap().push(task);
        }
        fn spawn_blocking(&self, _: Box<dyn FnOnce() + Send + 'static>) {}
        fn request_redraw(&self) {}
        fn generate_structured_output(
            &self,
            _: PluginStructuredGenerationRequest,
        ) -> bcode_plugin_sdk::tui::PluginStructuredGenerationFuture {
            *self.generations.lock().unwrap() += 1;
            Box::pin(async {
                Ok(
                    serde_json::json!({"implementation_prompt":"Implement", "stop_condition":"Verify"}),
                )
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
        let starts = host.starts.lock().unwrap();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].limits.cycle_cap, 20);
        drop(starts);
        assert_eq!(input_text(&surface.editor.limit), "20");
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
        surface.editor.start();
        surface.editor.begin_pending_host_work(&host);
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
    async fn invalid_generation_preserves_draft_and_can_retry() {
        let host = Host::default();
        let mut surface = GoalSurface::new(Some(SessionId::new()));
        surface.editor.prompt = text_state("original goal");
        surface.generate(&host, false);
        host.finish().await;
        *surface.completion.lock().unwrap() = Some(Ok(serde_json::json!({})));
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
            ] {
                assert_eq!(hit, hit.intersection(area));
            }
        }
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
