//! Generic milestone operation coordination; policy is provided by the disableable plugin.

use super::{ServerState, current_time_ms};
use bcode_plugin::PluginInvocationScope;
use bcode_plugin_sdk::ServiceCancellation;
use bcode_plugin_sdk::milestones::{
    AuxiliaryGenerationRequest, AuxiliaryGenerationResult, MilestonesHostRequest,
    MilestonesHostResponse,
};
use bcode_session_models::{SessionEventKind, SessionId};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const POLICY_INTERFACE: &str = "bcode.session-milestones/v1";

/// Bounded non-queuing operation admission, independent of feature trigger policy.
#[derive(Default)]
struct Admission {
    active: Mutex<BTreeSet<SessionId>>,
}

struct Permit {
    admission: Arc<Admission>,
    session_id: SessionId,
}

impl Admission {
    fn acquire(self: &Arc<Self>, session_id: SessionId, limit: usize) -> Result<Permit, String> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| "milestone scheduler unavailable")?;
        if limit == 0 || active.len() >= limit || active.contains(&session_id) {
            return Err("milestone operation busy".into());
        }
        active.insert(session_id);
        drop(active);
        Ok(Permit {
            admission: Arc::clone(self),
            session_id,
        })
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if let Ok(mut active) = self.admission.active.lock() {
            active.remove(&self.session_id);
        }
    }
}

fn scheduler() -> &'static Arc<Admission> {
    static ADMISSION: OnceLock<Arc<Admission>> = OnceLock::new();
    ADMISSION.get_or_init(|| Arc::new(Admission::default()))
}

/// Invoke plugin-owned typed policy through the ordinary cancellable service boundary.
/// Missing or ambiguous routes fail closed before any provider work.
async fn policy(
    state: &ServerState,
    session_id: SessionId,
    request: serde_json::Value,
    cancellation: &ServiceCancellation,
) -> Result<serde_json::Value, String> {
    if cancellation.is_cancelled() {
        return Err("milestone operation cancelled".into());
    }
    let plugin_id = state
        .plugins
        .registry()
        .service_registry()
        .unique_provider(POLICY_INTERFACE)
        .map_err(|_| "milestone generation plugin unavailable")?
        .to_owned();
    state
        .plugins
        .invoke_service_json_scoped_cancellable(
            &plugin_id,
            POLICY_INTERFACE,
            "request",
            &request,
            PluginInvocationScope::session(session_id.to_string()),
            Duration::from_secs(5),
            cancellation,
        )
        .await
        .map_err(|error| {
            bcode_model_provider_runtime::sanitize_provider_diagnostic(&format!(
                "milestone policy unavailable: {error}"
            ))
        })
}

/// Invoke no-tools inference only after canonical durable admission has succeeded.
/// Returns plugin-validated advisory content, never an authoritative source range or ID.
/// The caller must never call this for a duplicate durable request.
///
/// # Errors
/// Returns normalized errors for unavailable policy or provider, cancellation,
/// invalid preparation, or rejected generated output.
pub async fn infer_admitted(
    state: &ServerState,
    request: AuxiliaryGenerationRequest,
    context: serde_json::Value,
    cancellation: ServiceCancellation,
) -> Result<(serde_json::Value, AuxiliaryGenerationResult), String> {
    let prepared = policy(
        state,
        request.session_id,
        serde_json::json!({
            "operation": "prompt",
            "context": context,
            "max_input_tokens": request.max_input_tokens,
        }),
        &cancellation,
    )
    .await?;
    let system = prepared
        .get("system")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "invalid milestone prompt response".to_owned())?
        .to_owned();
    let user = prepared
        .get("user")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "invalid milestone prompt response".to_owned())?
        .to_owned();
    let session_id = request.session_id;
    let result = super::auxiliary_generation::generate(
        state,
        AuxiliaryGenerationRequest {
            system_prompt: system,
            prompt: user,
            ..request
        },
        cancellation.clone(),
    )
    .await?;
    let validated = policy(
        state,
        session_id,
        serde_json::json!({
            "operation": "validate", "text": &result.text,
        }),
        &cancellation,
    )
    .await?;
    Ok((validated, result))
}

struct Publication {
    session_id: SessionId,
    evaluation_id: String,
    source: bcode_session_models::SessionMilestoneSource,
    input_truncated: bool,
    follow_title: bool,
    config: bcode_config::MilestonesConfig,
}

async fn publish_generated(
    state: &ServerState,
    publication: Publication,
    validated: serde_json::Value,
    result: AuxiliaryGenerationResult,
    cancellation: &ServiceCancellation,
) -> Result<MilestonesHostResponse, String> {
    let Publication {
        session_id,
        evaluation_id,
        source,
        input_truncated,
        follow_title,
        config,
    } = publication;
    if cancellation.is_cancelled() {
        return Err("milestone operation cancelled".into());
    }
    if state.session_config(session_id).await.milestones != config {
        return Err("milestone configuration changed before publication".into());
    }
    state
        .plugins
        .registry()
        .service_registry()
        .unique_provider(POLICY_INTERFACE)
        .map_err(|_| "milestone generation plugin unavailable".to_owned())?;
    let generated = validated
        .get("generated")
        .ok_or("invalid milestone validation response")?;
    match generated.get("kind").and_then(serde_json::Value::as_str) {
        Some("no_milestone") => {
            state
                .sessions
                .complete_session_milestone_evaluation(
                    session_id,
                    bcode_session_models::SessionMilestoneEvaluationCompletion { evaluation_id },
                )
                .await
                .map_err(|_| "milestone completion rejected".to_owned())?;
            Ok(MilestonesHostResponse {
                success: true,
                text: "No new milestone.".into(),
            })
        }
        Some("milestone") => {
            let header = generated
                .get("header")
                .and_then(serde_json::Value::as_str)
                .ok_or("invalid milestone header")?
                .to_owned();
            let description = generated
                .get("description")
                .and_then(serde_json::Value::as_str)
                .ok_or("invalid milestone description")?
                .to_owned();
            let automatic_title = follow_title.then(|| header.clone());
            let text = format!("{header}\n{description}");
            state
                .sessions
                .publish_session_milestone(
                    session_id,
                    bcode_session_models::SessionMilestonePublicationRequest {
                        milestone: bcode_session_models::SessionMilestone {
                            evaluation_id,
                            source,
                            header,
                            description,
                            provenance: bcode_session_models::SessionMilestoneProvenance {
                                provider_id: result.provider_plugin_id,
                                model_id: result.model_id,
                                input_truncated,
                            },
                        },
                        automatic_title,
                    },
                )
                .await
                .map_err(|_| "milestone publication rejected".to_owned())?;
            Ok(MilestonesHostResponse {
                success: true,
                text,
            })
        }
        _ => Err("invalid milestone validation result".into()),
    }
}

// The portable checkpoint carries an admission sequence, not a timestamp. Read
// exactly that canonical event; never replay history or derive time from generated metadata.
async fn latest_attempt_timestamp(
    state: &ServerState,
    session_id: SessionId,
) -> Result<Option<u64>, String> {
    let Some(checkpoint) = state
        .sessions
        .session_milestone_evaluation_checkpoint(session_id)
        .await
        .map_err(|_| "milestone checkpoint unavailable".to_owned())?
    else {
        return Ok(None);
    };
    let window = state
        .sessions
        .session_history_around(
            session_id,
            bcode_session_models::SessionHistoryAroundQuery {
                sequence: checkpoint.admitted_sequence,
                before: 0,
                after: 0,
            },
        )
        .await
        .map_err(|_| "milestone admission timestamp unavailable".to_owned())?;
    let event = window
        .events
        .into_iter()
        .find(|event| event.sequence == checkpoint.admitted_sequence)
        .ok_or("milestone admission event unavailable")?;
    match event.kind {
        SessionEventKind::MilestoneEvaluationRequested { request }
            if request == checkpoint.request =>
        {
            Ok(Some(event.timestamp_ms))
        }
        _ => Err("milestone checkpoint inconsistent".into()),
    }
}

async fn generate(
    state: &ServerState,
    session_id: SessionId,
    explicit: bool,
    cancellation: ServiceCancellation,
) -> Result<MilestonesHostResponse, String> {
    let config = state.session_config(session_id).await.milestones;
    if !config.enabled {
        return Ok(MilestonesHostResponse {
            success: false,
            text: "Milestone generation is disabled. Explicit paid opt-in is required.".into(),
        });
    }
    if !config.valid() {
        return Err("invalid milestone configuration".into());
    }
    let profile = config
        .model_profile
        .as_deref()
        .ok_or_else(|| "milestone model profile required".to_owned())?;
    if state
        .session_config(session_id)
        .await
        .resolved_model_profile(profile)
        .is_none()
    {
        return Err("milestone model profile unavailable".into());
    }
    if cancellation.is_cancelled() {
        return Err("milestone operation cancelled".into());
    }
    let _permit = scheduler().acquire(
        session_id,
        usize::try_from(config.max_concurrent).unwrap_or(0),
    )?;
    // Detect a disabled/ambiguous policy plugin before context or paid provider discovery.
    state
        .plugins
        .registry()
        .service_registry()
        .unique_provider(POLICY_INTERFACE)
        .map_err(|_| "milestone generation plugin unavailable".to_owned())?;
    let context = state
        .sessions
        .session_milestone_context(
            session_id,
            usize::try_from(config.max_input_tokens)
                .unwrap_or(0)
                .saturating_mul(2),
            200,
        )
        .await
        .map_err(|_| "canonical milestone context unavailable".to_owned())?;
    if context.evaluation_count >= config.max_requests_per_session {
        return Ok(MilestonesHostResponse {
            success: false,
            text: "Session milestone request budget exhausted.".into(),
        });
    }
    let Some(source) = context.source.clone() else {
        return Ok(MilestonesHostResponse {
            success: false,
            text: "No eligible settled conversational source.".into(),
        });
    };
    let last_attempt_at_ms = latest_attempt_timestamp(state, session_id).await?;
    let cooldown_at_ms = last_attempt_at_ms
        .into_iter()
        .chain(context.last_evaluation_at_ms)
        .max();
    let elapsed_secs = cooldown_at_ms.map_or(0, |timestamp| {
        current_time_ms().saturating_sub(timestamp) / 1_000
    });
    let decision = policy(
        state,
        session_id,
        serde_json::json!({
            "operation": "decide", "config": &config,
            "facts": { "new_turns": context.new_turns, "new_tokens": context.new_tokens,
                "previously_evaluated": cooldown_at_ms.is_some(),
                "elapsed_secs": elapsed_secs, "explicit": explicit }
        }),
        &cancellation,
    )
    .await?;
    if decision.get("decision").and_then(serde_json::Value::as_str) != Some("due") {
        return Ok(MilestonesHostResponse {
            success: false,
            text: "Milestone evaluation is not due.".into(),
        });
    }
    admit_and_generate(state, session_id, config, context, source, cancellation).await
}

async fn admit_and_generate(
    state: &ServerState,
    session_id: SessionId,
    config: bcode_config::MilestonesConfig,
    context: bcode_session_models::SessionMilestoneContext,
    source: bcode_session_models::SessionMilestoneSource,
    cancellation: ServiceCancellation,
) -> Result<MilestonesHostResponse, String> {
    let profile = config
        .model_profile
        .as_deref()
        .ok_or("milestone model profile required")?;
    let evaluation_id = format!(
        "milestone:{session_id}:{}:{}",
        source.start_sequence, source.end_sequence
    );
    if state
        .sessions
        .session_milestone_evaluation(session_id, &evaluation_id)
        .await
        .map_err(|_| "milestone evaluation status unavailable".to_owned())?
        .is_some()
    {
        return Ok(MilestonesHostResponse {
            success: false,
            text: "Milestone source already admitted; provider call will not be replayed.".into(),
        });
    }
    if cancellation.is_cancelled() {
        return Err("milestone operation cancelled".into());
    }
    let fresh = state
        .sessions
        .request_session_milestone_evaluation(
            session_id,
            bcode_session_models::SessionMilestoneEvaluationRequest {
                evaluation_id: evaluation_id.clone(),
                source: source.clone(),
                expected_generation: context.generation,
                expected_explicit_name: context.explicit_name,
                max_requests_per_session: Some(config.max_requests_per_session),
            },
        )
        .await
        .map_err(|_| "milestone evaluation admission rejected".to_owned())?;
    if !fresh {
        return Ok(MilestonesHostResponse { success: false, text: "Milestone source already evaluated or admitted; provider call will not be replayed.".into() });
    }
    if state.session_config(session_id).await.milestones != config {
        return Err("milestone configuration changed before inference".into());
    }
    let request = AuxiliaryGenerationRequest {
        session_id,
        model_profile: profile.to_owned(),
        system_prompt: String::new(),
        prompt: String::new(),
        max_input_tokens: u64::from(config.max_input_tokens),
        max_output_tokens: config.max_output_tokens,
        timeout_ms: 30_000,
    };
    let (validated, result) = Box::pin(infer_admitted(
        state,
        request,
        serde_json::json!({
            "text": context.text, "truncated": context.truncated,
        }),
        cancellation.clone(),
    ))
    .await?;
    Box::pin(publish_generated(
        state,
        Publication {
            session_id,
            evaluation_id,
            source,
            input_truncated: context.truncated,
            follow_title: config.follow_title,
            config,
        },
        validated,
        result,
        &cancellation,
    ))
    .await
}

/// Handle the plugin command over bounded canonical reads and explicit generation.
///
/// # Errors
/// Returns normalized errors for unavailable canonical state, invalid configuration,
/// cancellation, admission rejection, or failed inference and publication.
pub async fn handle(
    state: &ServerState,
    request: MilestonesHostRequest,
    cancellation: ServiceCancellation,
) -> Result<MilestonesHostResponse, String> {
    match request {
        MilestonesHostRequest::Read { session_id } => {
            if cancellation.is_cancelled() {
                return Err("milestone operation cancelled".into());
            }
            let page = state
                .sessions
                .session_milestones(
                    session_id,
                    bcode_session_models::SessionMilestoneQuery {
                        after_sequence: None,
                        limit: 20,
                    },
                )
                .await
                .map_err(|_| "canonical milestones unavailable".to_owned())?;
            let mut text = String::new();
            for entry in page.milestones {
                use std::fmt::Write as _;
                let milestone = entry.milestone;
                let _ = writeln!(
                    text,
                    "{}\n{}\nSource {}–{} (anchor {})\n",
                    milestone.header,
                    milestone.description,
                    milestone.source.start_sequence,
                    milestone.source.end_sequence,
                    milestone.source.anchor_sequence
                );
            }
            if text.is_empty() {
                text = "No session milestones.".into();
            }
            if let Some(cursor) = page.next_after_sequence {
                use std::fmt::Write as _;
                let _ = writeln!(
                    text,
                    "More milestones available after publication {cursor}."
                );
            }
            Ok(MilestonesHostResponse {
                success: true,
                text,
            })
        }
        MilestonesHostRequest::EvaluateSettled { session_id } => {
            generate(state, session_id, false, cancellation).await
        }
        MilestonesHostRequest::Generate { session_id } => {
            generate(state, session_id, true, cancellation).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Admission, ServerState, ServiceCancellation, SessionEventKind, SessionId, generate,
        latest_attempt_timestamp,
    };
    use bcode_model::{ModelTurnRequest, OP_RUN_TURN, ProviderTurnEvent};
    use bcode_session::SessionManager;
    use bcode_session_models::ClientId;
    use std::collections::BTreeMap;
    use std::{collections::BTreeSet, sync::Arc, time::Duration};

    #[derive(Default)]
    struct FixtureProvider;

    static PIPELINE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    static PROVIDER_CANCELLED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    static PROVIDER_WAITING: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    static PROVIDER_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    const PROVIDER_MANIFEST: &str = "id='bcode.fake-provider'\nname='Milestone fixture'\nversion='0.0.1'\n[[services]]\ninterface_id='bcode.model-provider/v3'\nname='Fixture streaming'\n[[services]]\ninterface_id='bcode.model-provider/v2'\nname='Fixture model discovery'\n[runtime]\ntype='native'\nabi_version=5\nlibrary='unused.dylib'\nevent_symbol='bcode_plugin_handle_event_v1'\n";

    impl bcode_plugin_sdk::RustPlugin for FixtureProvider {
        fn invoke_service(
            &mut self,
            context: bcode_plugin_sdk::NativeServiceContext,
        ) -> bcode_plugin_sdk::ServiceResponse {
            if context.request.operation != OP_RUN_TURN {
                return bcode_plugin_sdk::RustPlugin::invoke_service(
                    &mut bcode_fake_provider_plugin::FakeProviderPlugin::default(),
                    context,
                );
            }
            let request: ModelTurnRequest = context.request.payload_json().unwrap();
            if !request.turn_id.contains("-auxiliary-") {
                return bcode_plugin_sdk::RustPlugin::invoke_service(
                    &mut bcode_fake_provider_plugin::FakeProviderPlugin::default(),
                    context,
                );
            }
            assert!(
                context.bridge.is_available(),
                "auxiliary inference requires the provider custody bridge"
            );
            assert!(request.tools.is_empty());
            assert!(
                request
                    .system_prompt
                    .as_deref()
                    .unwrap()
                    .contains("untrusted")
            );
            PROVIDER_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let text = request
                .provider_context
                .settings
                .get("fixture_output")
                .cloned()
                .unwrap();
            if text == "wait_for_cancel" {
                context.events.emit(
                    &serde_json::to_vec(&ProviderTurnEvent::Usage {
                        usage: bcode_model::TokenUsage {
                            input_tokens: Some(12),
                            output_tokens: Some(3),
                            ..bcode_model::TokenUsage::default()
                        },
                    })
                    .unwrap(),
                );
                PROVIDER_WAITING.store(true, std::sync::atomic::Ordering::SeqCst);
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while !context.cancellation.is_cancelled() && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                PROVIDER_CANCELLED.store(
                    context.cancellation.is_cancelled(),
                    std::sync::atomic::Ordering::SeqCst,
                );
                return bcode_plugin_sdk::ServiceResponse::json(&bcode_model::RunTurnResponse {
                    stop_reason: Some(bcode_model::StopReason::EndTurn),
                })
                .unwrap();
            }
            for event in [
                ProviderTurnEvent::Usage {
                    usage: bcode_model::TokenUsage {
                        input_tokens: Some(12),
                        output_tokens: Some(3),
                        ..bcode_model::TokenUsage::default()
                    },
                },
                ProviderTurnEvent::TextDelta { text },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: bcode_model::StopReason::EndTurn,
                },
            ] {
                context.events.emit(&serde_json::to_vec(&event).unwrap());
            }
            bcode_plugin_sdk::ServiceResponse::json(&bcode_model::RunTurnResponse {
                stop_reason: Some(bcode_model::StopReason::EndTurn),
            })
            .unwrap()
        }
    }

    fn fixture_plugins() -> bcode_plugin::PluginRuntimeHost {
        let plugins = [
            bcode_plugin::StaticBundledPlugin::new(
                PROVIDER_MANIFEST,
                bcode_plugin_sdk::static_plugin_vtable!(FixtureProvider, PROVIDER_MANIFEST),
            ),
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../plugins/session-milestones-plugin/bcode-plugin.toml"),
                bcode_session_milestones_plugin::static_plugin(),
            ),
        ];
        bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection {
                mode: bcode_plugin::PluginSelectionMode::Explicit,
                enabled: BTreeSet::from([
                    "bcode.fake-provider".into(),
                    "bcode.session-milestones".into(),
                ]),
                disabled: BTreeSet::new(),
            },
            &plugins,
        )
        .unwrap()
    }

    async fn settle(state: &ServerState, session_id: SessionId, number: usize) {
        let turn_id = bcode_session_models::TurnId(format!("conversation-{number}"));
        let submission_id = bcode_session_models::SubmissionId::for_queued_turn(&turn_id);
        let client_id = ClientId::new();
        let text = format!("Implement behavior {number}");
        state
            .sessions
            .accept_submission(
                session_id,
                bcode_session_models::PromptSubmission {
                    id: submission_id.clone(),
                    client_id,
                    text: text.clone(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                    requested_placement: bcode_session_models::SubmissionPlacement::FollowUp,
                    target: bcode_session_models::SubmissionTarget::OwnTurn {
                        turn_id: turn_id.clone(),
                    },
                    execution_descriptor: None,
                },
            )
            .await
            .unwrap();
        state
            .sessions
            .activate_submission(session_id, submission_id)
            .await
            .unwrap();
        state
            .sessions
            .append_event(
                session_id,
                SessionEventKind::PositionedAssistantResponseSegment {
                    turn_id: turn_id.0.clone(),
                    output_position: bcode_session_models::TurnOutputPosition::new(0),
                    segment_id: format!("answer-{number}"),
                    segment_order: 0,
                    text: "Implemented and checked behavior.".into(),
                },
            )
            .await
            .unwrap();
        state
            .sessions
            .append_event(
                session_id,
                SessionEventKind::ModelTurnFinished {
                    turn_id: turn_id.0,
                    outcome: bcode_session_models::ModelTurnOutcome::Completed,
                    message: None,
                },
            )
            .await
            .unwrap();
    }

    async fn assert_observed_usage_persisted(state: &ServerState, session_id: SessionId) {
        let checkpoint = state
            .sessions
            .session_milestone_evaluation_checkpoint(session_id)
            .await
            .unwrap()
            .unwrap();
        let window = state
            .sessions
            .session_history_around(
                session_id,
                bcode_session_models::SessionHistoryAroundQuery {
                    sequence: checkpoint.admitted_sequence,
                    before: 0,
                    after: 20,
                },
            )
            .await
            .unwrap();
        assert!(window.events.iter().any(|event| matches!(&event.kind,
            SessionEventKind::ModelUsage { usage, .. } if usage.input_tokens == Some(12) && usage.output_tokens == Some(3))),
            "observed usage must persist despite failed or cancelled generation");
    }

    #[tokio::test]
    async fn paid_pipeline_publishes_checkpoints_and_fences_duplicates_and_budget() {
        let guard = PIPELINE_TEST_LOCK.lock().await;
        let root = tempfile::tempdir().unwrap();
        let sessions = SessionManager::persistent(root.path()).unwrap();
        let session_id = sessions
            .create_session(None, root.path().to_path_buf())
            .await
            .unwrap()
            .id;
        let mut state = super::super::tests::test_server_state(sessions);
        state.plugins = fixture_plugins();
        state.startup_config.model.profiles.insert("small".into(), bcode_config::ModelProfileConfig {
            provider_plugin_id: "bcode.fake-provider".into(), model_id: Some("fake-echo".into()),
            settings: BTreeMap::from([("fixture_output".into(), "{\"kind\":\"milestone\",\"header\":\"Behavior implemented\",\"description\":\"Added tested behavior.\"}".into())]),
            ..bcode_config::ModelProfileConfig::default()
        });
        state.startup_config.milestones = bcode_config::MilestonesConfig {
            enabled: true,
            model_profile: Some("small".into()),
            min_interval_secs: 0,
            max_requests_per_session: 2,
            ..bcode_config::MilestonesConfig::default()
        };
        assert_successful_checkpoints_and_budget(&mut state, session_id).await;
        assert_failed_admission_cooldown_and_no_replay(&mut state, session_id).await;
        assert_running_provider_cancellation(&mut state, session_id).await;
        drop(state);
        drop(guard);
    }

    async fn execute_activated_turn(
        state: &Arc<ServerState>,
        session_id: SessionId,
        number: usize,
    ) -> bcode_session_models::ModelTurnOutcome {
        let turn_id = bcode_session_models::TurnId(format!("actual-{number}"));
        let id = bcode_session_models::SubmissionId::for_queued_turn(&turn_id);
        let client_id = ClientId::new();
        state
            .sessions
            .accept_submission(
                session_id,
                bcode_session_models::PromptSubmission {
                    id: id.clone(),
                    client_id,
                    text: "Implement bounded behavior".into(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                    requested_placement: bcode_session_models::SubmissionPlacement::FollowUp,
                    target: bcode_session_models::SubmissionTarget::OwnTurn { turn_id },
                    execution_descriptor: None,
                },
            )
            .await
            .unwrap();
        let trigger = crate::activated_submission_event(state, session_id, id)
            .await
            .unwrap()
            .unwrap();
        let mut permit = crate::SessionTurnPermit::new(session_id);
        let (_followup, mut followups) = tokio::sync::mpsc::channel(1);
        let (_steering, mut steering) = tokio::sync::mpsc::channel(1);
        let (cancel, mut cancels) = tokio::sync::mpsc::channel(1);
        let queued = std::sync::atomic::AtomicUsize::new(0);
        let mut commands = crate::RuntimeCommandContext::new(
            &mut followups,
            &mut steering,
            &mut cancels,
            &queued,
            Arc::new(tokio::sync::Mutex::new(None)),
        );
        let phase = Arc::new(tokio::sync::Mutex::new(crate::SessionRuntimePhase::Idle));
        let execution = Box::pin(crate::run_model_turn(
            state,
            &mut permit,
            &trigger,
            client_id,
            None,
            &mut commands,
            &phase,
            None,
            false,
        ));
        let cancel_running = async {
            if number == 3 {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while !PROVIDER_WAITING.load(std::sync::atomic::Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
                let (response, _received) = tokio::sync::oneshot::channel();
                cancel
                    .send(crate::CancelCommand {
                        expected_turn_id: None,
                        clear_queue: false,
                        requested_by: Some(client_id),
                        response,
                    })
                    .await
                    .unwrap();
            }
        };
        let (completion, ()) = tokio::join!(execution, cancel_running);
        completion.outcome
    }

    #[tokio::test]
    async fn actual_activated_conversation_publishes_automatic_milestone_after_settlement() {
        let guard = PIPELINE_TEST_LOCK.lock().await;
        let root = tempfile::tempdir().unwrap();
        let sessions = SessionManager::persistent(root.path()).unwrap();
        let session = sessions
            .create_session(None, root.path().to_path_buf())
            .await
            .unwrap();
        // Hold canonical ownership through execution and subsequent evidence reads.
        let evidence_ownership = sessions
            .acquire_session_ownership(session.id, bcode_session::SessionOwnershipKind::RuntimeWork)
            .await
            .unwrap();
        let mut state = Arc::new({
            let mut state = crate::tests::test_server_state(sessions);
            state.plugins = fixture_plugins();
            state.selected_provider_plugin_id = Some("bcode.fake-provider".into());
            state.selected_model_id = Some("fake-echo".into());
            state.startup_config.model.profiles.insert("small".into(), bcode_config::ModelProfileConfig {
            provider_plugin_id: "bcode.fake-provider".into(), model_id: Some("fake-echo".into()),
            settings: BTreeMap::from([("fixture_output".into(), "{\"kind\":\"milestone\",\"header\":\"Behavior implemented\",\"description\":\"Added tested behavior.\"}".into())]),
            ..bcode_config::ModelProfileConfig::default()
        });
            state.startup_config.milestones = bcode_config::MilestonesConfig {
                enabled: true,
                model_profile: Some("small".into()),
                min_interval_secs: 0,
                ..bcode_config::MilestonesConfig::default()
            };
            state
        });
        for number in 0..3 {
            assert_eq!(
                execute_activated_turn(&state, session.id, number).await,
                bcode_session_models::ModelTurnOutcome::Completed
            );
        }
        let page = state
            .sessions
            .session_milestones(
                session.id,
                bcode_session_models::SessionMilestoneQuery {
                    after_sequence: None,
                    limit: 20,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            page.milestones.len(),
            1,
            "real completed activation must reach registry policy, inference and canonical publication"
        );
        assert_eq!(page.milestones[0].milestone.header, "Behavior implemented");
        {
            let state = Arc::get_mut(&mut state)
                .expect("settled turns must release fixture state ownership");
            state.startup_config.milestones.subsequent_turns = 1;
            state
                .startup_config
                .model
                .profiles
                .get_mut("small")
                .unwrap()
                .settings
                .insert("fixture_output".into(), "wait_for_cancel".into());
        }
        PROVIDER_WAITING.store(false, std::sync::atomic::Ordering::SeqCst);
        PROVIDER_CANCELLED.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            execute_activated_turn(&state, session.id, 3).await,
            bcode_session_models::ModelTurnOutcome::Completed,
            "observer cancellation must not rewrite the settled conversational outcome"
        );
        assert!(PROVIDER_CANCELLED.load(std::sync::atomic::Ordering::SeqCst));
        assert_observed_usage_persisted(&state, session.id).await;
        drop(evidence_ownership);
        drop(state);
        drop(guard);
    }

    async fn assert_successful_checkpoints_and_budget(
        state: &mut ServerState,
        session_id: SessionId,
    ) {
        for number in 0..3 {
            settle(state, session_id, number).await;
        }
        let first = generate(state, session_id, false, ServiceCancellation::default())
            .await
            .unwrap();
        assert!(first.success);
        let page = state
            .sessions
            .session_milestones(
                session_id,
                bcode_session_models::SessionMilestoneQuery {
                    after_sequence: None,
                    limit: 20,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.milestones.len(), 1);
        assert_eq!(
            page.milestones[0].milestone.provenance.provider_id,
            "bcode.fake-provider"
        );
        let calls = PROVIDER_CALLS.load(std::sync::atomic::Ordering::SeqCst);
        let repeated = generate(state, session_id, true, ServiceCancellation::default())
            .await
            .unwrap();
        assert!(!repeated.success);
        assert_eq!(
            PROVIDER_CALLS.load(std::sync::atomic::Ordering::SeqCst),
            calls
        );
        state
            .startup_config
            .model
            .profiles
            .get_mut("small")
            .unwrap()
            .settings
            .insert(
                "fixture_output".into(),
                "{\"kind\":\"no_milestone\"}".into(),
            );
        settle(state, session_id, 3).await;
        assert!(
            generate(state, session_id, true, ServiceCancellation::default())
                .await
                .unwrap()
                .success
        );
        let checkpoint = state
            .sessions
            .session_milestone_evaluation_checkpoint(session_id)
            .await
            .unwrap()
            .unwrap();
        assert!(checkpoint.completed);
        settle(state, session_id, 4).await;
        assert!(
            !generate(state, session_id, true, ServiceCancellation::default())
                .await
                .unwrap()
                .success
        );
        assert_eq!(
            PROVIDER_CALLS.load(std::sync::atomic::Ordering::SeqCst),
            calls + 1
        );
        let context = state
            .sessions
            .session_milestone_context(session_id, 8_000, 200)
            .await
            .unwrap();
        assert_eq!(context.evaluation_count, 2);
        assert!(
            context.text.contains("Implement behavior 3"),
            "no-result checkpoint must retain unpublished source"
        );
    }

    async fn assert_failed_admission_cooldown_and_no_replay(
        state: &mut ServerState,
        session_id: SessionId,
    ) {
        state.startup_config.milestones.max_requests_per_session = 4;
        state
            .startup_config
            .model
            .profiles
            .get_mut("small")
            .unwrap()
            .settings
            .insert("fixture_output".into(), "{malformed".into());
        let before = PROVIDER_CALLS.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            generate(state, session_id, true, ServiceCancellation::default())
                .await
                .is_err()
        );
        assert_eq!(
            PROVIDER_CALLS.load(std::sync::atomic::Ordering::SeqCst),
            before + 1
        );
        assert!(
            !generate(state, session_id, true, ServiceCancellation::default())
                .await
                .unwrap()
                .success
        );
        assert_eq!(
            PROVIDER_CALLS.load(std::sync::atomic::Ordering::SeqCst),
            before + 1,
            "malformed/ambiguous requests must not replay"
        );
        assert_observed_usage_persisted(state, session_id).await;
        assert!(
            latest_attempt_timestamp(state, session_id)
                .await
                .unwrap()
                .is_some()
        );
        assert_failed_range_cooldown(state, session_id, before).await;
    }

    async fn assert_failed_range_cooldown(
        state: &mut ServerState,
        session_id: SessionId,
        before: usize,
    ) {
        state.startup_config.milestones.min_interval_secs = 300;
        for number in 10..16 {
            settle(state, session_id, number).await;
        }
        assert!(
            !generate(state, session_id, false, ServiceCancellation::default())
                .await
                .unwrap()
                .success,
            "a failed admission must cool down even a new eligible source range"
        );
        assert_eq!(
            PROVIDER_CALLS.load(std::sync::atomic::Ordering::SeqCst),
            before + 1
        );
        let cancelled = ServiceCancellation::default();
        cancelled.cancel();
        assert!(generate(state, session_id, true, cancelled).await.is_err());
        assert_eq!(
            PROVIDER_CALLS.load(std::sync::atomic::Ordering::SeqCst),
            before + 1
        );
        assert_eq!(
            state
                .sessions
                .session_milestones(
                    session_id,
                    bcode_session_models::SessionMilestoneQuery {
                        after_sequence: None,
                        limit: 20
                    }
                )
                .await
                .unwrap()
                .milestones
                .len(),
            1
        );
    }

    async fn assert_running_provider_cancellation(state: &mut ServerState, session_id: SessionId) {
        state
            .startup_config
            .model
            .profiles
            .get_mut("small")
            .unwrap()
            .settings
            .insert("fixture_output".into(), "wait_for_cancel".into());
        settle(state, session_id, 20).await;
        let cancellation = ServiceCancellation::default();
        let cancel_running = async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !PROVIDER_WAITING.load(std::sync::atomic::Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            cancellation.cancel();
        };
        let (result, ()) = tokio::join!(
            generate(state, session_id, true, cancellation.clone()),
            cancel_running
        );
        assert!(result.is_err());
        assert!(
            PROVIDER_CANCELLED.load(std::sync::atomic::Ordering::SeqCst),
            "cancellation must reach running remote work"
        );
        assert_observed_usage_persisted(state, session_id).await;
    }

    #[tokio::test]
    async fn disabled_missing_selection_and_cancelled_paths_never_need_provider() {
        let mut state = super::super::tests::test_server_state(SessionManager::default());
        let session_id = SessionId::new();
        assert!(
            !generate(&state, session_id, true, ServiceCancellation::default())
                .await
                .unwrap()
                .success
        );
        state.startup_config.milestones.enabled = true;
        assert!(
            generate(&state, session_id, true, ServiceCancellation::default())
                .await
                .is_err()
        );
        state.startup_config.milestones.model_profile = Some("absent".into());
        assert!(
            generate(&state, session_id, true, ServiceCancellation::default())
                .await
                .is_err()
        );
        drop(state);
    }

    #[test]
    fn admission_is_per_session_and_bounded_without_queueing() {
        let admission = Arc::new(Admission::default());
        let first = SessionId::new();
        let second = SessionId::new();
        let third = SessionId::new();
        let permit = admission.acquire(first, 2).unwrap();
        assert!(admission.acquire(first, 2).is_err());
        let other = admission.acquire(second, 2).unwrap();
        assert!(admission.acquire(third, 2).is_err());
        drop(permit);
        assert!(admission.acquire(third, 2).is_ok());
        drop(other);
        assert!(admission.acquire(first, 2).is_ok());
        assert!(admission.acquire(first, 0).is_err());
    }
}
