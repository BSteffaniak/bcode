//! Side-effect-free host preparation retry policy. Never wrap invocation or persistence here.

use std::{future::Future, time::Duration};

use bcode_config::ToolExecutionConfig;

use crate::TurnCancelState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolBatchOutcome {
    Completed,
    Cancelled,
    PreparationFailed,
    EffectsUncertain,
    PersistenceFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparationFailure {
    Timeout,
    Unavailable,
    Permanent,
}

impl PreparationFailure {
    pub const fn diagnostic(self) -> &'static str {
        match self {
            Self::Timeout => "Tool host catalog preparation timed out",
            Self::Unavailable => "Tool host catalog is unavailable",
            Self::Permanent => "Tool host preparation cannot safely continue",
        }
    }
}

fn retry_delay(config: &ToolExecutionConfig, attempt: u64, jitter: u64) -> Duration {
    let cap = config.host_retry_max_delay_ms.max(1);
    let base = config
        .host_retry_initial_delay_ms
        .max(1)
        .saturating_mul(1_u64 << u32::try_from(attempt.saturating_sub(1).min(63)).unwrap_or(63))
        .min(cap);
    // Equal jitter avoids synchronized retries, while preserving a nonzero delay and the cap.
    let floor = base.div_ceil(2);
    Duration::from_millis(floor + jitter % (base - floor + 1))
}

pub async fn prepare_with_retry<T, F, Fut, N, Notice>(
    config: &ToolExecutionConfig,
    cancel: &TurnCancelState,
    timeout: impl Into<Option<Duration>>,
    mut prepare: F,
    mut notice: N,
) -> Result<T, ToolBatchOutcome>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, PreparationFailure>>,
    N: FnMut(PreparationFailure, u64, Duration) -> Notice,
    Notice: Future<Output = ()>,
{
    let timeout = timeout.into();
    let mut attempt = 0_u64;
    loop {
        let failure = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ToolBatchOutcome::Cancelled),
            result = async {
                match timeout {
                    Some(timeout) => tokio::time::timeout(timeout, prepare()).await
                        .unwrap_or(Err(PreparationFailure::Timeout)),
                    None => prepare().await,
                }
            } => match result {
                Ok(value) => return Ok(value),
                Err(failure) => failure,
            },
        };
        if failure == PreparationFailure::Permanent
            || config.host_max_retries.is_some_and(|max| attempt >= max)
        {
            return Err(ToolBatchOutcome::PreparationFailed);
        }
        attempt = attempt.saturating_add(1);
        let random = uuid::Uuid::new_v4();
        let jitter = u64::from_le_bytes(random.as_bytes()[..8].try_into().expect("UUID prefix"));
        let delay = retry_delay(config, attempt, jitter);
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ToolBatchOutcome::Cancelled),
            () = notice(failure, attempt, delay) => {},
        }
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ToolBatchOutcome::Cancelled),
            () = tokio::time::sleep(delay) => {},
        }
    }
}

pub async fn prepare_catalog<'a>(
    state: &'a crate::ServerState,
    session_id: crate::SessionId,
    context: &mut crate::RuntimeCommandContext<'_>,
    cancel: &TurnCancelState,
    config: &ToolExecutionConfig,
    timeout: Option<Duration>,
    prepare: impl FnMut() -> crate::ProviderCallFuture<
        'a,
        Result<crate::UnifiedToolCatalog, PreparationFailure>,
    > + Send,
) -> Result<crate::UnifiedToolCatalog, ToolBatchOutcome> {
    let preparation = prepare_with_retry(
        config,
        cancel,
        timeout,
        prepare,
        |failure, attempt, delay| retry_notice(state, session_id, failure, attempt, delay),
    );
    match crate::wait_for_provider_call(state, session_id, context, cancel, Box::pin(preparation))
        .await
    {
        crate::ProviderCallWait::Completed(result) => result,
        crate::ProviderCallWait::Cancelled => Err(ToolBatchOutcome::Cancelled),
    }
}

pub async fn prepare_host_context(
    state: &crate::ServerState,
    session_id: crate::SessionId,
    context: &mut crate::RuntimeCommandContext<'_>,
    cancel: &TurnCancelState,
    working_directory: &std::path::Path,
    config: &ToolExecutionConfig,
    timeout: Duration,
) -> Result<Vec<bcode_tool::ToolHostContextEntry>, ToolBatchOutcome> {
    let preparation = prepare_with_retry(
        config,
        cancel,
        timeout,
        || async {
            let absolute_directory = if working_directory.is_absolute() {
                working_directory.to_path_buf()
            } else {
                std::env::current_dir()
                    .map_err(|_| PreparationFailure::Unavailable)?
                    .join(working_directory)
            };
            crate::invocation_service_host_context(state, session_id, &absolute_directory)
                .await
                .map_err(|_| PreparationFailure::Permanent)
        },
        |failure, attempt, delay| retry_notice(state, session_id, failure, attempt, delay),
    );
    match crate::wait_for_provider_call(state, session_id, context, cancel, Box::pin(preparation))
        .await
    {
        crate::ProviderCallWait::Completed(result) => result,
        crate::ProviderCallWait::Cancelled => Err(ToolBatchOutcome::Cancelled),
    }
}

pub async fn retry_notice(
    state: &crate::ServerState,
    session_id: crate::SessionId,
    failure: PreparationFailure,
    attempt: u64,
    delay: Duration,
) {
    state.metrics.record_histogram_with_labels(
        "tool.host.preparation_retry_delay_ms",
        u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
        crate::tool_invocation_metric_labels(session_id, "host.preparation", None),
    );
    tracing::info!(%session_id, attempt, delay_ms = delay.as_millis(), reason = failure.diagnostic(), "retrying side-effect-free tool host preparation");
    crate::append_system_event(state, session_id, format!(
        "{}; retrying the same pending tool calls in {}ms (attempt {attempt}; no tools dispatched).",
        failure.diagnostic(), delay.as_millis(),
    )).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fast_config() -> ToolExecutionConfig {
        ToolExecutionConfig {
            host_retry_initial_delay_ms: 1,
            host_retry_max_delay_ms: 1,
            ..ToolExecutionConfig::default()
        }
    }

    #[tokio::test]
    async fn repeated_preparation_timeouts_recover_before_one_dispatch() {
        let preparations = AtomicUsize::new(0);
        let dispatches = AtomicUsize::new(0);
        let notices = AtomicUsize::new(0);
        let calls = vec![("same-id", "same-tool")];
        let prepared = prepare_with_retry(
            &fast_config(),
            &TurnCancelState::default(),
            Duration::from_millis(1),
            || async {
                if preparations.fetch_add(1, Ordering::SeqCst) < 3 {
                    std::future::pending::<()>().await;
                }
                Ok(calls.clone())
            },
            |failure, _, _| {
                assert_eq!(failure, PreparationFailure::Timeout);
                notices.fetch_add(1, Ordering::SeqCst);
                std::future::ready(())
            },
        )
        .await
        .expect("recovery");
        dispatches.fetch_add(1, Ordering::SeqCst);
        assert_eq!(prepared, calls);
        assert_eq!(preparations.load(Ordering::SeqCst), 4);
        assert_eq!(notices.load(Ordering::SeqCst), 3);
        assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancellation_during_backoff_does_not_prepare_again() {
        let cancel = TurnCancelState::default();
        let attempts = AtomicUsize::new(0);
        let result = prepare_with_retry::<(), _, _, _, _>(
            &ToolExecutionConfig::default(),
            &cancel,
            Duration::from_secs(1),
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Err(PreparationFailure::Unavailable))
            },
            |_, _, _| {
                cancel.close();
                std::future::ready(())
            },
        )
        .await;
        assert_eq!(result, Err(ToolBatchOutcome::Cancelled));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn permanent_failure_and_retry_limit_stop_without_dispatch() {
        for (failure, max, expected) in [
            (PreparationFailure::Permanent, None, 1),
            (PreparationFailure::Unavailable, Some(2), 3),
        ] {
            let attempts = AtomicUsize::new(0);
            let config = ToolExecutionConfig {
                host_max_retries: max,
                ..fast_config()
            };
            let result = prepare_with_retry::<(), _, _, _, _>(
                &config,
                &TurnCancelState::default(),
                Duration::from_secs(1),
                || {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Err(failure))
                },
                |_, _, _| std::future::ready(()),
            )
            .await;
            assert_eq!(result, Err(ToolBatchOutcome::PreparationFailed));
            assert_eq!(attempts.load(Ordering::SeqCst), expected);
        }
    }

    #[test]
    fn backoff_is_capped_even_at_saturation() {
        let config = ToolExecutionConfig::default();
        for attempt in [1, 2, 30, u64::MAX] {
            for jitter in [0, 123, u64::MAX] {
                let delay = retry_delay(&config, attempt, jitter);
                assert!(delay >= Duration::from_millis(1));
                assert!(delay <= Duration::from_millis(config.host_retry_max_delay_ms));
            }
        }
    }
}
