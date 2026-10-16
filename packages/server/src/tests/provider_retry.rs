use super::*;

fn transient_error(category: bcode_model::ProviderErrorCategory) -> bcode_model::ProviderError {
    bcode_model::ProviderError {
        code: "stream_read_timeout".into(),
        category,
        message: "provider request timed out".into(),
        retryable: true,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    }
}

#[test]
fn transport_timeouts_use_unlimited_capped_exponential_backoff() {
    let config = bcode_config::ModelRetryConfig::default();
    let error = transient_error(bcode_model::ProviderErrorCategory::Timeout);
    let policy = matching_provider_retry_policy(
        &config,
        &error,
        &SessionModelSelection::default(),
        &[],
        &[],
    )
    .expect("retryable transport timeout");
    for (attempt, seconds) in [
        (1, 2),
        (2, 4),
        (3, 8),
        (9, 512),
        (10, 600),
        (100, 600),
        (u64::MAX, 600),
    ] {
        assert!(policy.allows_attempt(attempt));
        assert_eq!(
            provider_retry_delay(&policy, &error, attempt),
            Duration::from_secs(seconds)
        );
    }
    let disabled = bcode_config::ModelRetryConfig {
        enabled: false,
        ..config.clone()
    };
    assert!(
        matching_provider_retry_policy(
            &disabled,
            &error,
            &SessionModelSelection::default(),
            &[],
            &[]
        )
        .is_none()
    );
    let mut permanent = error;
    permanent.retryable = false;
    assert!(
        matching_provider_retry_policy(
            &config,
            &permanent,
            &SessionModelSelection::default(),
            &[],
            &[]
        )
        .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn alternating_timeouts_retry_until_success_without_replaying_tools() {
    let state = test_server_state(SessionManager::default());
    let session = state
        .sessions
        .create_session(None, "/tmp/retry-regression".into())
        .await
        .expect("session");
    let session_id = session.id;
    drop(session);
    let (_followup_tx, mut followup_rx) = mpsc::channel(1);
    let (_steering_tx, mut steering_rx) = mpsc::channel(1);
    let (_cancel_tx, mut cancel_rx) = mpsc::channel(1);
    let queued = AtomicUsize::new(0);
    let mut commands = RuntimeCommandContext::new(
        &mut followup_rx,
        &mut steering_rx,
        &mut cancel_rx,
        &queued,
        Arc::new(Mutex::new(None)),
    );
    let cancel = TurnCancelState::default();
    let phase = Arc::new(Mutex::new(SessionRuntimePhase::ProviderActive));
    let selection = SessionModelSelection::default();
    let config = bcode_config::ModelRetryConfig::default();
    let mut recovery = ModelTurnRecoveryState::default();
    for (index, error) in [
        model_no_progress_timeout_error("no progress".into()),
        transient_error(bcode_model::ProviderErrorCategory::Timeout),
        transient_error(bcode_model::ProviderErrorCategory::Network),
        model_no_progress_timeout_error("no progress".into()),
    ]
    .into_iter()
    .enumerate()
    {
        let outcome = ModelPollOutcome {
            provider_error: Some(error),
            incomplete_tool_calls: vec!["never-executable".into()],
            ..ModelPollOutcome::default()
        };
        let start = tokio::time::Instant::now();
        let result = maybe_retry_after_provider_error(
            &state,
            session_id,
            &outcome,
            &mut recovery,
            ProviderErrorRetryContext {
                trigger_event_sequence: 0,
                turn_id: "timeout-regression",
                selection: &selection,
                provider_retry_rules: &[],
                remote_catalog_retry_rules: &[],
                retry_config: &config,
                cancel_state: &cancel,
                phase: &phase,
                provider_round: u32::try_from(index).expect("small round"),
                completed_tool_rounds: 0,
                segment_order: 0,
                output_position: 0,
                compaction_decision: resolve_compaction_decision(
                    bcode_config::CompactionMode::Off,
                    None,
                ),
            },
            &mut commands,
        )
        .await;
        assert!(matches!(result, ModelTurnRetry::Continue));
        assert_eq!(start.elapsed(), Duration::from_secs(2 << index));
        assert_eq!(outcome.incomplete_tool_calls, ["never-executable"]);
        assert!(outcome.pending_tool_calls.is_empty());
    }
    assert_eq!(recovery.consecutive_retry_attempts, 4);
    assert_eq!(recovery.retry_attempts["builtin.no_progress_timeout"], 2);
    assert_eq!(recovery.retry_attempts["builtin.transient"], 2);
    recovery.record_successful_provider_round();
    assert_eq!(recovery.consecutive_retry_attempts, 0);
    assert!(recovery.retry_attempts.is_empty());
    drop(state);
}

#[tokio::test(start_paused = true)]
async fn maximum_backoff_is_cancellable_even_when_control_channel_is_closed() {
    let state = test_server_state(SessionManager::default());
    let session = state
        .sessions
        .create_session(None, "/tmp/retry-cancellation".into())
        .await
        .expect("session");
    let (_followup_tx, mut followup_rx) = mpsc::channel(1);
    let (_steering_tx, mut steering_rx) = mpsc::channel(1);
    let (cancel_tx, mut cancel_rx) = mpsc::channel(1);
    drop(cancel_tx);
    let queued = AtomicUsize::new(0);
    let mut commands = RuntimeCommandContext::new(
        &mut followup_rx,
        &mut steering_rx,
        &mut cancel_rx,
        &queued,
        Arc::new(Mutex::new(None)),
    );
    let cancel = TurnCancelState::default();
    let error = transient_error(bcode_model::ProviderErrorCategory::Timeout);
    let policy = matching_provider_retry_policy(
        &bcode_config::ModelRetryConfig::default(),
        &error,
        &SessionModelSelection::default(),
        &[],
        &[],
    )
    .expect("policy");
    let start = tokio::time::Instant::now();
    let (result, ()) = tokio::join!(
        retry_after_provider_error(
            &state,
            session.id,
            ("cancel-backoff", 100, 100),
            &error,
            &policy,
            &cancel,
            &mut commands
        ),
        async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            cancel.cancel().await;
        }
    );
    assert!(matches!(
        result,
        ModelTurnRetry::Return(ModelTurnCompletion {
            outcome: ModelTurnOutcome::Cancelled,
            ..
        })
    ));
    assert_eq!(start.elapsed(), Duration::from_secs(1));
    drop(session);
    drop(state);
}
