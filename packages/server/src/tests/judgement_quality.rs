//! Offline comparative captures from real consumer services and central optional access.

async fn run_capture_turn(
    state: &ServerState,
    session: SessionId,
    text: String,
) -> ModelTurnCompletion {
    let (_, accepted) = state
        .sessions
        .admit_queued_turn_with_events(
            session,
            ClientId::new(),
            text,
            bcode_session_models::TurnAdmissionMetadata::default(),
        )
        .await
        .unwrap();
    let SessionEventKind::SubmissionAccepted { submission } = &accepted[0].kind else {
        panic!("expected admitted submission")
    };
    state
        .sessions
        .activate_submission(session, submission.id.clone())
        .await
        .unwrap();
    let trigger = state
        .sessions
        .session_history(session)
        .await
        .unwrap()
        .into_iter()
        .rev()
        .find(|event| matches!(event.kind, SessionEventKind::SubmissionActivated { .. }))
        .unwrap();
    let mut permit = SessionTurnPermit::new(session);
    let (followup_tx, mut followup) = mpsc::channel(1);
    let (steering_tx, mut steering) = mpsc::channel(1);
    let (cancel_tx, mut cancel) = mpsc::channel(1);
    let queued = Arc::new(AtomicUsize::new(0));
    let cancel_state = Arc::new(TurnCancelState::default());
    // The registered actor starts idle; lifecycle alone owns begin/end_current_turn.
    let current = Arc::new(Mutex::new(None));
    let phase = Arc::new(Mutex::new(SessionRuntimePhase::Idle));
    state.session_runtimes.lock().await.insert(
        session,
        SessionRuntimeHandle {
            followup_commands: followup_tx,
            steering_commands: steering_tx,
            cancel_commands: cancel_tx,
            queued_followups: Arc::clone(&queued),
            queued_steering: Arc::new(AtomicUsize::new(0)),
            phase: Arc::clone(&phase),
            current_turn: Arc::clone(&current),
        },
    );
    let mut commands = RuntimeCommandContext::new(
        &mut followup,
        &mut steering,
        &mut cancel,
        &queued,
        Arc::clone(&current),
    );
    let completion = run_model_turn(
        state,
        &mut permit,
        &trigger,
        ClientId::new(),
        None,
        &mut commands,
        &phase,
        Some(cancel_state),
        false,
    )
    .await;
    assert!(
        current.lock().await.is_none(),
        "lifecycle must release its current turn after settlement"
    );
    let resolved = state.session_config(session).await;
    for feature in [
        Feature::SessionCategorization,
        Feature::ToolResultTriage,
        Feature::ProgressMonitoring,
        Feature::Recommendations,
        Feature::ContextReranking,
        Feature::EvalCategorization,
    ] {
        assert_eq!(
            resolved.judgement.enabled(feature),
            state.startup_config.judgement.enabled(feature),
            "lifecycle resolved feature mode differs: {feature:?}"
        );
    }
    assert!(
        resolved.judgement.selection == state.startup_config.judgement.selection,
        "lifecycle must retain fixture judgement selection"
    );
    completion
}

fn configure_normal_turn(state: &mut ServerState) {
    state.startup_config.skills.enabled = false;
    state.startup_config.system_prompt.mode = bcode_config::SystemPromptMode::Replace;
    state.startup_config.system_prompt.text = Some("Offline integration fixture. Preserve objectives, settled results and completion authority.".into());
    state
        .startup_config
        .system_prompt
        .sections
        .repository_context = false;
    state
        .startup_config
        .system_prompt
        .sections
        .repository_invariants = false;
    state
        .startup_config
        .system_prompt
        .sections
        .dynamic_repository_context = false;
    state.selected_provider_plugin_id = Some("bcode.fake-provider".into());
    state
        .selected_provider_context
        .settings
        .insert("fake_prompt_tool_directives".into(), "true".into());
}

async fn create_capture_session(state: &ServerState, root: &std::path::Path) -> SessionId {
    let session = state
        .sessions
        .create_session(None, root.into())
        .await
        .unwrap()
        .id;
    state
        .sessions
        .append_model_changed(
            session,
            "bcode.fake-provider".into(),
            "fake-echo".into(),
            bcode_session_models::ModelSelectionSource::UserExplicit,
        )
        .await
        .unwrap();
    session
}

#[tokio::test]
async fn normal_session_turn_captures_settled_tool_and_categorization() {
    let _guard = HARNESS_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("lease.rs");
    std::fs::write(&file, "fn release() { drop(database_handle); }\n").unwrap();
    let mut captured = Vec::new();
    for arm in [
        QualityArm::GlobalOff,
        QualityArm::FeatureOff,
        QualityArm::AutoNoKey,
        QualityArm::AutoError,
        QualityArm::AutoEligible,
    ] {
        REQUESTS.lock().unwrap().clear();
        let mut state = state(arm, Feature::SessionCategorization);
        configure_normal_turn(&mut state);
        let session = create_capture_session(&state, root.path()).await;
        let completion = run_capture_turn(
            &state,
            session,
            format!(
                "Debug database persistence failure\ntool-call filesystem.read {}",
                serde_json::json!({"path":file,"offset":1,"limit":10})
            ),
        )
        .await;
        assert_eq!(
            completion.outcome,
            ModelTurnOutcome::Completed,
            "{arm:?}: {:?}",
            completion.message
        );
        let requests = REQUESTS.lock().unwrap().clone();
        assert!(
            requests.len() >= 2,
            "must capture actual tool continuation: {arm:?}"
        );
        let tool_result = requests
            .last()
            .unwrap()
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|block| match block {
                ContentBlock::ToolResult { result } => Some(result),
                _ => None,
            })
            .unwrap();
        assert!(!tool_result.is_error);
        assert!(
            tool_result
                .output
                .starts_with("fn release() { drop(database_handle); }")
        );
        let history = state.sessions.session_history(session).await.unwrap();
        assert!(history.iter().any(|event| matches!(
            event.kind,
            SessionEventKind::ToolInvocationResultRecorded { .. }
        )));
        let labels = state
            .advisory_categorizations
            .lock()
            .unwrap()
            .get(&session)
            .cloned();
        let calls = CALLS.lock().unwrap().clone();
        assert_call_policy(Feature::SessionCategorization, arm, &calls);
        assert_eq!(
            labels.is_some(),
            arm == QualityArm::AutoEligible,
            "categorization {arm:?}"
        );
        captured.push((
            arm,
            labels.map(|labels| (labels.task, labels.domain)),
            calls.inference,
        ));
        drop(state);
    }
    println!(
        "normal-session-turn offline-correctness arms={captured:?}; live-improvement=unproven"
    );
}

use super::*;
use bcode_eval::quality::{
    self, QualityArm, QualityCalls, QualityObservation, QualityOutput, QualityPath,
};
use bcode_model::judgement::{self, Feature};
use serde::Deserialize;

const PROVIDER_MANIFEST: &str =
    include_str!("../../../../plugins/fake-provider-plugin/bcode-plugin.toml");

#[tokio::test]
async fn normal_session_failures_capture_triage_and_progress() {
    let _guard = HARNESS_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing.rs");
    let mut captures = Vec::new();
    for feature in [Feature::ToolResultTriage, Feature::ProgressMonitoring] {
        for arm in [
            QualityArm::GlobalOff,
            QualityArm::FeatureOff,
            QualityArm::AutoNoKey,
            QualityArm::AutoError,
            QualityArm::AutoEligible,
        ] {
            REQUESTS.lock().unwrap().clear();
            let mut state = state(arm, feature);
            configure_normal_turn(&mut state);
            let session = create_capture_session(&state, root.path()).await;
            let directive = format!(
                "tool-call-expect-error No such file or directory :: filesystem.read {}",
                serde_json::json!({"path":missing,"offset":1,"limit":10})
            );
            let completion = run_capture_turn(&state, session, format!("Debug missing database prerequisite; do not claim success.\n{directive}\n{directive}\n{directive}\n{directive}")).await;
            assert_eq!(
                completion.outcome,
                ModelTurnOutcome::Completed,
                "{feature:?} {arm:?}: {:?}",
                completion.message
            );
            let requests = REQUESTS.lock().unwrap().clone();
            assert!(
                requests.len() >= 5,
                "four settled tool continuations required"
            );
            let results = requests
                .last()
                .unwrap()
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|block| match block {
                    ContentBlock::ToolResult { result } => Some(result),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(results.len(), 4);
            assert!(results.iter().all(
                |result| result.is_error && result.output.contains("No such file or directory")
            ));
            if feature == Feature::ProgressMonitoring {
                assert!(
                    results
                        .windows(2)
                        .all(|pair| pair[0].output == pair[1].output),
                    "fixture must repeat the same settled failure fingerprint"
                );
                println!(
                    "progress fixture {arm:?} bounded_window={:?}",
                    state.progress_windows.lock().unwrap().get(&session)
                );
                assert!(requests[0].messages.iter().any(|message| message.role == MessageRole::User && message.content.iter().any(|block| matches!(block, ContentBlock::Text { text } if !text.trim().is_empty() && text.len() <= 4096))), "explicit objective must reach actual model context within progress bound");
            }
            let schema = match feature {
                Feature::ToolResultTriage => "bcode.loop.diagnostic-advisory/v1",
                Feature::ProgressMonitoring => "bcode.loop.progress-monitoring/v1",
                _ => unreachable!(),
            };
            let advice = requests
                .iter()
                .flat_map(|request| &request.messages)
                .flat_map(|message| &message.content)
                .filter_map(|block| match block {
                    ContentBlock::Text { text }
                        if (feature == Feature::ToolResultTriage
                            && text.starts_with("Read-only diagnostic advisory"))
                            || (feature == Feature::ProgressMonitoring
                                && text.starts_with("Advisory observation only;")) =>
                    {
                        Some(text.clone())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            let calls = CALLS.lock().unwrap().clone();
            println!(
                "{feature:?} {arm:?} calls={calls:?} normalized_judgements={:?}",
                JUDGEMENT_CAPTURES.lock().unwrap().clone()
            );
            assert_call_policy(feature, arm, &calls);
            assert_eq!(
                !advice.is_empty(),
                arm == QualityArm::AutoEligible,
                "real delivered advice {feature:?} {arm:?}"
            );
            let history = state.sessions.session_history(session).await.unwrap();
            assert!(!history.iter().any(|event| matches!(&event.kind, SessionEventKind::ToolContributionPlaced { envelope } if envelope.contribution.schema == schema)), "advisory must remain transient");
            drop(state);
            captures.push((feature, arm, advice, calls.inference));
        }
    }
    println!("normal-session failures offline-correctness={captures:?}; live-improvement=unproven");
}

#[derive(Default)]
struct CountedProvider {
    provider: bcode_fake_provider_plugin::FakeProviderPlugin,
}

// Each harness run is serialized internally; no other test uses these counters.
static CALLS: std::sync::Mutex<QualityCalls> = std::sync::Mutex::new(QualityCalls {
    discovery: 0,
    verification: 0,
    inference: 0,
});
static JUDGEMENT_CAPTURES: std::sync::Mutex<
    Vec<(judgement::Request, Option<judgement::Response>)>,
> = std::sync::Mutex::new(Vec::new());
static REQUESTS: std::sync::Mutex<Vec<bcode_model::ModelTurnRequest>> =
    std::sync::Mutex::new(Vec::new());
static HARNESS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static FAIL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

static INFERENCE_GATE: std::sync::Mutex<Option<Arc<InferenceGate>>> = std::sync::Mutex::new(None);

#[derive(Default)]
struct InferenceGate {
    entered: std::sync::atomic::AtomicBool,
    released: std::sync::Mutex<bool>,
    wake: std::sync::Condvar,
    returned: std::sync::atomic::AtomicBool,
}
impl InferenceGate {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
    async fn wait_for(&self, flag: &std::sync::atomic::AtomicBool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !flag.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("gated offline inference did not reach expected boundary");
    }
}
struct ReleaseInference(Arc<InferenceGate>);
impl Drop for ReleaseInference {
    fn drop(&mut self) {
        self.0.release();
        *INFERENCE_GATE.lock().unwrap() = None;
    }
}

impl bcode_plugin_sdk::RustPlugin for CountedProvider {
    fn register_auth_providers(
        &mut self,
        registrar: bcode_plugin_sdk::AuthRegistrar,
    ) -> Result<(), bcode_plugin_sdk::PluginError> {
        self.provider.register_auth_providers(registrar)
    }

    fn invoke_service(
        &mut self,
        context: bcode_plugin_sdk::NativeServiceContext,
    ) -> bcode_plugin_sdk::ServiceResponse {
        if matches!(
            context.request.operation.as_str(),
            bcode_model::OP_START_TURN | bcode_model::OP_RUN_TURN
        ) && let Ok(request) = context
            .request
            .payload_json::<bcode_model::ModelTurnRequest>()
        {
            REQUESTS.lock().unwrap().push(request);
        }
        if context.request.interface_id == judgement::INTERFACE_ID
            || context.request.interface_id == judgement::VERIFICATION_INTERFACE_ID
        {
            let mut calls = CALLS.lock().unwrap();
            match context.request.operation.as_str() {
                judgement::OP_MODELS => calls.discovery += 1,
                judgement::OP_VERIFY => calls.verification += 1,
                judgement::OP_JUDGE => calls.inference += 1,
                _ => {}
            }
        }
        if context.request.operation == judgement::OP_JUDGE && FAIL.load(Ordering::SeqCst) {
            return bcode_plugin_sdk::ServiceResponse::error(
                "offline_failure",
                "offline fixture failure",
            );
        }
        let judged = if context.request.operation == judgement::OP_JUDGE {
            context
                .request
                .payload_json::<judgement::ProviderRequest>()
                .ok()
                .map(|envelope| envelope.judgement)
        } else {
            None
        };
        let gate = judged
            .as_ref()
            .and_then(|_| INFERENCE_GATE.lock().unwrap().clone());
        if let Some(gate) = &gate {
            gate.entered.store(true, Ordering::SeqCst);
            let (released, timeout) = gate
                .wake
                .wait_timeout_while(
                    gate.released.lock().unwrap(),
                    Duration::from_secs(10),
                    |released| !*released,
                )
                .unwrap();
            assert!(
                *released && !timeout.timed_out(),
                "offline inference gate must be released"
            );
            drop(released);
        }
        let response = self.provider.invoke_service(context);
        if let Some(request) = judged {
            JUDGEMENT_CAPTURES
                .lock()
                .unwrap()
                .push((request, response.payload_json::<judgement::Response>().ok()));
        }
        if let Some(gate) = gate {
            gate.returned.store(true, Ordering::SeqCst);
        }
        response
    }
}

fn state(arm: QualityArm, feature: Feature) -> ServerState {
    JUDGEMENT_CAPTURES.lock().unwrap().clear();
    *CALLS.lock().unwrap() = QualityCalls::default();
    FAIL.store(arm == QualityArm::AutoError, Ordering::SeqCst);
    let bundled = [
        bcode_plugin::StaticBundledPlugin::new(
            PROVIDER_MANIFEST,
            bcode_plugin_sdk::static_plugin_vtable!(CountedProvider, PROVIDER_MANIFEST),
        ),
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../../plugins/filesystem-plugin/bcode-plugin.toml"),
            bcode_filesystem_plugin::static_plugin(),
        ),
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../../plugins/skills-plugin/bcode-plugin.toml"),
            bcode_skills_plugin::static_plugin(),
        ),
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../../plugins/loop-plugin/bcode-plugin.toml"),
            bcode_loop_plugin::static_plugin(),
        ),
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../../plugins/session-derivation-plugin/bcode-plugin.toml"),
            bcode_session_derivation_plugin::static_plugin(),
        ),
    ];
    let plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::Explicit,
            enabled: BTreeSet::from([
                "bcode.fake-provider".into(),
                "bcode.filesystem".into(),
                "bcode.skills".into(),
                "bcode.loop".into(),
                "bcode.session-derivation".into(),
            ]),
            disabled: BTreeSet::new(),
        },
        &bundled,
    )
    .unwrap();
    assert!(
        plugins
            .auth_provider_registry()
            .providers()
            .iter()
            .any(|provider| provider.plugin_id == "bcode.fake-provider"),
        "counted adapter must preserve the real provider's auth registration"
    );
    let mut state = test_server_state_with_plugins(SessionManager::default(), plugins);
    // Exact offline profile means discovery cannot select a user's provider/account.
    state.startup_config.judgement.selection = Some(judgement::SetupSelection {
        provider_plugin_id: "bcode.fake-provider".into(),
        model_id: "fake-judgement".into(),
        auth_profile: "quality-offline".into(),
    });
    state.startup_config.auth.profiles.insert(
        "quality-offline".into(),
        bcode_config::AuthProfileConfig {
            backend: "aws".into(),
            owner_plugin_id: Some("bcode.fake-provider".into()),
            provider_id: Some("fake".into()),
            scheme: Some("api_key".into()),
            map: BTreeMap::from([(
                "api_key".into(),
                bcode_config::AuthCredentialMapping {
                    env: Some("QUALITY_OFFLINE_TOKEN".into()),
                    key: None,
                },
            )]),
            settings: if arm == QualityArm::AutoNoKey {
                BTreeMap::new()
            } else {
                BTreeMap::from([(
                    "env.QUALITY_OFFLINE_TOKEN".into(),
                    "offline-test-only".into(),
                )])
            },
        },
    );
    if feature == Feature::ProgressMonitoring {
        state
            .startup_config
            .auth
            .profiles
            .get_mut("quality-offline")
            .unwrap()
            .settings
            .insert(
                "env.FAKE_JUDGEMENT_YES_NO_PROBABILITIES".into(),
                serde_json::json!({"progress":0.1,"stall":0.9,"drift":0.1}).to_string(),
            );
    }
    isolate_feature(&mut state.startup_config.judgement, arm, feature);
    state
}

async fn service<T: serde::Serialize + Sync, R: serde::de::DeserializeOwned>(
    state: &ServerState,
    interface: &str,
    operation: &str,
    input: &T,
    cancel: &TurnCancelState,
) -> Option<R> {
    let value = invoke_optional_feature_service(
        state,
        interface,
        operation,
        serde_json::to_value(input).unwrap(),
        &state.startup_config,
        cancel,
    )
    .await?;
    serde_json::from_value(value).ok()
}

#[derive(Deserialize)]
struct RankingInput {
    task: String,
    candidates: Vec<RankingCandidate>,
    protected: Vec<String>,
    #[serde(default)]
    byte_budget: usize,
}
#[derive(Deserialize)]
struct RankingCandidate {
    id: String,
    description: String,
    #[serde(default)]
    source: Option<bcode_session_models::JudgementContextSource>,
}

async fn ranking(
    state: &ServerState,
    case: &quality::QualityCase,
    cancel: &TurnCancelState,
) -> QualityOutput {
    use bcode_skills_plugin::{context_selection as context, selection};
    let input: RankingInput = serde_json::from_value(case.input.clone()).unwrap();
    if case.feature == Feature::ContextReranking {
        let request = context::SelectionRequest {
            task: input.task,
            budget_bytes: Some(input.byte_budget),
            candidates: input
                .candidates
                .into_iter()
                .map(|candidate| context::Candidate {
                    protected: input.protected.contains(&candidate.id),
                    provenance: format!("fixture:{}", candidate.id),
                    id: candidate.id,
                    source: candidate.source.unwrap_or(context::Source::File),
                    snippet: candidate.description,
                })
                .collect(),
        };
        let result = select_optional_context(state, &request, &state.startup_config, cancel).await;
        let admitted_bytes: usize = request
            .candidates
            .iter()
            .filter(|candidate| !candidate.protected && result.included_ids.contains(&candidate.id))
            .map(|candidate| candidate.snippet.len())
            .sum();
        assert!(admitted_bytes <= request.budget_bytes.unwrap());
        assert!(
            request
                .candidates
                .iter()
                .filter(|candidate| candidate.protected)
                .all(|candidate| result.included_ids.contains(&candidate.id))
        );
        println!(
            "context-admission {} ids={:?} bytes={admitted_bytes} budget={}",
            case.id,
            result.included_ids,
            request.budget_bytes.unwrap()
        );
        QualityOutput::Ranking {
            ids: result.ordered_ids,
        }
    } else {
        let request = selection::SelectionRequest {
            purpose: selection::Purpose::Recommendations,
            task: input.task,
            candidates: input
                .candidates
                .into_iter()
                .map(|candidate| selection::Candidate {
                    protected: input.protected.contains(&candidate.id),
                    id: candidate.id,
                    description: candidate.description,
                })
                .collect(),
        };
        // This is the same host orchestration used by the normal skill shortlist consumer.
        let candidates = request
            .candidates
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let ids = optional_selection_order(
            state,
            &request.task,
            "recommendations",
            candidates,
            &state.startup_config,
            cancel,
        )
        .await
        .unwrap_or_else(|| {
            request
                .candidates
                .iter()
                .map(|candidate| candidate.id.clone())
                .collect()
        });
        QualityOutput::Ranking { ids }
    }
}

async fn capture(
    state: &ServerState,
    case: &quality::QualityCase,
    cancel: &TurnCancelState,
) -> QualityOutput {
    match case.feature {
        Feature::Recommendations | Feature::ContextReranking => ranking(state, case, cancel).await,
        Feature::ToolResultTriage => {
            use bcode_loop_plugin::tool_result_triage as triage;
            #[derive(Deserialize)]
            struct Input {
                tool: String,
                settled_success: bool,
                excerpt: String,
            }
            let input: Input = serde_json::from_value(case.input.clone()).unwrap();
            let response: triage::TriageResponse = service(
                state,
                triage::INTERFACE_ID,
                triage::OP_TRIAGE,
                &triage::TriageRequest {
                    version: 1,
                    invocation_id: case.id.clone(),
                    tool_name: input.tool,
                    is_error: !input.settled_success,
                    partial: false,
                    excerpt_truncated: false,
                    output_excerpt: input.excerpt,
                },
                cancel,
            )
            .await
            .unwrap_or_default();
            response
                .advisory
                .map_or(QualityOutput::Absent, |advice| QualityOutput::Labels {
                    values: BTreeMap::from([(
                        "category".into(),
                        match advice.category {
                            triage::Category::Auth => "permission",
                            triage::Category::Inconclusive => "uncertain",
                            triage::Category::Code => "code",
                            triage::Category::Environment => "environment",
                            triage::Category::Transient => "transient",
                            triage::Category::Partial => "partial",
                        }
                        .into(),
                    )]),
                })
        }
        Feature::ProgressMonitoring => {
            use bcode_loop_plugin::session_progress as progress;
            let fixtures: BTreeMap<String, progress::ProgressRequest> = serde_json::from_str(
                include_str!("../../../eval/fixtures/automatic-quality-progress-v1.json"),
            )
            .unwrap();
            let response: Option<progress::ProgressResponse> = service(
                state,
                progress::INTERFACE_ID,
                progress::OP_OBSERVE,
                &fixtures[&case.id],
                cancel,
            )
            .await;
            response
                .and_then(|response| response.advice)
                .map_or(QualityOutput::Absent, |advice| QualityOutput::Labels {
                    values: BTreeMap::from([(
                        "state".into(),
                        match advice.kind {
                            progress::ProgressAdviceKind::Drift => "drift",
                            progress::ProgressAdviceKind::RepeatedFailure => "stall",
                        }
                        .into(),
                    )]),
                })
        }
        Feature::SessionCategorization => {
            let response: Option<bcode_session_models::CategorizationResponse> = service(
                state,
                bcode_session_models::CATEGORIZATION_INTERFACE_ID,
                bcode_session_models::OP_CATEGORIZE,
                &bcode_session_models::CategorizationRequest {
                    session_id: SessionId::new(),
                    source_sequence: 1,
                    newest_request: case.input["newest_request"].as_str().unwrap().into(),
                },
                cancel,
            )
            .await;
            response
                .and_then(|response| response.categorization)
                .map_or(QualityOutput::Absent, |labels| QualityOutput::Labels {
                    values: BTreeMap::from([
                        ("task".into(), labels.task),
                        ("domain".into(), labels.domain),
                    ]),
                })
        }
        Feature::EvalCategorization => capture_eval(state, case).await,
    }
}

#[tokio::test]
async fn six_consumer_comparative_capture() {
    let _guard = HARNESS_LOCK.lock().await;
    let suite = quality::representative_quality_suite().unwrap();
    let mut observations = Vec::new();
    for case in &suite.cases {
        let mut baseline = None;
        for arm in [
            QualityArm::GlobalOff,
            QualityArm::FeatureOff,
            QualityArm::AutoNoKey,
            QualityArm::AutoError,
            QualityArm::AutoEligible,
        ] {
            let state = state(arm, case.feature);
            let output = capture(&state, case, &TurnCancelState::default()).await;
            let calls = CALLS.lock().unwrap().clone();
            if arm == QualityArm::GlobalOff {
                baseline = Some(output.clone());
            }
            if matches!(arm, QualityArm::GlobalOff | QualityArm::FeatureOff) {
                assert_eq!(
                    calls.discovery + calls.verification + calls.inference,
                    0,
                    "{} {arm:?}",
                    case.id
                );
            }
            if arm != QualityArm::AutoEligible {
                assert_eq!(Some(&output), baseline.as_ref(), "{} {arm:?}", case.id);
            }
            observations.push(QualityObservation {
                case_id: case.id.clone(),
                arm,
                path: QualityPath::DomainConsumer,
                operation: "server.tests.judgement_quality.six_consumer_comparative_capture".into(),
                output,
                calls,
                // No session history exists in this service harness. Do not invent authority/persistence evidence.
                authority_preserved: false,
                request_only: false,
            });
            drop(state);
        }
    }
    let report = quality::compare_quality(&suite, &observations).unwrap();
    println!(
        "six-consumer-summary captures={} cases={} eligible_pairs={} gaps={} mean_delta={:?}",
        observations.len(),
        suite.cases.len(),
        report.eligible_pairs,
        report.gaps.len(),
        report.mean_accuracy_delta
    );
    assert_eq!(observations.len(), suite.cases.len() * 5);
    assert_eq!(
        report.eligible_pairs, 0,
        "Domain captures must not claim application closure"
    );
}

fn isolate_feature(config: &mut bcode_config::JudgementConfig, arm: QualityArm, feature: Feature) {
    // Isolate the experimental consumer; ordinary turns can trigger multiple defaults.
    // Counters remain aggregate and unfiltered, so off still proves zero provider work.
    config.mode = if arm == QualityArm::GlobalOff {
        bcode_config::JudgementMode::Off
    } else {
        bcode_config::JudgementMode::Auto
    };
    config.tool_result_triage = bcode_config::JudgementMode::Off;
    config.context_reranking = bcode_config::JudgementMode::Off;
    config.progress_monitoring = bcode_config::JudgementMode::Off;
    config.recommendations = bcode_config::JudgementMode::Off;
    config.session_categorization = bcode_config::JudgementMode::Off;
    config.eval_categorization = bcode_config::JudgementMode::Off;
    let mode = match feature {
        Feature::ToolResultTriage => &mut config.tool_result_triage,
        Feature::ContextReranking => &mut config.context_reranking,
        Feature::ProgressMonitoring => &mut config.progress_monitoring,
        Feature::Recommendations => &mut config.recommendations,
        Feature::SessionCategorization => &mut config.session_categorization,
        Feature::EvalCategorization => &mut config.eval_categorization,
    };
    if arm != QualityArm::FeatureOff {
        *mode = bcode_config::JudgementMode::Auto;
    }
}

async fn capture_eval(state: &ServerState, case: &quality::QualityCase) -> QualityOutput {
    let oracle = &case.input["oracle"];
    let mut diagnosis = bcode_eval::diagnosis::classify(
        oracle,
        &serde_json::json!([]),
        &serde_json::json!({}),
        &BTreeMap::new(),
    );
    if let Some(input) = bcode_eval::diagnosis::optional_request(oracle, &diagnosis) {
        let query = input.request.clone();
        let response = state.optional_judgement.judge(state, input).await;
        bcode_eval::diagnosis::attach_advisory(&mut diagnosis, &query, response);
    }
    diagnosis["advisory_model_labels"]["failure_category"]
        .as_str()
        .map_or(QualityOutput::Absent, |label| QualityOutput::Labels {
            values: BTreeMap::from([("failure_category".into(), label.into())]),
        })
}

fn configure_probabilities(state: &mut ServerState, probabilities: &BTreeMap<String, f64>) {
    state
        .startup_config
        .auth
        .profiles
        .get_mut("quality-offline")
        .unwrap()
        .settings
        .insert(
            "env.FAKE_JUDGEMENT_YES_NO_PROBABILITIES".into(),
            serde_json::to_string(probabilities).unwrap(),
        );
}

#[tokio::test]
async fn normal_tool_catalog_recommendations_capture_original_permutation() {
    let _guard = HARNESS_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    let mut baseline: Option<Vec<String>> = None;
    for arm in [
        QualityArm::GlobalOff,
        QualityArm::FeatureOff,
        QualityArm::AutoNoKey,
        QualityArm::AutoError,
        QualityArm::AutoEligible,
    ] {
        REQUESTS.lock().unwrap().clear();
        let mut state = state(arm, Feature::Recommendations);
        configure_normal_turn(&mut state);
        if let Some(original) = &baseline {
            let probabilities = original
                .iter()
                .take(32)
                .enumerate()
                .map(|(index, id): (usize, &String)| {
                    (
                        format!("candidate_{index}"),
                        if id == "filesystem.grep" { 0.9 } else { 0.1 },
                    )
                })
                .collect();
            configure_probabilities(&mut state, &probabilities);
        }
        let session = create_capture_session(&state, root.path()).await;
        let completion = run_capture_turn(
            &state,
            session,
            "Find the release_session_ownership symbol across the repository using filesystem search.".into(),
        )
        .await;
        assert_eq!(completion.outcome, ModelTurnOutcome::Completed);
        let request = REQUESTS.lock().unwrap()[0].clone();
        let ids = request
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        assert!(ids.len() >= 2);
        let calls = CALLS.lock().unwrap().clone();
        assert_call_policy(Feature::Recommendations, arm, &calls);
        if let Some(original) = &baseline {
            assert_eq!(
                ids.iter().collect::<BTreeSet<_>>(),
                original.iter().collect::<BTreeSet<_>>(),
                "recommendation cannot change available tools"
            );
            if arm == QualityArm::AutoEligible {
                assert_eq!(ids[0], "filesystem.grep");
                assert!(
                    original
                        .iter()
                        .position(|id| id == "filesystem.grep")
                        .unwrap()
                        > 0,
                    "fixture must measure a real ranking improvement"
                );
            } else {
                assert_eq!(
                    &ids, original,
                    "unavailable advice preserves exact original order"
                );
            }
        } else {
            baseline = Some(ids.clone());
        }
        drop(state);
        println!(
            "normal tool recommendations {arm:?} calls={calls:?} original_ids={ids:?}; synthetic relevant grep-tool ranking, live improvement unproven"
        );
    }
}

#[tokio::test]
async fn context_client_boundary_ranks_before_constrained_admission() {
    use bcode_session_models::{
        ContextSelectionRequest, JudgementContextCandidate, JudgementContextSource,
    };
    let _guard = HARNESS_LOCK.lock().await;
    let candidates = vec![
        JudgementContextCandidate {
            id: "unrelated".into(),
            source: JudgementContextSource::Memory,
            provenance: "fixture:unrelated".into(),
            snippet: "unrelated formatting preference".into(),
            protected: false,
        },
        JudgementContextCandidate {
            id: "lease".into(),
            source: JudgementContextSource::History,
            provenance: "fixture:lease".into(),
            snippet: "release database handle with lease".into(),
            protected: false,
        },
    ];
    let request = ContextSelectionRequest {
        task: "Fix released lease retaining database locks".into(),
        candidates: candidates.clone(),
        budget_bytes: Some(36),
    };
    for arm in [
        QualityArm::GlobalOff,
        QualityArm::FeatureOff,
        QualityArm::AutoNoKey,
        QualityArm::AutoError,
        QualityArm::AutoEligible,
    ] {
        let state = Arc::new({
            let mut state = state(arm, Feature::ContextReranking);
            configure_probabilities(
                &mut state,
                &BTreeMap::from([("candidate_0".into(), 0.1), ("candidate_1".into(), 0.9)]),
            );
            state
        });
        let socket = tempfile::tempdir().unwrap();
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket.path().join("context.sock"));
        let listener = LocalIpcListener::bind(&endpoint).unwrap();
        let (shutdown, stopped) = oneshot::channel();
        let server = tokio::spawn(serve_runtime_test_clients(
            listener,
            Arc::clone(&state),
            stopped,
        ));
        let client = bcode_client::BcodeClient::new(endpoint);
        let result = client.select_optional_context(&request).await.unwrap();
        shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        let calls = CALLS.lock().unwrap().clone();
        assert_call_policy(Feature::ContextReranking, arm, &calls);
        let expected = if arm == QualityArm::AutoEligible {
            "lease"
        } else {
            "unrelated"
        };
        assert_eq!(result.included_ids, vec![expected]);
        assert_eq!(result.applied, arm == QualityArm::AutoEligible);
        let admitted = candidates
            .iter()
            .filter(|candidate| result.included_ids.contains(&candidate.id))
            .collect::<Vec<_>>();
        assert!(
            admitted
                .iter()
                .map(|candidate| candidate.snippet.len())
                .sum::<usize>()
                <= 36
        );
        assert_eq!(
            request.candidates, candidates,
            "original producer context must not be rewritten"
        );
        drop(state);
        println!(
            "context client IPC {arm:?} calls={calls:?} output={result:?}; measured synthetic prebudget relevance, live improvement unproven"
        );
    }
}

#[tokio::test]
async fn eval_diagnosis_client_boundary_preserves_measured_failure() {
    let _guard = HARNESS_LOCK.lock().await;
    let suite = quality::representative_quality_suite().unwrap();
    let case = suite
        .cases
        .iter()
        .find(|case| case.feature == Feature::EvalCategorization)
        .unwrap();
    let oracle = &case.input["oracle"];
    for arm in [
        QualityArm::GlobalOff,
        QualityArm::FeatureOff,
        QualityArm::AutoNoKey,
        QualityArm::AutoError,
        QualityArm::AutoEligible,
    ] {
        let state = Arc::new(state(arm, Feature::EvalCategorization));
        let socket = tempfile::tempdir().unwrap();
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket.path().join("eval.sock"));
        let listener = LocalIpcListener::bind(&endpoint).unwrap();
        let (shutdown, stopped) = oneshot::channel();
        let server = tokio::spawn(serve_runtime_test_clients(
            listener,
            Arc::clone(&state),
            stopped,
        ));
        let client = bcode_client::BcodeClient::new(endpoint);
        let mut diagnosis = bcode_eval::diagnosis::classify(
            oracle,
            &serde_json::json!([]),
            &serde_json::json!({}),
            &BTreeMap::new(),
        );
        let baseline = diagnosis.clone();
        let optional = bcode_eval::diagnosis::optional_request(oracle, &diagnosis).unwrap();
        let result = client.optional_judge(&optional).await.unwrap();
        bcode_eval::diagnosis::attach_advisory(&mut diagnosis, &optional.request, result);
        shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        let calls = CALLS.lock().unwrap().clone();
        assert_call_policy(Feature::EvalCategorization, arm, &calls);
        assert_eq!(diagnosis["labels"], baseline["labels"]);
        assert_eq!(
            diagnosis["root_cause_verified"],
            baseline["root_cause_verified"]
        );
        assert_eq!(
            diagnosis["advisory_model_labels"].is_object(),
            arm == QualityArm::AutoEligible
        );
        drop(state);
        println!(
            "eval diagnosis client IPC {arm:?} calls={calls:?} diagnosis={diagnosis}; measured facts unchanged, live improvement unproven"
        );
    }
}

#[tokio::test]
async fn cancelled_context_admission_and_stale_workflow_advice_make_no_calls() {
    let _guard = HARNESS_LOCK.lock().await;
    let state = state(QualityArm::AutoEligible, Feature::ContextReranking);
    let suite = quality::representative_quality_suite().unwrap();
    let case = suite
        .cases
        .iter()
        .find(|case| case.feature == Feature::ContextReranking)
        .unwrap();
    let cancel = TurnCancelState::default();
    cancel.close();
    let output = capture(&state, case, &cancel).await;
    assert!(matches!(output, QualityOutput::Ranking { .. }));
    assert_no_calls();
    drop(state);
    let second = self::state(QualityArm::AutoEligible, Feature::ProgressMonitoring);
    let mut foreign = test_workflow_execution_authority();
    foreign.daemon_instance_id = "foreign-fixture-owner".into();
    let fixtures: BTreeMap<String, bcode_loop_plugin::session_progress::ProgressRequest> =
        serde_json::from_str(include_str!(
            "../../../eval/fixtures/automatic-quality-progress-v1.json"
        ))
        .unwrap();
    let request = serde_json::to_value(fixtures.values().next().unwrap()).unwrap();
    let reply = invoke_workflow_advisory_fenced(
        &second,
        "absent-fixture-run",
        &foreign,
        &request,
        &second.startup_config,
    )
    .await;
    assert!(reply.is_none());
    assert_no_calls();
    assert!(second.workflow_progress_windows.lock().unwrap().is_empty());
    drop(second);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_flight_context_cancellation_drops_late_judgement() {
    let _guard = HARNESS_LOCK.lock().await;
    let mut state = state(QualityArm::AutoEligible, Feature::ContextReranking);
    configure_probabilities(
        &mut state,
        &BTreeMap::from([("candidate_0".into(), 0.1), ("candidate_1".into(), 0.9)]),
    );
    let request = bcode_skills_plugin::context_selection::SelectionRequest {
        task: "Find database ownership release evidence".into(),
        budget_bytes: Some(36),
        candidates: vec![
            bcode_skills_plugin::context_selection::Candidate {
                id: "unrelated".into(),
                source: bcode_skills_plugin::context_selection::Source::Memory,
                provenance: "fixture:unrelated".into(),
                snippet: "unrelated formatting preference".into(),
                protected: false,
            },
            bcode_skills_plugin::context_selection::Candidate {
                id: "lease".into(),
                source: bcode_skills_plugin::context_selection::Source::History,
                provenance: "fixture:lease".into(),
                snippet: "release the database lease".into(),
                protected: false,
            },
        ],
    };
    let gate = Arc::new(InferenceGate::default());
    *INFERENCE_GATE.lock().unwrap() = Some(Arc::clone(&gate));
    let release = ReleaseInference(Arc::clone(&gate));
    let cancel = TurnCancelState::default();
    {
        let operation = select_optional_context(&state, &request, &state.startup_config, &cancel);
        tokio::pin!(operation);
        tokio::select! {
            () = gate.wait_for(&gate.entered) => {},
            _ = &mut operation => panic!("admission finished before offline inference gate"),
        }
        cancel.cancel().await;
        let result = tokio::time::timeout(Duration::from_secs(2), &mut operation)
            .await
            .expect("cancelled host must not wait for synchronous provider");
        assert!(!result.applied);
        assert_eq!(result.included_ids, vec!["unrelated"]);
        assert_eq!(CALLS.lock().unwrap().inference, 1);
        assert!(!gate.returned.load(Ordering::SeqCst));
        gate.release();
        gate.wait_for(&gate.returned).await;
        assert_eq!(
            JUDGEMENT_CAPTURES.lock().unwrap().len(),
            1,
            "provider produced a late normalized answer"
        );
        assert!(!result.applied, "late result cannot reopen admission");
        assert_eq!(result.included_ids, vec!["unrelated"]);
    }
    drop(state);
    drop(release);
}

async fn admit_quality_authority_run(
    state: &ServerState,
    root: &std::path::Path,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
) -> (
    switchy_database::rusqlite::RusqliteDatabase,
    bcode_workflow_store::NewWorkflowRun,
) {
    use bcode_workflow_store::representation::{self as rep, attempts::owner_boundary};
    let prompt: bcode_workflow::WorkflowPromptConfiguration =
        serde_json::from_value(test_workflow_prompt_configuration(
            bcode_workflow::ValueSchema::of::<u32>(),
            bcode_workflow::PromptContextTarget::FreshIsolated,
        ))
        .expect("existing typed prompt fixture");
    let definition = bcode_workflow::WorkflowBuilder::new(
        "quality-authority",
        bcode_workflow::Step::<u32, u32>::agent("worker", &prompt).expect("typed agent fixture"),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        "quality-authority",
        &definition,
    )
    .unwrap();
    let parent = state
        .sessions
        .create_session(Some("quality-authority-parent".into()), root.to_path_buf())
        .await
        .expect("real fixture parent session");
    let run = bcode_workflow_store::NewWorkflowRun {
        run_id: "quality-authority".into(),
        definition_id: identity.definition_id,
        definition_version: identity.definition_version,
        workspace_snapshot: root.display().to_string(),
        parent_session_id: Some(parent.id.to_string()),
        parent_session_generation: None,
        binding: None,
        authored_provenance: None,
        input: Some(serde_json::json!(1)),
        execution_authority: Some(authority.clone()),
        created_at_ms: 1,
        authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
            version: 1,
            provider_id: "fixture".into(),
            profile_id: "build".into(),
            policy_digest_sha256: "a".repeat(64),
        },
        authorization_ceiling: bcode_workflow::WorkflowToolCapability::ReadOnly,
        limits: bcode_workflow::WorkflowRunLimits::default(),
    };
    let db = switchy_database_connection::open_sqlite_rusqlite(
        &workflow_store_path(state),
        &switchy_database_connection::SqliteOpenOptions {
            access: switchy_database_connection::SqliteAccess::ReadWrite,
            allow_uri: false,
            busy_timeout: Duration::ZERO,
            foreign_keys: true,
            connections: std::num::NonZeroUsize::MIN,
        },
    )
    .expect("typed fixture database");
    rep::install(&db).await.unwrap();
    rep::admit(&db, &run, &definition).await.unwrap();
    owner_boundary::verify(&db, &run.run_id, authority)
        .await
        .unwrap();
    (db, run)
}

async fn quality_handoff_target(
    db: &dyn switchy_database::Database,
    run: &bcode_workflow_store::NewWorkflowRun,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
    record: &bcode_daemon_lifecycle::DaemonRecord,
    evidence_root: &std::path::Path,
) -> (
    bcode_workflow_store::representation::safety::AuthorityHandoff,
    EndedFixtureOwner,
    bcode_daemon_lifecycle::ExecutionLifetime,
) {
    use bcode_workflow_store::representation::safety;
    let transition = safety::AuthorityHandoff {
        run_id: run.run_id.clone(),
        expected: authority.clone(),
        replacement: bcode_workflow_store::WorkflowExecutionAuthority {
            daemon_instance_id: "quality-replacement".into(),
            generation: 2,
            fencing_token: "quality-replacement-fence".into(),
            ..authority.clone()
        },
    };
    let replacement_record = bcode_daemon_lifecycle::DaemonRecord::current(
        &bcode_ipc::default_endpoint(),
        evidence_root.join("replacement.log"),
        None,
        transition.replacement.daemon_instance_id.clone(),
    )
    .unwrap();
    assert_eq!(
        replacement_record.artifact_id.as_ref().unwrap().as_str(),
        transition.replacement.target_artifact_id
    );
    assert_eq!(
        replacement_record.state_location_id,
        record.state_location_id
    );
    let replacement_lifetime =
        bcode_daemon_lifecycle::ExecutionLifetime::begin(evidence_root, &replacement_record)
            .unwrap();
    let resolver = EndedFixtureOwner {
        root: evidence_root.into(),
        location: record
            .state_location_id
            .clone()
            .expect("fresh exact fixture daemon record has state location"),
        transition: transition.clone(),
    };
    assert!(
        safety::handoff_authority(db, &transition, &resolver)
            .await
            .is_err(),
        "live exact source cannot be stolen"
    );
    (transition, resolver, replacement_lifetime)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_response_workflow_authority_transfer_drops_advice() {
    use bcode_workflow_store::representation::{attempts::owner_boundary, safety};
    let _guard = HARNESS_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    let mut state = state(QualityArm::AutoEligible, Feature::ProgressMonitoring);
    state.workflow_store = Arc::new(std::sync::Mutex::new(
        bcode_workflow_store::WorkflowStore::create_coexistence_baseline(root.path()).unwrap(),
    ));
    let record = bcode_daemon_lifecycle::DaemonRecord::current(
        &bcode_ipc::default_endpoint(),
        root.path().join("daemon.log"),
        None,
        state.daemon_status.instance_id.clone(),
    )
    .unwrap();
    state
        .daemon_status
        .artifact_id
        .clone_from(&record.artifact_id);
    let authority = bcode_workflow_store::WorkflowExecutionAuthority {
        target_artifact_id: workflow_operations::current_artifact_id(&state),
        daemon_instance_id: state.daemon_status.instance_id.clone(),
        generation: 1,
        fencing_token: "quality-source-fence".into(),
    };
    let (db, run) = admit_quality_authority_run(&state, root.path(), &authority).await;
    let evidence_root = tempfile::tempdir().unwrap();
    assert_eq!(
        owner_boundary::execution_authority(&db, &run.run_id)
            .await
            .unwrap(),
        Some(authority.clone()),
        "fixture daemon identity must match durable source authority"
    );
    assert_eq!(
        record.artifact_id.as_ref().unwrap().as_str(),
        authority.target_artifact_id
    );
    let lifetime =
        bcode_daemon_lifecycle::ExecutionLifetime::begin(evidence_root.path(), &record).unwrap();
    let (transition, resolver, replacement_lifetime) =
        quality_handoff_target(&db, &run, &authority, &record, evidence_root.path()).await;
    let fixtures: BTreeMap<String, bcode_loop_plugin::session_progress::ProgressRequest> =
        serde_json::from_str(include_str!(
            "../../../eval/fixtures/automatic-quality-progress-v1.json"
        ))
        .unwrap();
    let request = serde_json::to_value(fixtures.values().next().unwrap()).unwrap();
    let gate = Arc::new(InferenceGate::default());
    *INFERENCE_GATE.lock().unwrap() = Some(Arc::clone(&gate));
    let release = ReleaseInference(Arc::clone(&gate));
    {
        let operation = invoke_workflow_advisory_fenced(
            &state,
            &run.run_id,
            &authority,
            &request,
            &state.startup_config,
        );
        tokio::pin!(operation);
        tokio::select! { () = gate.wait_for(&gate.entered) => {}, _ = &mut operation => panic!("advice finished before gate") }
        drop(lifetime);
        safety::handoff_authority(&db, &transition, &resolver)
            .await
            .unwrap();
        owner_boundary::verify(&db, &run.run_id, &transition.replacement)
            .await
            .unwrap();
        assert!(
            owner_boundary::verify(&db, &run.run_id, &authority)
                .await
                .is_err()
        );
        let reply = tokio::time::timeout(Duration::from_secs(2), &mut operation)
            .await
            .unwrap();
        assert!(reply.is_none());
        assert!(!gate.returned.load(Ordering::SeqCst));
        gate.release();
        gate.wait_for(&gate.returned).await;
        assert_eq!(CALLS.lock().unwrap().inference, 1);
        assert_eq!(JUDGEMENT_CAPTURES.lock().unwrap().len(), 1);
    }
    assert!(state.workflow_progress_windows.lock().unwrap().is_empty());
    drop(state);
    assert_eq!(
        owner_boundary::execution_authority(&db, &run.run_id)
            .await
            .unwrap(),
        Some(transition.replacement)
    );
    drop(release);
    drop(replacement_lifetime);
}

struct EndedFixtureOwner {
    root: PathBuf,
    location: String,
    transition: bcode_workflow_store::representation::safety::AuthorityHandoff,
}
impl bcode_workflow_store::representation::safety::AuthorityHandoffResolver for EndedFixtureOwner {
    fn verify<'a>(
        &'a self,
        transition: &'a bcode_workflow_store::representation::safety::AuthorityHandoff,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        bcode_workflow_store::representation::safety::HandoffEvidence,
                        bcode_workflow_store::representation::RepresentationError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            use bcode_workflow_store::representation::safety::HandoffEvidence;
            if transition != &self.transition
                || transition.replacement.target_artifact_id
                    != transition.expected.target_artifact_id
                || bcode_daemon_lifecycle::execution_lifetime_status(
                    &self.root,
                    &transition.replacement.daemon_instance_id,
                    &transition.replacement.target_artifact_id,
                    &self.location,
                ) != bcode_daemon_lifecycle::ExecutionLifetimeStatus::Live
                || bcode_daemon_lifecycle::execution_lifetime_status(
                    &self.root,
                    &transition.expected.daemon_instance_id,
                    &transition.expected.target_artifact_id,
                    &self.location,
                ) != bcode_daemon_lifecycle::ExecutionLifetimeStatus::Released
            {
                return Ok(HandoffEvidence::Unknown);
            }
            Ok(HandoffEvidence::Verified {
                transition: Box::new(transition.clone()),
                receipt_id: "quality-verified-exact-ended-lifetime".into(),
                verified_at_ms: 2,
            })
        })
    }
}

fn assert_no_calls() {
    let calls = CALLS.lock().unwrap().clone();
    assert_eq!(calls.discovery + calls.verification + calls.inference, 0);
}

fn assert_call_policy(feature: Feature, arm: QualityArm, calls: &QualityCalls) {
    if matches!(arm, QualityArm::GlobalOff | QualityArm::FeatureOff) {
        assert_eq!(
            calls.discovery + calls.verification + calls.inference,
            0,
            "{feature:?} {arm:?}: {calls:?}"
        );
    }
    if arm == QualityArm::AutoNoKey {
        assert_eq!(calls.inference, 0, "{feature:?} {arm:?}");
    }
    if arm == QualityArm::AutoEligible {
        assert!(calls.inference > 0, "eligible {feature:?}: {calls:?}");
    }
}
