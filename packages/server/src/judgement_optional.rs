//! Central failure-optional routing. No feature prompts or provider transports live here.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use bcode_model::judgement::{
    self, OptionalOperationObservation, OptionalOperationOutcome, OptionalRequest,
    OptionalResponse, SetupSelection,
};
use bcode_plugin::PluginInvocationScope;
use sha2::{Digest as _, Sha256};

use crate::ServerState;

const SUCCESS_TTL: Duration = Duration::from_mins(5);
const FAILURE_BACKOFF: Duration = Duration::from_secs(30);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(40);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CACHE_ENTRIES: usize = 64;

#[derive(Debug, Default)]
struct VerificationCache {
    epoch: u64,
    retry_after: Option<Instant>,
    entries: BTreeMap<[u8; 32], (Instant, Result<judgement::VerificationResult, &'static str>)>,
}

impl VerificationCache {
    fn get(
        &self,
        key: &[u8; 32],
        now: Instant,
    ) -> Option<Result<judgement::VerificationResult, &'static str>> {
        self.entries
            .get(key)
            .filter(|(expiry, _)| *expiry > now)
            .map(|(_, ready)| *ready)
    }

    fn put(
        &mut self,
        epoch: u64,
        key: [u8; 32],
        ready: Result<judgement::VerificationResult, &'static str>,
        now: Instant,
    ) {
        if epoch != self.epoch {
            return;
        }
        self.entries.retain(|_, (expiry, _)| *expiry > now);
        if self.entries.len() >= MAX_CACHE_ENTRIES {
            self.entries.clear();
        }
        self.entries.insert(
            key,
            (
                now + if ready.is_ok_and(judgement::VerificationResult::is_ready) {
                    SUCCESS_TTL
                } else {
                    FAILURE_BACKOFF
                },
                ready,
            ),
        );
    }
}

/// Retain one instance per application state. Optional failures never change normal behavior.
/// A single bounded permit prevents overlapping probes/calls; busy callers fall back immediately.
#[derive(Debug)]
pub struct OptionalJudgementAccess {
    permit: tokio::sync::Semaphore,
    cache: Arc<Mutex<VerificationCache>>,
}

/// Credential-refresh bridge without application state, credentials or routing authority.
#[derive(Debug, Clone)]
pub struct JudgementInvalidationHandle {
    cache: Arc<Mutex<VerificationCache>>,
}

impl JudgementInvalidationHandle {
    /// Clear cached verification/backoff and fence out old in-flight cache writes.
    pub fn invalidate(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.entries.clear();
            cache.retry_after = None;
            cache.epoch = cache.epoch.wrapping_add(1);
        }
    }
}

impl Default for OptionalJudgementAccess {
    fn default() -> Self {
        Self::new()
    }
}

impl OptionalJudgementAccess {
    /// Construct application-scoped optional routing and verification policy.
    #[must_use]
    pub fn new() -> Self {
        Self {
            permit: tokio::sync::Semaphore::new(1),
            cache: Arc::new(Mutex::new(VerificationCache::default())),
        }
    }

    /// Invalidate after auth, configuration or provider lifecycle changes.
    /// In-flight old verification cannot repopulate the new cache generation.
    pub fn invalidate(&self) {
        self.invalidation_handle().invalidate();
    }

    /// Narrow cloneable refresh bridge sharing this application's cache generation.
    #[must_use]
    pub fn invalidation_handle(&self) -> super::JudgementInvalidationHandle {
        JudgementInvalidationHandle {
            cache: Arc::clone(&self.cache),
        }
    }

    /// Evaluate an applicable caller-owned trigger, or retain ordinary behavior.
    /// Off performs zero discovery, verification or judgement calls. Dropping this future
    /// cancels transient provider work; this access never persists or retries judgement.
    pub async fn judge(&self, state: &ServerState, input: OptionalRequest) -> OptionalResponse {
        self.judge_with_config(state, &state.startup_config, input)
            .await
    }

    /// Evaluate using the resolved session/application configuration. Off returns before
    /// discovery, verification or judgement. Credentials resolve only in this config scope.
    pub async fn judge_with_config(
        &self,
        state: &ServerState,
        config: &bcode_config::BcodeConfig,
        input: OptionalRequest,
    ) -> OptionalResponse {
        let started = Instant::now();
        let mut observation = OptionalOperationObservation::default();
        let _permit = match self.begin(config.judgement.enabled(input.feature)) {
            Ok(permit) => permit,
            Err(outcome) => {
                observation.outcome = outcome;
                observation.elapsed_ms =
                    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                return OptionalResponse {
                    response: None,
                    observation: Some(observation),
                };
            }
        };
        let response = if let Ok(response) = tokio::time::timeout(
            TOTAL_TIMEOUT,
            self.evaluate(state, config, input, &mut observation),
        )
        .await
        {
            response
        } else {
            observation.outcome = OptionalOperationOutcome::TimedOut;
            None
        };
        observation.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        OptionalResponse {
            response,
            observation: Some(observation),
        }
    }

    fn begin(
        &self,
        enabled: bool,
    ) -> Result<tokio::sync::SemaphorePermit<'_>, OptionalOperationOutcome> {
        if !enabled {
            return Err(OptionalOperationOutcome::Disabled);
        }
        let allowed = self
            .cache
            .lock()
            .map_err(|_| OptionalOperationOutcome::Unavailable)?
            .retry_after
            .is_none_or(|until| until <= Instant::now());
        if !allowed {
            return Err(OptionalOperationOutcome::Backoff);
        }
        self.permit
            .try_acquire()
            .map_err(|_| OptionalOperationOutcome::Busy)
    }

    async fn evaluate(
        &self,
        state: &ServerState,
        config: &bcode_config::BcodeConfig,
        mut input: OptionalRequest,
        observation: &mut OptionalOperationObservation,
    ) -> Option<judgement::Response> {
        // Bound input before even discovery. Auto callers may use a nonempty placeholder model.
        judgement::validate_request(&input.request).ok()?;
        let selection = if let Some(selection) = input
            .selection
            .or_else(|| config.judgement.selection.clone())
        {
            selection
        } else {
            let catalog =
                super::setup_catalog_with_config_observed(state, config, observation).await;
            let mut candidates = Vec::new();
            for provider in catalog.providers {
                for model in provider.models {
                    if !input
                        .request
                        .questions
                        .values()
                        .all(|q| model.question_kinds.contains(&q.kind()))
                    {
                        continue;
                    }
                    for profile in &provider.profiles {
                        candidates.push(SetupSelection {
                            provider_plugin_id: provider.plugin_id.clone(),
                            model_id: model.model_id.clone(),
                            auth_profile: profile.clone(),
                        });
                    }
                    if provider.environment_available {
                        candidates.push(SetupSelection {
                            provider_plugin_id: provider.plugin_id.clone(),
                            model_id: model.model_id.clone(),
                            auth_profile: String::new(),
                        });
                    }
                    // Model preference follows provider advertisement; do not treat an
                    // alias and its concrete version as separate account candidates.
                    break;
                }
            }
            // Ambiguous accounts/providers fail closed instead of guessing an account preference.
            if candidates.len() != 1 {
                return None;
            }
            candidates.pop()?
        };
        input.request.model_id = selection.model_id.clone();
        if !self
            .verify_inner(state, config, &selection, observation)
            .await?
            .ok()?
            .is_ready()
        {
            return None;
        }
        observation.outcome = OptionalOperationOutcome::InferenceFailed;
        let result = super::invoke_judgement_model_with_config_observed(
            state,
            config,
            &selection.provider_plugin_id,
            &selection.auth_profile,
            input.request,
            observation,
        )
        .await
        .ok();
        if result.is_none()
            && let Ok(mut cache) = self.cache.lock()
        {
            cache.retry_after = Some(Instant::now() + FAILURE_BACKOFF);
        }
        if result.is_some() {
            observation.outcome = OptionalOperationOutcome::Advice;
        }
        result
    }

    async fn verify_inner(
        &self,
        state: &ServerState,
        config: &bcode_config::BcodeConfig,
        selection: &SetupSelection,
        observation: &mut OptionalOperationObservation,
    ) -> Option<Result<judgement::VerificationResult, &'static str>> {
        if selection.provider_plugin_id.is_empty()
            || selection.model_id.is_empty()
            || selection.provider_plugin_id.len() > 256
            || selection.model_id.len() > 256
            || selection.auth_profile.len() > 256
        {
            return None;
        }
        observation.effective_provider = Some(selection.provider_plugin_id.clone());
        observation.effective_model = Some(selection.model_id.clone());
        let manifest = state
            .plugins
            .registry()
            .manifests()
            .get(&selection.provider_plugin_id)?;
        if !manifest
            .services
            .iter()
            .any(|s| s.interface_id == judgement::VERIFICATION_INTERFACE_ID)
        {
            return None;
        }
        let providers = state.plugins.auth_provider_registry().providers();
        let mut matching = providers
            .iter()
            .filter(|p| p.plugin_id == selection.provider_plugin_id);
        let provider = matching.next()?;
        if matching.next().is_some() {
            return None;
        }
        let context = if selection.auth_profile.is_empty() {
            bcode_provider_auth::resolve_declared_environment_context(&provider.contribution)
                .ok()?
        } else {
            bcode_provider_auth::resolve_owned_profile_context(
                config,
                &provider.contribution.provider_id,
                &selection.provider_plugin_id,
                &selection.auth_profile,
            )
            .ok()?
        };
        context.auth.as_ref()?;
        observation.model_listing_invocations += 1;
        let listing: judgement::ModelList = state
            .plugins
            .invoke_service_json_scoped_with_timeout(
                &selection.provider_plugin_id,
                judgement::INTERFACE_ID,
                judgement::OP_MODELS,
                &context,
                PluginInvocationScope::Global,
                VERIFY_TIMEOUT,
            )
            .await
            .ok()?;
        if listing.models.len() > 4096 {
            return None;
        }
        if !listing
            .models
            .iter()
            .any(|model| model.model_id == selection.model_id)
        {
            return None;
        }
        let verification = judgement::VerificationRequest {
            model_id: selection.model_id.clone(),
            provider_context: context,
        };
        self.verify_cached(state, config, selection, &verification, observation)
            .await
    }

    async fn verify_cached(
        &self,
        state: &ServerState,
        config: &bcode_config::BcodeConfig,
        selection: &SetupSelection,
        verification: &judgement::VerificationRequest,
        observation: &mut OptionalOperationObservation,
    ) -> Option<Result<judgement::VerificationResult, &'static str>> {
        // Digest credential/settings identity; never retain raw credentials in cache or diagnostics.
        let key: [u8; 32] = Sha256::digest(
            serde_json::to_vec(&(&config.active_context, selection, verification)).ok()?,
        )
        .into();
        let (epoch, cached) = {
            let cache = self.cache.lock().ok()?;
            (cache.epoch, cache.get(&key, Instant::now()))
        };
        observation.outcome = OptionalOperationOutcome::VerificationFailed;
        let result = if let Some(result) = cached {
            observation.verification_cache_hit = true;
            result
        } else {
            observation.verification_invocations += 1;
            let result = state
                .plugins
                .invoke_service_json_scoped_with_timeout(
                    &selection.provider_plugin_id,
                    judgement::VERIFICATION_INTERFACE_ID,
                    judgement::OP_VERIFY,
                    verification,
                    PluginInvocationScope::Global,
                    VERIFY_TIMEOUT,
                )
                .await
                .map_err(|_| "judgement verification unavailable");
            self.cache
                .lock()
                .ok()?
                .put(epoch, key, result, Instant::now());
            result
        };
        Some(result)
    }

    /// Verify an exact application selection using shared bounded routing and cache policy.
    ///
    /// # Errors
    /// Returns normalized errors for invalid selection, unavailable credentials/capabilities,
    /// contention, timeout or provider failure. Explicit verification is not a feature trigger.
    pub async fn verify(
        &self,
        state: &ServerState,
        selection: &SetupSelection,
    ) -> Result<judgement::VerificationResult, &'static str> {
        let _permit = self
            .permit
            .try_acquire()
            .map_err(|_| "judgement verification busy")?;
        let mut observation = OptionalOperationObservation::default();
        tokio::time::timeout(
            TOTAL_TIMEOUT,
            self.verify_inner(state, &state.startup_config, selection, &mut observation),
        )
        .await
        .map_err(|_| "judgement verification timed out")?
        .ok_or("judgement verification unavailable")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_outcomes_preserve_off_backoff_and_permit_order() {
        let access = OptionalJudgementAccess::new();
        let permit = access.begin(true).unwrap();
        assert_eq!(
            access.begin(true).unwrap_err(),
            OptionalOperationOutcome::Busy
        );
        access.cache.lock().unwrap().retry_after = Some(Instant::now() + FAILURE_BACKOFF);
        assert_eq!(
            access.begin(true).unwrap_err(),
            OptionalOperationOutcome::Backoff
        );
        assert_eq!(
            access.begin(false).unwrap_err(),
            OptionalOperationOutcome::Disabled
        );
        drop(permit);
        access.invalidate();
        assert!(access.begin(true).is_ok());
    }

    #[test]
    fn poisoned_cache_is_unavailable_but_disabled_still_short_circuits() {
        let access = OptionalJudgementAccess::new();
        let cache = Arc::clone(&access.cache);
        let _ = std::thread::spawn(move || {
            let _guard = cache.lock().unwrap();
            panic!("poison fixture cache");
        })
        .join();
        assert_eq!(
            access.begin(true).unwrap_err(),
            OptionalOperationOutcome::Unavailable
        );
        assert_eq!(
            access.begin(false).unwrap_err(),
            OptionalOperationOutcome::Disabled
        );
    }

    #[tokio::test]
    async fn timeout_retains_attempted_invocations_and_cancels_borrowed_work() {
        let mut observation = OptionalOperationObservation::default();
        let attempted = tokio::time::timeout(Duration::from_millis(1), async {
            observation.model_listing_invocations += 1;
            observation.verification_invocations += 1;
            std::future::pending::<()>().await;
            observation.inference_invocations += 1;
        })
        .await;
        assert!(attempted.is_err());
        assert_eq!(observation.model_listing_invocations, 1);
        assert_eq!(observation.verification_invocations, 1);
        assert_eq!(observation.inference_invocations, 0);
        assert_eq!(observation.inference_usage, None);
        assert_eq!(observation.transport_attempts, None);
    }

    #[test]
    fn cloned_refresh_handle_invalidates_shared_generation_and_backoff() {
        let access = OptionalJudgementAccess::new();
        let handle = access.invalidation_handle();
        let refresh = handle.clone();
        {
            let mut cache = access.cache.lock().unwrap();
            cache.put(0, [1; 32], Err("temporary"), Instant::now());
            cache.retry_after = Some(Instant::now() + FAILURE_BACKOFF);
        }
        refresh.invalidate();
        let mut cache = access.cache.lock().unwrap();
        assert_eq!(cache.epoch, 1);
        assert!(cache.entries.is_empty());
        assert!(cache.retry_after.is_none());
        cache.put(0, [2; 32], Err("stale probe"), Instant::now());
        assert!(cache.entries.is_empty());
        drop(cache);
        handle.invalidate();
        assert_eq!(access.cache.lock().unwrap().epoch, 2);
    }

    #[test]
    fn verification_expiry_backoff_and_invalidation_are_bounded() {
        let mut cache = VerificationCache::default();
        let now = Instant::now();
        let ready = Ok(judgement::VerificationResult {
            method: judgement::VerificationMethod::AuthenticatedDiscovery,
            authentication: judgement::AuthenticationStatus::Accepted,
            model: judgement::ModelReadiness::NotChecked,
        });
        cache.put(0, [1; 32], ready, now);
        cache.put(0, [2; 32], Err("temporary"), now);
        assert_eq!(cache.get(&[1; 32], now + FAILURE_BACKOFF), Some(ready));
        assert_eq!(cache.get(&[2; 32], now + FAILURE_BACKOFF), None);
        assert_eq!(cache.get(&[1; 32], now + SUCCESS_TTL), None);
        cache.epoch = 1;
        cache.entries.clear();
        cache.put(0, [3; 32], ready, now);
        assert!(cache.entries.is_empty());
        for n in 0..100 {
            cache.put(1, [n; 32], ready, now);
        }
        assert!(cache.entries.len() <= MAX_CACHE_ENTRIES);
    }

    #[test]
    fn authentication_and_readiness_are_independent() {
        use judgement::{
            AuthenticationStatus as Auth, ModelReadiness as Model, VerificationResult,
        };
        assert!(
            VerificationResult {
                method: judgement::VerificationMethod::AuthenticatedDiscovery,
                authentication: Auth::Accepted,
                model: Model::Ready
            }
            .is_ready()
        );
        assert!(
            VerificationResult {
                method: judgement::VerificationMethod::AuthenticatedDiscovery,
                authentication: Auth::Accepted,
                model: Model::NotChecked,
            }
            .is_ready()
        );
        for model in [
            Model::QuotaLimited,
            Model::TemporaryFailure,
            Model::Unsupported,
            Model::Unavailable,
        ] {
            assert!(
                !VerificationResult {
                    method: judgement::VerificationMethod::AuthenticatedDiscovery,
                    authentication: Auth::Accepted,
                    model
                }
                .is_ready()
            );
        }
        for authentication in [Auth::Unknown, Auth::Rejected] {
            assert!(
                !VerificationResult {
                    method: judgement::VerificationMethod::AuthenticatedDiscovery,
                    authentication,
                    model: Model::Ready
                }
                .is_ready()
            );
        }
    }
}
