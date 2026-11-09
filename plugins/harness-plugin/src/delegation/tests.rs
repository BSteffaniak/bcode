//! Behavior of the simplified delegation shapes against a fake managed application.
use super::*;
use std::cell::RefCell;

const PARENT: &str = "11111111-1111-4111-8111-111111111111";

fn instance() -> HarnessInstance {
    serde_json::from_value(
        json!({"instance_id":"claude","plugin_id":"bcode.claude-code",
        "executable":"/explicit/claude","config_namespace":"/isolated",
        "auth":{"provider_id":"claude-code","profile":"native","credential_namespace":"/isolated",
        "custody":"native_managed"}}),
    )
    .unwrap()
}

fn scope() -> InvocationScope {
    InvocationScope {
        parent_session_id: PARENT.into(),
        working_directory: "/workspace".into(),
    }
}

#[derive(Default)]
struct Child {
    instance: Option<HarnessInstance>,
    open_id: String,
    target: Option<HarnessSessionTarget>,
    submissions: BTreeMap<String, String>,
    responses: BTreeMap<String, String>,
    observations: Vec<HarnessObservation>,
    running: bool,
    pending_approval: Option<String>,
}

/// Fake managed application with canonical retained-identity semantics: exact retries return
/// recorded evidence, conflicting reuse fails, and every native effect is counted.
#[derive(Default)]
struct FakeApplication {
    children: RefCell<BTreeMap<String, Child>>,
    effects: RefCell<Vec<String>>,
    calls: RefCell<Vec<String>>,
    lose_next_submit_ack: RefCell<bool>,
    require_approval: RefCell<bool>,
}

impl FakeApplication {
    fn response(request: &HarnessRequest, kind: &Value) -> Value {
        json!({"schema_version":1,"request_id":request.request_id,
            "operation":request.kind.operation(),"kind":kind})
    }

    fn observation(child: &mut Child, session: &str, kind: HarnessObservationKind) {
        let index = child.observations.len();
        child.observations.push(HarnessObservation {
            observation_id: format!("observation-{index}"),
            session_id: session.into(),
            generation: 1,
            native_correlations: Vec::new(),
            completeness: bcode_harness_models::HarnessCompleteness::Complete,
            kind,
        });
    }

    fn finish(child: &mut Child, session: &str, submission: &str, attempt: &str, text: &str) {
        let reply = format!("reply {text}");
        Self::observation(
            child,
            session,
            HarnessObservationKind::Text {
                text: reply.clone(),
                segment_id: None,
            },
        );
        Self::observation(
            child,
            session,
            HarnessObservationKind::Execution {
                event: HarnessExecutionEvent::SubmissionTerminal {
                    attempt: bcode_harness_models::HarnessAttemptIdentity {
                        submission_id: submission.into(),
                        attempt_id: attempt.into(),
                    },
                    native_turn_id: None,
                    native_frame_id: Some("frame".into()),
                    disposition: HarnessTerminalDisposition::Completed,
                    output: Some(reply),
                },
            },
        );
        child.running = false;
    }

    #[allow(clippy::too_many_lines)] // One fake canonical application dispatcher.
    fn harness(&self, operation: &str, payload: &Value) -> Result<Value, CallFailure> {
        let request: HarnessRequest = serde_json::from_value(payload["harness"].clone()).unwrap();
        let mut children = self.children.borrow_mut();
        match (operation, &request.kind) {
            ("start", HarnessRequestKind::Open(open)) => {
                assert_eq!(payload["parent_session_id"], PARENT);
                let child = children.entry(open.session_id.clone()).or_default();
                if child.target.is_some() && child.open_id != open.open_id {
                    return Ok(Self::response(
                        &request,
                        &json!({"kind":"failure","response":"conflict"}),
                    ));
                }
                if child.target.is_none() {
                    self.effects
                        .borrow_mut()
                        .push(format!("open:{}", open.session_id));
                    child.instance = Some(request.instance.clone());
                    child.open_id.clone_from(&open.open_id);
                    child.target = Some(HarnessSessionTarget::Prepared(
                        bcode_harness_models::HarnessPreparedSession {
                            open_id: open.open_id.clone(),
                            session_id: open.session_id.clone(),
                            instance_id: request.instance.instance_id.clone(),
                            generation: 1,
                            native_version: "1.0.0".into(),
                            requested_native_session_id: None,
                        },
                    ));
                }
                let HarnessSessionTarget::Prepared(prepared) = child.target.clone().unwrap() else {
                    unreachable!()
                };
                Ok(Self::response(
                    &request,
                    &json!({"kind":"prepared","response":prepared}),
                ))
            }
            ("followup", HarnessRequestKind::Submit { target, submission }) => {
                let child = children
                    .get_mut(&submission.session_id)
                    .ok_or_else(|| CallFailure::Host("managed_session_stale_owner".into()))?;
                if let Some(text) = child.submissions.get(&submission.submission_id) {
                    let kind = if *text == submission.text {
                        json!({"kind":"receipt","response":{"submission_id":submission.submission_id,
                            "attempt_id":submission.attempt_id,"session_id":submission.session_id,
                            "generation":1,"state":"accepted","native_request_id":null}})
                    } else {
                        json!({"kind":"failure","response":"conflict"})
                    };
                    return Ok(Self::response(&request, &kind));
                }
                let current = child.target.clone().unwrap();
                if *target != current || child.running {
                    return Ok(Self::response(
                        &request,
                        &json!({"kind":"failure","response":"conflict"}),
                    ));
                }
                self.effects
                    .borrow_mut()
                    .push(format!("submit:{}", submission.text));
                child
                    .submissions
                    .insert(submission.submission_id.clone(), submission.text.clone());
                child.running = true;
                if let HarnessSessionTarget::Prepared(prepared) = &current {
                    let binding = bcode_harness_models::HarnessSessionBinding {
                        schema_version: 1,
                        session_id: prepared.session_id.clone(),
                        instance_id: prepared.instance_id.clone(),
                        native_session_id: "native".into(),
                        native_version: "1.0.0".into(),
                        generation: 1,
                    };
                    child.target = Some(HarnessSessionTarget::Confirmed(binding.clone()));
                    Self::observation(
                        child,
                        &submission.session_id,
                        HarnessObservationKind::BindingConfirmed {
                            open_id: prepared.open_id.clone(),
                            binding,
                        },
                    );
                }
                Self::observation(
                    child,
                    &submission.session_id,
                    HarnessObservationKind::SubmissionAccepted {
                        submission_id: submission.submission_id.clone(),
                        attempt_id: submission.attempt_id.clone(),
                        native_turn_id: "turn".into(),
                    },
                );
                if *self.require_approval.borrow() {
                    let request_id = format!("approval-{}", child.submissions.len());
                    child.pending_approval = Some(request_id.clone());
                    Self::observation(
                        child,
                        &submission.session_id,
                        HarnessObservationKind::Interaction {
                            interaction: HarnessInteraction {
                                request_id,
                                session_id: submission.session_id.clone(),
                                generation: 1,
                                kind: HarnessInteractionKind::Approval,
                                prompt: "Allow Read?".into(),
                                scope: "tool".into(),
                                choices: Vec::new(),
                                tool_name: Some("Read".into()),
                                input_preview: Some("{\"file_path\":\"notes\"}".into()),
                            },
                        },
                    );
                } else {
                    Self::finish(
                        child,
                        &submission.session_id,
                        &submission.submission_id,
                        &submission.attempt_id,
                        &submission.text,
                    );
                }
                if std::mem::take(&mut *self.lose_next_submit_ack.borrow_mut()) {
                    return Err(CallFailure::Unknown);
                }
                Ok(Self::response(
                    &request,
                    &json!({"kind":"receipt","response":{
                    "submission_id":submission.submission_id,"attempt_id":submission.attempt_id,
                    "session_id":submission.session_id,"generation":1,"state":"accepted",
                    "native_request_id":null}}),
                ))
            }
            ("respond", HarnessRequestKind::Respond { target, response }) => {
                let child = children.get_mut(&response.session_id).unwrap();
                if let Some(value) = child.responses.get(&response.response_id) {
                    assert_eq!(*value, serde_json::to_string(&response.value).unwrap());
                    return Ok(Self::response(
                        &request,
                        &json!({"kind":"acknowledged","response":{"mutation_id":response.response_id}}),
                    ));
                }
                if Some(target) != child.target.as_ref()
                    || child.pending_approval.as_deref() != Some(response.request_id.as_str())
                {
                    return Ok(Self::response(
                        &request,
                        &json!({"kind":"failure","response":"conflict"}),
                    ));
                }
                self.effects
                    .borrow_mut()
                    .push(format!("respond:{}", response.request_id));
                child.responses.insert(
                    response.response_id.clone(),
                    serde_json::to_string(&response.value).unwrap(),
                );
                child.pending_approval = None;
                let session = response.session_id.clone();
                let (submission, text) = child
                    .submissions
                    .iter()
                    .next_back()
                    .map(|(id, text)| (id.clone(), text.clone()))
                    .unwrap();
                Self::observation(
                    child,
                    &session,
                    HarnessObservationKind::ToolResult {
                        native_tool_id: "tool".into(),
                        output: "notes".into(),
                        is_error: false,
                        truncated: false,
                    },
                );
                Self::finish(child, &session, &submission, "attempt", &text);
                Ok(Self::response(
                    &request,
                    &json!({"kind":"acknowledged","response":{"mutation_id":response.response_id}}),
                ))
            }
            ("interrupt", HarnessRequestKind::Interrupt(control)) => {
                self.effects.borrow_mut().push("interrupt".into());
                Ok(Self::response(
                    &request,
                    &json!({"kind":"acknowledged","response":{"mutation_id":control.mutation_id}}),
                ))
            }
            _ => panic!("unexpected managed operation {operation}"),
        }
    }
}

impl ManagedApplication for FakeApplication {
    fn call(&self, operation: &str, payload: Value) -> Result<Value, CallFailure> {
        self.calls.borrow_mut().push(operation.to_owned());
        match operation {
            "binding" => {
                let request: ManagedDelegationBindingRequest =
                    serde_json::from_value(payload).unwrap();
                let children = self.children.borrow();
                let binding = children.get(&request.session_id).and_then(|child| {
                    Some(ManagedDelegationBinding {
                        session_id: request.session_id.clone(),
                        instance: child.instance.clone()?,
                        target: child.target.clone()?,
                    })
                });
                Ok(json!({"binding": binding}))
            }
            "history" => {
                let request: ManagedDelegationHistoryRequest =
                    serde_json::from_value(payload).unwrap();
                let children = self.children.borrow();
                let child = children
                    .get(&request.session_id)
                    .ok_or_else(|| CallFailure::Host("managed_session_stale_owner".into()))?;
                let start = request
                    .after
                    .as_deref()
                    .map_or(0, |cursor| cursor.parse::<usize>().unwrap());
                let observations = child
                    .observations
                    .iter()
                    .skip(start)
                    .take(usize::from(request.limit))
                    .cloned()
                    .collect::<Vec<_>>();
                // Like canonical history, the end of history keeps the cursor unchanged.
                let end = start + observations.len();
                Ok(
                    json!({"observations":observations,"next_after":end.to_string(),
                    "completeness":"complete","status":{"connectivity":"unknown","activity":"unknown",
                    "attention":"unknown","continuation":"unknown","completeness":"unknown"}}),
                )
            }
            operation => self.harness(operation, &payload),
        }
    }
}

fn delegate_input(prompt: &str) -> SimpleInvocation {
    SimpleInvocation::parse(DELEGATE_TOOL, &json!({"prompt": prompt}))
        .unwrap()
        .unwrap()
}

fn run(app: &FakeApplication, invocation_id: &str, invocation: &SimpleInvocation) -> Outcome {
    execute(app, &scope(), invocation_id, invocation, Some(&instance()))
}

fn result_of(app: &FakeApplication, session: &str, after: Option<&str>) -> Value {
    let mut arguments = json!({"session_id": session});
    if let Some(after) = after {
        arguments["after"] = json!(after);
    }
    let invocation = SimpleInvocation::parse(RESULT_TOOL, &arguments)
        .unwrap()
        .unwrap();
    let outcome = run(app, "poll", &invocation);
    assert!(!outcome.is_error, "{}", outcome.output);
    outcome.output
}

#[test]
fn delegate_poll_and_continue_use_only_session_ids_and_prompts() {
    let app = FakeApplication::default();
    let started = run(&app, "call-1", &delegate_input("first"));
    assert!(!started.is_error, "{}", started.output);
    assert_eq!(started.output["status"], "submitted");
    assert_eq!(started.output["instance_id"], "claude");
    let session = started.output["session_id"].as_str().unwrap().to_owned();
    assert!(session.parse::<SessionId>().is_ok());

    let polled = result_of(&app, &session, None);
    assert_eq!(polled["activity"], "settled");
    assert_eq!(polled["latest_result"], "reply first");
    assert_eq!(polled["pending"], json!([]));
    let cursor = polled["cursor"].as_str().unwrap().to_owned();

    let next = SimpleInvocation::parse(
        CONTINUE_TOOL,
        &json!({"session_id":session,"prompt":"second"}),
    )
    .unwrap()
    .unwrap();
    let continued = run(&app, "call-2", &next);
    assert!(!continued.is_error, "{}", continued.output);
    assert_eq!(continued.output["status"], "submitted");
    // Continuation targets the binding confirmed by the first turn, not the prepared target.
    let polled = result_of(&app, &session, Some(&cursor));
    assert_eq!(polled["activity"], "settled");
    assert_eq!(polled["latest_result"], "reply second");
    assert_eq!(
        *app.effects.borrow(),
        vec![
            format!("open:{session}"),
            "submit:first".into(),
            "submit:second".into()
        ]
    );
}

#[test]
fn lost_submission_ack_is_reconciled_by_the_same_invocation_never_replayed() {
    let app = FakeApplication::default();
    *app.lose_next_submit_ack.borrow_mut() = true;
    let lost = run(&app, "call-1", &delegate_input("first"));
    assert!(lost.is_error);
    assert_eq!(lost.output["status"], "uncertain");
    let session = lost.output["session_id"].as_str().unwrap().to_owned();

    // The host re-dispatching the identical invocation derives identical identities. The
    // binding is already confirmed, so the first turn is reported, never re-sent.
    let retried = run(&app, "call-1", &delegate_input("first"));
    assert!(!retried.is_error, "{}", retried.output);
    assert_eq!(retried.output["status"], "already_submitted");
    assert_eq!(retried.output["session_id"], session);
    assert_eq!(
        retried.output["submission_id"],
        DelegationIdentities::derive(&scope(), "call-1", "claude", "first").submission_id
    );
    assert_eq!(
        app.effects
            .borrow()
            .iter()
            .filter(|effect| effect.starts_with("submit:"))
            .count(),
        1
    );
    assert_eq!(
        app.effects
            .borrow()
            .iter()
            .filter(|effect| effect.starts_with("open:"))
            .count(),
        1
    );
}

#[test]
fn exact_retry_before_confirmation_resubmits_identical_retained_identities() {
    let app = FakeApplication::default();
    let first = run(&app, "call-1", &delegate_input("first"));
    let session = first.output["session_id"].as_str().unwrap().to_owned();
    // Simulate a target that is still prepared (no native confirmation yet).
    {
        let mut children = app.children.borrow_mut();
        let child = children.get_mut(&session).unwrap();
        let HarnessSessionTarget::Confirmed(binding) = child.target.clone().unwrap() else {
            panic!("fake confirms on first submission")
        };
        child.target = Some(HarnessSessionTarget::Prepared(
            bcode_harness_models::HarnessPreparedSession {
                open_id: child.open_id.clone(),
                session_id: binding.session_id,
                instance_id: binding.instance_id,
                generation: 1,
                native_version: "1.0.0".into(),
                requested_native_session_id: None,
            },
        ));
    }
    let retried = run(&app, "call-1", &delegate_input("first"));
    assert!(!retried.is_error, "{}", retried.output);
    assert_eq!(retried.output["status"], "submitted");
    assert_eq!(
        retried.output["submission_id"],
        first.output["submission_id"]
    );
    assert_eq!(
        app.effects.borrow().len(),
        2,
        "no second open or submission effect"
    );
}

#[test]
fn distinct_invocations_create_distinct_children_and_identities() {
    let app = FakeApplication::default();
    let one = run(&app, "call-1", &delegate_input("same task"));
    let two = run(&app, "call-2", &delegate_input("same task"));
    assert_ne!(one.output["session_id"], two.output["session_id"]);
    assert_ne!(one.output["submission_id"], two.output["submission_id"]);
    let other_parent = InvocationScope {
        parent_session_id: "33333333-3333-4333-8333-333333333333".into(),
        working_directory: "/workspace".into(),
    };
    assert_ne!(
        DelegationIdentities::derive(&scope(), "call-1", "claude", "same task"),
        DelegationIdentities::derive(&other_parent, "call-1", "claude", "same task")
    );
    assert!(
        DelegationIdentities::derive(&scope(), "call-1", "claude", "same task")
            .attempt_id
            .parse::<uuid::Uuid>()
            .is_ok()
    );
}

#[test]
fn pending_approval_is_reported_and_answered_through_the_simple_respond_shape() {
    let app = FakeApplication::default();
    *app.require_approval.borrow_mut() = true;
    let started = run(&app, "call-1", &delegate_input("first"));
    let session = started.output["session_id"].as_str().unwrap().to_owned();
    let polled = result_of(&app, &session, None);
    assert_eq!(polled["activity"], "needs_attention");
    let pending = polled["pending"].as_array().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["kind"], "approval");
    assert_eq!(pending[0]["tool_name"], "Read");
    let request_id = pending[0]["request_id"].as_str().unwrap();

    let respond = SimpleInvocation::parse(
        "harness.respond",
        &json!({"session_id":session,"request_id":request_id,"approve":true}),
    )
    .unwrap()
    .unwrap();
    let answered = run(&app, "call-2", &respond);
    assert!(!answered.is_error, "{}", answered.output);
    assert_eq!(answered.output["status"], "acknowledged");
    // Same invocation retried: the retained response identity is reconciled, not re-sent.
    assert!(!run(&app, "call-2", &respond).is_error);
    assert_eq!(
        app.effects
            .borrow()
            .iter()
            .filter(|effect| effect.starts_with("respond:"))
            .count(),
        1
    );
    let polled = result_of(&app, &session, None);
    assert_eq!(polled["activity"], "settled");
    assert_eq!(polled["pending"], json!([]));
    assert_eq!(polled["latest_result"], "reply first");
}

#[test]
fn busy_child_continue_fails_closed_with_guidance() {
    let app = FakeApplication::default();
    *app.require_approval.borrow_mut() = true;
    let started = run(&app, "call-1", &delegate_input("first"));
    let session = started.output["session_id"].as_str().unwrap().to_owned();
    let next = SimpleInvocation::parse(
        CONTINUE_TOOL,
        &json!({"session_id":session,"prompt":"second"}),
    )
    .unwrap()
    .unwrap();
    let refused = run(&app, "call-2", &next);
    assert!(refused.is_error);
    assert_eq!(refused.output["reason"], "conflict");
    assert!(
        refused.output["hint"]
            .as_str()
            .unwrap()
            .contains("harness.result")
    );
    assert_eq!(
        app.effects
            .borrow()
            .iter()
            .filter(|effect| effect.starts_with("submit:"))
            .count(),
        1
    );
}

#[test]
fn interrupt_simple_shape_targets_the_retained_binding() {
    let app = FakeApplication::default();
    let started = run(&app, "call-1", &delegate_input("first"));
    let session = started.output["session_id"].as_str().unwrap().to_owned();
    let interrupt = SimpleInvocation::parse("harness.interrupt", &json!({"session_id":session}))
        .unwrap()
        .unwrap();
    let outcome = run(&app, "call-2", &interrupt);
    assert!(!outcome.is_error, "{}", outcome.output);
    assert_eq!(outcome.output["status"], "interrupt_requested");
}

#[test]
fn simple_inputs_are_strict_and_advanced_inputs_keep_their_route() {
    assert!(SimpleInvocation::parse(DELEGATE_TOOL, &json!({"prompt":"  "})).is_err());
    assert!(
        SimpleInvocation::parse(DELEGATE_TOOL, &json!({"prompt":"x","authority":"parent"}))
            .is_err()
    );
    assert!(
        SimpleInvocation::parse(
            DELEGATE_TOOL,
            &json!({"prompt":"x".repeat(MAX_HARNESS_TEXT_BYTES + 1)})
        )
        .is_err()
    );
    assert!(
        SimpleInvocation::parse(
            CONTINUE_TOOL,
            &json!({"session_id":"not-a-session","prompt":"x"})
        )
        .is_err()
    );
    assert!(
        SimpleInvocation::parse(
            "harness.respond",
            &json!({"session_id":PARENT,"request_id":"r"})
        )
        .is_err()
    );
    assert!(
        SimpleInvocation::parse(
            "harness.respond",
            &json!({"session_id":PARENT,"request_id":"r","approve":true,"answer":"yes"})
        )
        .is_err()
    );
    assert!(
        SimpleInvocation::parse(
            "harness.respond",
            &json!({"session_id":PARENT,"request_id":"r","approve":true,"choice_id":"0"})
        )
        .is_err()
    );
    assert!(SimpleInvocation::parse("harness.respond", &json!({"request_id_json":"{}"})).is_err());
    assert_eq!(
        SimpleInvocation::parse("harness.respond", &json!({"request_json":"{}"})).unwrap(),
        None
    );
    assert_eq!(
        SimpleInvocation::parse("harness.interrupt", &json!({"request_json":"{}"})).unwrap(),
        None
    );
    assert!(SimpleInvocation::parse("harness.start", &json!({"prompt":"x"})).is_err());
    assert!(
        SimpleInvocation::parse(RESULT_TOOL, &json!({"session_id":PARENT}))
            .unwrap()
            .unwrap()
            .read_only()
    );
}

#[test]
fn invocation_scope_comes_only_from_host_workspace_context() {
    let entry = |payload: Value| bcode_tool::ToolHostContextEntry {
        schema: bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA.into(),
        schema_version: bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION,
        payload,
    };
    let scope = InvocationScope::from_host_context(&[entry(
        json!({"working_directory":"/workspace","workspace_id":PARENT}),
    )])
    .unwrap();
    assert_eq!(scope.parent_session_id, PARENT);
    assert!(InvocationScope::from_host_context(&[]).is_err());
    assert!(
        InvocationScope::from_host_context(&[entry(
            json!({"working_directory":"relative","workspace_id":PARENT})
        )])
        .is_err()
    );
    assert!(
        InvocationScope::from_host_context(&[entry(
            json!({"working_directory":"/workspace","workspace_id":null})
        )])
        .is_err()
    );
}

#[test]
fn delegate_resolves_explicit_or_default_configured_instances_only() {
    let config = json!({"enabled":true, "default_instance":"claude", "instances":{"claude":{
        "enabled":true, "instance":instance()}}});
    assert_eq!(resolve_instance(&config, None).unwrap(), instance());
    assert_eq!(
        resolve_instance(&config, Some("claude")).unwrap(),
        instance()
    );
    assert!(resolve_instance(&config, Some("codex")).is_err());
    let mut no_default = config;
    no_default
        .as_object_mut()
        .unwrap()
        .remove("default_instance");
    assert!(resolve_instance(&no_default, None).is_err());
    assert!(resolve_instance(&Value::Null, None).is_err());
}

#[test]
fn result_previews_are_bounded() {
    let long = "é".repeat(RESULT_PREVIEW_BYTES);
    let shown = preview(&long, RESULT_PREVIEW_BYTES);
    assert!(shown.len() <= RESULT_PREVIEW_BYTES + '…'.len_utf8());
    let mut tail = String::new();
    for _ in 0..64 {
        append_tail(&mut tail, &long);
    }
    assert!(tail.len() <= RESULT_PREVIEW_BYTES * 2);
}

/// Canonical history pages end at committed batches, including empty ones.
struct BatchedHistory {
    batches: Vec<Vec<HarnessObservation>>,
}

impl ManagedApplication for BatchedHistory {
    fn call(&self, operation: &str, payload: Value) -> Result<Value, CallFailure> {
        assert_eq!(operation, "history");
        let request: ManagedDelegationHistoryRequest = serde_json::from_value(payload).unwrap();
        let index = request
            .after
            .as_deref()
            .map_or(0, |cursor| cursor.parse::<usize>().unwrap());
        let (observations, next) = self
            .batches
            .get(index)
            .map_or((Vec::new(), index), |batch| (batch.clone(), index + 1));
        Ok(
            json!({"observations":observations,"next_after":next.to_string(),
            "completeness":"partial","status":{"connectivity":"unknown","activity":"unknown",
            "attention":"unknown","continuation":"unknown","completeness":"unknown"}}),
        )
    }
}

#[test]
fn result_reads_past_empty_committed_batches_until_the_cursor_stops() {
    let mut child = Child::default();
    FakeApplication::observation(
        &mut child,
        PARENT,
        HarnessObservationKind::SubmissionAccepted {
            submission_id: "submit".into(),
            attempt_id: "attempt".into(),
            native_turn_id: "turn".into(),
        },
    );
    FakeApplication::finish(&mut child, PARENT, "submit", "attempt", "batched");
    let mut observations = child.observations.into_iter();
    let first = observations.next().unwrap();
    let app = BatchedHistory {
        batches: vec![vec![first], Vec::new(), observations.collect()],
    };
    let invocation = SimpleInvocation::parse(RESULT_TOOL, &json!({"session_id": PARENT}))
        .unwrap()
        .unwrap();
    let outcome = execute(&app, &scope(), "poll", &invocation, Some(&instance()));
    assert_eq!(outcome.output["activity"], "settled", "{}", outcome.output);
    assert_eq!(outcome.output["latest_result"], "reply batched");
    assert_eq!(outcome.output["cursor"], "3");
}
