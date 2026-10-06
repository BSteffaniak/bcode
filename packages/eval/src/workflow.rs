//! Real product workflow execution. Never reads daemon databases or private sidecars.
use super::{
    Command, EvalArtifactRef, EvalCase, EvalDiagnostic, EvalDiagnosticSeverity, EvalError,
    EvalMeasurementSet, EvalSuite, EvalVariant, ExecutionOutput, Instant, Path, fs,
    prepare_agent_daemon_isolation, process, rendered_prompt, session_telemetry, write_json,
    write_session_events_jsonl,
};
use bcode_eval_models::WorkflowEvalConfig;
use sha2::Digest;
use std::time::Duration;

fn verify_cancellation(
    run_id: &str,
    invoke: &impl Fn(&[String], &str, Duration) -> Result<serde_json::Value, EvalError>,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let args = [
            "workflow".into(),
            "inspect-run".into(),
            "--run-id".into(),
            run_id.into(),
            "--limit".into(),
            "1".into(),
        ];
        if invoke(&args, "cancel-verification", Duration::from_secs(3))
            .is_ok_and(|value| terminal_status(&value))
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

pub fn terminal_status(value: &serde_json::Value) -> bool {
    value
        .pointer("/run/status")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|status| ["completed", "failed", "cancelled"].contains(&status))
}

pub fn execute(
    suite: &EvalSuite,
    case: &EvalCase,
    variant: &EvalVariant,
    rep_dir: &Path,
    workspace: &Path,
) -> Result<ExecutionOutput, EvalError> {
    let mut config: WorkflowEvalConfig =
        serde_json::from_value(variant.metadata.get("workflow").cloned().ok_or_else(|| {
            EvalError::Validation("workflow executor requires metadata.workflow".into())
        })?)?;
    if let Some(oracle) = case.metadata.get("workflow_oracle") {
        if let Some(value) = oracle.get("expected_contains") {
            config.expected_contains = serde_json::from_value(value.clone())?;
        }
        if let Some(value) = oracle.get("answer_file") {
            config.answer_file = serde_json::from_value(value.clone())?;
        }
        if let Some(value) = oracle.get("expected_json") {
            config.expected_json = Some(value.clone());
        }
        if let Some(value) = oracle.get("oracle_command") {
            config.oracle_command = serde_json::from_value(value.clone())?;
        }
        if let Some(value) = oracle.get("expect_rejection") {
            config.expect_rejection = serde_json::from_value(value.clone())?;
        }
    }
    if let Some(command) = &mut config.oracle_command {
        let suite_dir = suite
            .metadata
            .get("suite_dir")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                EvalError::Validation("oracle requires resolved suite directory".into())
            })?;
        *command = command.replace("{{suite_dir}}", suite_dir);
    }
    if !config.binary.is_absolute()
        || !config.binary.is_file()
        || !["auto", "collaborate", "no-delegation"].contains(&config.strategy.as_str())
        || config.max_iterations == 0
        || config.concurrency == 0
    {
        return Err(EvalError::Validation(
            "invalid workflow executable, strategy or allowances".into(),
        ));
    }
    if let Some(profile) = &variant.profile {
        return Err(EvalError::Validation(format!(
            "workflow profile {profile} requires an explicit config_toml overlay; profile metadata is not applied by the external CLI"
        )));
    }
    let _environment = prepare_agent_daemon_isolation(variant, rep_dir)?;
    let preserved: Vec<_> = config
        .preserve_files
        .iter()
        .map(|path| {
            if path.is_absolute()
                || path
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                return Err(EvalError::Validation(
                    "preservation paths must be workspace-relative".into(),
                ));
            }
            Ok((path.clone(), fs::read(workspace.join(path))?))
        })
        .collect::<Result<_, EvalError>>()?;
    let mut hasher = sha2::Sha256::new();
    {
        use std::io::Read;
        let mut file = fs::File::open(&config.binary)?;
        let mut buffer = vec![0_u8; 65_536];
        loop {
            let length = file.read(&mut buffer)?;
            if length == 0 {
                break;
            }
            hasher.update(&buffer[..length]);
        }
    }
    let digest = hex::encode(hasher.finalize());
    write_json(
        rep_dir.join("subject.json"),
        &serde_json::json!({"schema_version":1,"binary":config.binary,"sha256":digest,"strategy":config.strategy,"judge":config.judge,"acceptance":config.stop_condition}),
    )?;
    let deadline =
        Instant::now() + Duration::from_millis(case.timeout_ms.unwrap_or(suite.run.timeout_ms));
    let invoke =
        |args: &[String], label: &str, timeout: Duration| -> Result<serde_json::Value, EvalError> {
            let mut command = Command::new(&config.binary);
            command
                .args(args)
                .current_dir(workspace)
                .envs(&variant.env)
                .envs(&case.env);
            let (code, expired, out, err) =
                process::bounded_command(command, timeout, rep_dir, label)?;
            if expired || code != Some(0) {
                return Err(EvalError::Client(format!(
                    "{label} failed (exit={code:?}, timeout={expired}): {}",
                    String::from_utf8_lossy(&err)
                )));
            }
            Ok(serde_json::from_slice(&out)?)
        };
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let session = invoke(
        &[
            "session".into(),
            "create".into(),
            "--cwd".into(),
            workspace.display().to_string(),
            "--json".into(),
            format!("eval:{}:{}", case.id, variant.id),
        ],
        "session",
        remaining(),
    )?;
    let session_id = session["id"]
        .as_str()
        .ok_or_else(|| EvalError::Validation("session response missing identity".into()))?;
    if let Some(model) = &variant.model {
        invoke(
            &[
                "session".into(),
                "set-model".into(),
                session_id.into(),
                model.clone(),
                "--json".into(),
            ],
            "model-selection",
            remaining(),
        )?;
    }
    let mut args = vec![
        "goal".into(),
        "--session".into(),
        session_id.into(),
        "--objective".into(),
        rendered_prompt(case, variant),
        "--stop-condition".into(),
        config.stop_condition.clone(),
        "--agents".into(),
        config.strategy.clone(),
        "--max-iterations".into(),
        config.max_iterations.to_string(),
        "--concurrency".into(),
        config.concurrency.to_string(),
        "--worker-attempts".into(),
        config.worker_attempts.to_string(),
        "--swarm-instructions".into(),
        config.guidance.clone(),
    ];
    if config.generated_entry {
        let position = args
            .iter()
            .position(|value| value == "--stop-condition")
            .expect("authored condition argument");
        args.drain(position..position + 2);
    }
    if let Some(judge) = &config.judge {
        args.extend([
            "--judge-provider".into(),
            judge.provider.clone(),
            "--judge-model".into(),
            judge.model.clone(),
            "--judge-account".into(),
            judge.account.clone(),
            "--judge-threshold".into(),
            judge.threshold.to_string(),
            "--judge-failure".into(),
            "pause".into(),
        ]);
    }
    let start = Instant::now();
    let admitted = invoke(&args, "admission", remaining())?;
    let run_id = admitted["run_id"]
        .as_str()
        .ok_or_else(|| EvalError::Validation("admission missing run identity".into()))?;
    write_json(rep_dir.join("admission.json"), &admitted)?;
    let mut timed_out = false;
    let mut inspection;
    loop {
        if remaining().is_zero() {
            timed_out = true;
            break;
        }
        let inspection_args = vec![
            "workflow".into(),
            "inspect-run".into(),
            "--run-id".into(),
            run_id.into(),
            "--limit".into(),
            "100".into(),
        ];
        inspection = match invoke(&inspection_args, "inspection", remaining()) {
            Ok(value) => value,
            Err(error) if remaining().is_zero() => {
                timed_out = true;
                write_json(rep_dir.join("observation-error.json"), &error.to_string())?;
                break;
            }
            Err(error) => {
                write_json(rep_dir.join("observation-error.json"), &error.to_string())?;
                let cancellation = invoke(
                    &[
                        "workflow".into(),
                        "cancel-run".into(),
                        "--run-id".into(),
                        run_id.into(),
                    ],
                    "observation-cancellation",
                    Duration::from_secs(15),
                );
                let verified = verify_cancellation(run_id, &invoke);
                write_json(
                    rep_dir.join("cancellation.json"),
                    &serde_json::json!({"requested":cancellation.is_ok(),"error":cancellation.err().map(|error|error.to_string()),"terminal_verified":verified,"reason":"observation_failed"}),
                )?;
                return Err(error);
            }
        };
        write_json(rep_dir.join("inspection.json"), &inspection)?;
        let status = inspection
            .pointer("/run/status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if ["completed", "failed", "cancelled", "repair_required"].contains(&status) {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if timed_out {
        let cancel = invoke(
            &[
                "workflow".into(),
                "cancel-run".into(),
                "--run-id".into(),
                run_id.into(),
            ],
            "cancellation",
            Duration::from_secs(15),
        );
        let requested = cancel.is_ok();
        let cancellation_deadline = Instant::now() + Duration::from_secs(15);
        let mut terminal_verified = false;
        while requested && Instant::now() < cancellation_deadline {
            let state = invoke(
                &[
                    "workflow".into(),
                    "inspect-run".into(),
                    "--run-id".into(),
                    run_id.into(),
                    "--limit".into(),
                    "100".into(),
                ],
                "cancel-status",
                Duration::from_secs(5),
            );
            if state.as_ref().is_ok_and(terminal_status) {
                terminal_verified = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        write_json(
            rep_dir.join("cancellation.json"),
            &serde_json::json!({"requested":requested,"error":cancel.err().map(|e|e.to_string()),"terminal_verified":terminal_verified}),
        )?;
    }
    let outputs = invoke(
        &[
            "workflow".into(),
            "run-output".into(),
            "--run-id".into(),
            run_id.into(),
            "--limit".into(),
            "100".into(),
        ],
        "outputs",
        Duration::from_secs(15),
    )?;
    write_json(rep_dir.join("workflow-output.json"), &outputs)?;
    let history = invoke(
        &[
            "session".into(),
            "history".into(),
            session_id.into(),
            "--limit".into(),
            "100".into(),
            "--json".into(),
        ],
        "history",
        Duration::from_secs(15),
    )?;
    write_json(rep_dir.join("session-history.json"), &history)?;
    let mut measurements = EvalMeasurementSet::new();
    measurements.insert(
        "workflow.wall_time_ms".into(),
        start.elapsed().as_millis() as f64,
    );
    measurements.insert("workflow.timed_out".into(), f64::from(timed_out));
    let final_inspection = invoke(
        &[
            "workflow".into(),
            "inspect-run".into(),
            "--run-id".into(),
            run_id.into(),
            "--limit".into(),
            "100".into(),
        ],
        "final",
        Duration::from_secs(15),
    )?;
    write_json(rep_dir.join("inspection.json"), &final_inspection)?;
    let mut capture_failed = false;
    if let Some(revision) = final_inspection
        .pointer("/graph/revision")
        .and_then(serde_json::Value::as_u64)
        && let Err(error) = super::capture::capture(run_id, revision, rep_dir, invoke)
    {
        capture_failed = true;
        write_json(rep_dir.join("graph-capture-error.json"), &error.to_string())?;
    }
    if let Err(error) = super::capture::capture_history(run_id, rep_dir, invoke) {
        capture_failed = true;
        write_json(
            rep_dir.join("history-capture-error.json"),
            &error.to_string(),
        )?;
    }
    let mut event_sessions = vec![session_id.to_owned()];
    event_sessions.extend(
        final_inspection["child_sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|value| value["id"].as_str().map(str::to_owned)),
    );
    let mut telemetry = EvalMeasurementSet::new();
    let mut capture_complete = !capture_failed
        && final_inspection["unavailable_collections"]
            .as_array()
            .is_none_or(Vec::is_empty)
        && final_inspection.pointer("/graph/nodes_complete")
            == Some(&serde_json::Value::Bool(true))
        && final_inspection.pointer("/graph/edges_complete")
            == Some(&serde_json::Value::Bool(true))
        && ["activations", "child_sessions", "events", "attempts"]
            .iter()
            .all(|key| {
                final_inspection[*key]
                    .as_array()
                    .is_some_and(|values| values.len() < 100)
            })
        && outputs.as_array().is_some_and(|values| values.len() < 100);
    for (index, identity) in event_sessions.iter().enumerate() {
        let mut after = 0_u64;
        let mut events = Vec::new();
        loop {
            let page = invoke(
                &[
                    "session".into(),
                    "history".into(),
                    identity.clone(),
                    "--after".into(),
                    after.to_string(),
                    "--limit".into(),
                    "100".into(),
                    "--json".into(),
                ],
                &format!("history-{index}"),
                Duration::from_secs(15),
            )?;
            let page_events: Vec<bcode_session_models::SessionEvent> =
                serde_json::from_value(page["events"].clone())?;
            if let Some(last) = page_events.last() {
                after = last.sequence + 1;
            }
            events.extend(page_events);
            if page["has_more"] != true {
                break;
            }
            if events.len() >= 10000 {
                capture_complete = false;
                break;
            }
        }
        write_session_events_jsonl(&rep_dir.join(format!("transcript-{index}.jsonl")), &events)?;
        for (key, value) in session_telemetry(&events).measurements {
            *telemetry.entry(key).or_default() += value;
        }
    }
    measurements.extend(telemetry);
    measurements.insert(
        "workflow.capture_complete".into(),
        f64::from(capture_complete),
    );
    let activations = final_inspection["activations"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let worker_count = activations
        .iter()
        .filter(|value| {
            value["status"] == "completed"
                && value["node_id"]
                    .as_str()
                    .is_some_and(|id| !id.starts_with("loop.") && !id.starts_with("delegation."))
        })
        .count();
    measurements.insert("workflow.worker_activations".into(), worker_count as f64);
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for value in &activations {
        if let Some(id) = value["node_id"].as_str() {
            *counts.entry(id.into()).or_default() += 1;
        }
    }
    let repeated = counts
        .iter()
        .filter(|(id, _)| !id.starts_with("loop.") && !id.starts_with("delegation."))
        .map(|(_, count)| count.saturating_sub(1))
        .sum::<usize>();
    measurements.insert(
        "workflow.repeated_worker_activations".into(),
        repeated as f64,
    );
    let judge_calls = activations
        .iter()
        .filter(|value| value["node_id"] == "loop.judgement.evaluate")
        .count();
    measurements.insert("workflow.judge_calls".into(), judge_calls as f64);
    let status = final_inspection
        .pointer("/run/status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let completed = status == "completed"
        && final_inspection.pointer("/terminal_output/value/condition_met")
            == Some(&serde_json::Value::Bool(true));
    measurements.insert("workflow.completion_claimed".into(), f64::from(completed));
    let completed_value = final_inspection.pointer("/terminal_output/value");
    let output_text = completed_value.map_or_else(String::new, |value| {
        serde_json::to_string(
            &serde_json::json!({"summary":value["summary"],"evidence":value["evidence"]}),
        )
        .unwrap_or_default()
    });
    let artifact_observed = completed_value.is_some()
        || config.answer_file.is_some()
        || (config.oracle_command.is_some()
            && config.expected_contains.is_empty()
            && config.expected_json.is_none());
    measurements.insert(
        "workflow.artifact_observed".into(),
        f64::from(artifact_observed),
    );
    let artifact_correct = if config.expected_contains.is_empty()
        && config.expected_json.is_none()
        && config.oracle_command.is_none()
    {
        false
    } else {
        super::oracle::grade(&config, workspace, rep_dir, &output_text)?
    };
    let preservation = preserved
        .iter()
        .all(|(path, bytes)| fs::read(workspace.join(path)).is_ok_and(|current| current == *bytes));
    let oracle_passed = preservation
        && if config.expect_rejection {
            !completed && status == "failed"
        } else {
            artifact_correct && completed
        };
    let false_completion =
        completed && (!artifact_correct || !preservation || config.expect_rejection);
    let completion_failure = !config.expect_rejection && artifact_correct && !completed;
    for (key, value) in [
        ("oracle_passed", oracle_passed),
        ("artifact_correct", artifact_correct),
        ("preservation_passed", preservation),
        ("false_completion", false_completion),
        ("completion_failure", completion_failure),
    ] {
        measurements.insert(format!("workflow.{key}"), f64::from(value));
    }
    write_json(
        rep_dir.join("oracle.json"),
        &serde_json::json!({"schema_version":1,"passed":oracle_passed,"artifact_correct":artifact_correct,"artifact_observed":artifact_observed,"preservation_passed":preservation,"completion_claimed":completed,"false_completion":false_completion,"completion_failure":completion_failure,"status":status,"timed_out":timed_out,"capture_complete":capture_complete}),
    )?;
    let oracle_value = serde_json::json!({"completion_claimed":completed,"status":status,"capture_complete":capture_complete,"timed_out":timed_out,"artifact_correct":artifact_correct,"artifact_observed":artifact_observed,"false_completion":false_completion,"preservation_passed":preservation,"completion_failure":completion_failure});
    write_json(
        rep_dir.join("diagnosis.json"),
        &super::diagnosis::classify(&oracle_value, &outputs, &final_inspection, &measurements),
    )?;
    let diagnostics = if timed_out {
        vec![EvalDiagnostic {severity:EvalDiagnosticSeverity::Error,message:"workflow deadline exceeded; cancellation requested, terminal state requires verification".into()}]
    } else {
        Vec::new()
    };
    Ok(ExecutionOutput {
        exit_code: Some(if timed_out {
            124
        } else {
            i32::from(!oracle_passed)
        }),
        measurements,
        diagnostics,
        artifacts: vec![
            EvalArtifactRef {
                kind: "workflow_outputs".into(),
                path: "workflow-output.json".into(),
            },
            EvalArtifactRef {
                kind: "workflow_inspection".into(),
                path: "inspection.json".into(),
            },
        ],
    })
}
