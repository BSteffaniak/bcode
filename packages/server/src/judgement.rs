//! Non-conversational judgement application routing.
//!
//! The application resolves credentials, identity and capabilities before invoking the plugin.
//! Provider service failures are untrusted and never returned verbatim to a client.

use bcode_model::judgement::{self, ModelList, ProviderRequest, Request, Response};
use bcode_plugin::PluginInvocationScope;

use crate::ServerState;

#[path = "judgement_optional.rs"]
mod optional;
pub use optional::OptionalJudgementAccess;

/// Bounded disposable turn snapshots; independent turn handles never share operation state.
#[derive(Debug, Default)]
pub struct TurnObservationStore {
    turns: std::collections::VecDeque<(
        bcode_session_models::SessionId,
        String,
        TurnObservationHandle,
    )>,
}

/// Request-only producer handle. Closing fences every outstanding operation handle.
#[derive(Debug, Clone)]
pub struct TurnObservationHandle(
    std::sync::Arc<std::sync::Mutex<judgement::TurnJudgementObservation>>,
);

/// Correlated operation handle, scoped by the host rather than plugin-controlled IDs.
#[derive(Debug, Clone)]
pub struct ConsumerOperationHandle {
    turn: TurnObservationHandle,
    index: Option<u64>,
    pending: std::sync::Arc<std::sync::Mutex<(usize, bool)>>,
}

impl TurnObservationStore {
    /// Start a fresh observation; duplicate identities are rejected rather than reset.
    pub fn begin(
        &mut self,
        session: bcode_session_models::SessionId,
        turn: &str,
    ) -> Option<TurnObservationHandle> {
        if turn.is_empty()
            || turn.len() > 128
            || self
                .turns
                .iter()
                .any(|(s, t, _)| *s == session && t == turn)
        {
            return None;
        }
        if self.turns.len() >= 32 {
            let index = self.turns.iter().position(|(_, _, handle)| {
                handle
                    .0
                    .lock()
                    .is_ok_and(|snapshot| snapshot.terminal.is_some())
            })?;
            self.turns.remove(index);
        }
        let handle = TurnObservationHandle(std::sync::Arc::new(std::sync::Mutex::new(
            judgement::TurnJudgementObservation {
                schema_version: judgement::TURN_OBSERVATION_VERSION,
                scope: judgement::ObservationScope::NormalTurn,
                session_id: session.to_string(),
                turn_id: turn.into(),
                operations: vec![],
                accounting: Some(judgement::OperationAccounting::default()),
                passive_aggregates: vec![],
                covered_features: vec![],
                terminal: None,
                omitted: 0,
                complete: false,
            },
        )));
        self.turns.push_back((session, turn.into(), handle.clone()));
        Some(handle)
    }

    /// Retrieve only an authenticated exact session/turn handle; never looks up current turn.
    pub fn handle(
        &self,
        session: bcode_session_models::SessionId,
        turn: &str,
    ) -> Option<TurnObservationHandle> {
        self.turns
            .iter()
            .find(|(s, t, _)| *s == session && t == turn)
            .map(|(_, _, handle)| handle.clone())
    }

    /// Read a bounded transient snapshot; absent/poisoned/evicted observations stay unavailable.
    pub fn snapshot_json(
        &self,
        session: bcode_session_models::SessionId,
        turn: &str,
    ) -> Option<String> {
        let (_, _, handle) = self
            .turns
            .iter()
            .find(|(s, t, _)| *s == session && t == turn)?;
        let snapshot = handle.0.lock().ok()?;
        snapshot.validate().ok()?;
        serde_json::to_string(&*snapshot).ok()
    }
}

fn compact_passive_operations(snapshot: &mut judgement::TurnJudgementObservation) {
    let mut index = 0;
    while index < snapshot.operations.len() {
        let op = &snapshot.operations[index];
        if !matches!(
            op.trigger,
            judgement::TriggerDisposition::Disabled | judgement::TriggerDisposition::NotApplicable
        ) || op.application != Some(judgement::ApplicationDisposition::BaselineRetained)
            || !op.inference_complete
            || !op.inference.is_empty()
            || op.selection.is_some()
            || op.recommendation.is_some()
        {
            index += 1;
            continue;
        }
        if let Some(aggregate) = snapshot
            .passive_aggregates
            .iter_mut()
            .find(|aggregate| aggregate.feature == op.feature && aggregate.trigger == op.trigger)
        {
            let Some(count) = aggregate.count.checked_add(1) else {
                snapshot.omitted = snapshot.omitted.saturating_add(1);
                index += 1;
                continue;
            };
            aggregate.count = count;
            aggregate.first_operation_id = aggregate.first_operation_id.min(op.operation_id);
            aggregate.last_operation_id = aggregate.last_operation_id.max(op.operation_id);
        } else {
            snapshot
                .passive_aggregates
                .push(judgement::PassiveOperationAggregate {
                    feature: op.feature,
                    trigger: op.trigger,
                    application: judgement::ApplicationDisposition::BaselineRetained,
                    count: 1,
                    first_operation_id: op.operation_id,
                    last_operation_id: op.operation_id,
                });
        }
        snapshot.operations.remove(index);
    }
}

impl TurnObservationHandle {
    /// Authenticated host correlation for a nested invocation, never current-turn discovery.
    pub fn invocation_scope(&self) -> Option<bcode_plugin::PluginInvocationScope> {
        let snapshot = self.0.lock().ok()?;
        Some(
            bcode_plugin::PluginInvocationScope::session(snapshot.session_id.clone())
                .with_turn_id(snapshot.turn_id.clone()),
        )
    }

    /// Record actual trigger evaluation before invoking optional work.
    #[must_use]
    pub fn begin_operation(
        &self,
        feature: judgement::Feature,
        trigger: judgement::TriggerDisposition,
    ) -> ConsumerOperationHandle {
        let index = self.0.lock().ok().and_then(|mut snapshot| {
            if snapshot.terminal.is_some() {
                return None;
            }
            let accounting = snapshot.accounting.as_mut()?;
            let index = accounting.begun;
            let Some(next) = accounting.begun.checked_add(1) else {
                snapshot.omitted = snapshot.omitted.saturating_add(1);
                return None;
            };
            accounting.begun = next;
            if snapshot.operations.len() >= judgement::MAX_TURN_OPERATIONS {
                compact_passive_operations(&mut snapshot);
            }
            if snapshot.operations.len() >= judgement::MAX_TURN_OPERATIONS {
                snapshot.omitted = snapshot.omitted.saturating_add(1);
                let accounting = snapshot.accounting.as_mut()?;
                accounting.omitted_operations = accounting.omitted_operations.saturating_add(1);
                return None;
            }
            snapshot
                .operations
                .push(judgement::ConsumerOperationObservation {
                    operation_id: index,
                    feature,
                    trigger,
                    inference: vec![],
                    inference_complete: trigger != judgement::TriggerDisposition::Triggered,
                    application: None,
                    selection: None,
                    recommendation: None,
                });
            Some(index)
        });
        ConsumerOperationHandle {
            turn: self.clone(),
            index,
            pending: std::sync::Arc::new(std::sync::Mutex::new((0, false))),
        }
    }

    /// Certify only hooks actually instrumented by this producer. Terminal closure is stable.
    pub fn close(&self, terminal: judgement::ObservationTerminal, covered: &[judgement::Feature]) {
        if let Ok(mut snapshot) = self.0.lock() {
            if snapshot.terminal.is_some() {
                return;
            }
            snapshot.terminal = Some(terminal);
            snapshot.covered_features =
                covered
                    .iter()
                    .copied()
                    .take(6)
                    .fold(Vec::new(), |mut result, feature| {
                        if !result.contains(&feature) && snapshot.has_feature(feature) {
                            result.push(feature);
                        }
                        result
                    });
            let required: &[judgement::Feature] = match snapshot.scope {
                judgement::ObservationScope::NormalTurn => &[
                    judgement::Feature::ToolResultTriage,
                    judgement::Feature::ContextReranking,
                    judgement::Feature::ProgressMonitoring,
                    judgement::Feature::Recommendations,
                    judgement::Feature::SessionCategorization,
                ],
                judgement::ObservationScope::EvalAttempt => {
                    &[judgement::Feature::EvalCategorization]
                }
            };
            snapshot.complete = terminal != judgement::ObservationTerminal::Cancelled
                && snapshot.omitted == 0
                && required
                    .iter()
                    .all(|feature| snapshot.covered_features.contains(feature))
                && snapshot
                    .covered_features
                    .iter()
                    .all(|feature| snapshot.has_feature(*feature))
                && snapshot.operations.iter().all(|op| {
                    op.inference_complete
                        && op.application.is_some()
                        && op.application != Some(judgement::ApplicationDisposition::Unavailable)
                });
        }
    }
}

impl ConsumerOperationHandle {
    /// Record a producer's explicit no-work prerequisite result, only before central work.
    pub fn record_not_applicable(&self) {
        self.update(|op| {
            let allowed = self
                .pending
                .lock()
                .is_ok_and(|pending| pending.0 == 0 && !pending.1);
            if allowed
                && op.trigger == judgement::TriggerDisposition::Triggered
                && op.inference.is_empty()
                && op.application.is_none()
            {
                op.trigger = judgement::TriggerDisposition::NotApplicable;
                op.inference_complete = true;
            }
        });
    }

    /// Mark central work as pending atomically with the retained operation, before dispatch.
    pub fn inference_started(&self) {
        self.update(|op| {
            if let Ok(mut pending) = self.pending.lock() {
                if let Some(count) = pending.0.checked_add(1) {
                    pending.0 = count;
                } else {
                    pending.1 = true;
                }
            }
            op.inference_complete = false;
        });
    }

    /// Retain normalized facts atomically with pending-work and application qualifiers.
    pub fn record_inference(&self, response: &judgement::OptionalResponse) {
        self.update(|op| {
            let Ok(mut pending) = self.pending.lock() else {
                op.inference_complete = false;
                return;
            };
            let unmatched = pending.0 == 0;
            pending.0 = pending.0.saturating_sub(1);
            pending.1 |= unmatched || response.observation.is_none();
            let Some(observation) = &response.observation else {
                op.inference_complete = false;
                return;
            };
            if op.inference.len() >= 8
                || serde_json::to_vec(observation).map_or(true, |bytes| {
                    bytes.len() > judgement::MAX_TURN_OBSERVATION_BYTES / 4
                })
            {
                pending.1 = true;
                op.inference_complete = false;
                return;
            }
            op.inference.push(observation.clone());
            op.inference_complete = pending.0 == 0 && !pending.1;
        });
        // If an encoded bound was crossed, discard details and fail closed rather than export
        // an oversized snapshot. Operation/application identities remain retained.
        if let Ok(mut snapshot) = self.turn.0.lock() {
            if snapshot.terminal.is_some() {
                return;
            }
            if serde_json::to_vec(&*snapshot).map_or(true, |bytes| {
                bytes.len() > judgement::MAX_TURN_OBSERVATION_BYTES / 2
            }) {
                for op in &mut snapshot.operations {
                    op.inference.clear();
                    op.inference_complete = false;
                }
                snapshot.omitted = snapshot.omitted.saturating_add(1);
            }
        }
    }

    /// Host acknowledgment of actual application/fallback, after validation and installation.
    pub fn finish(
        &self,
        application: judgement::ApplicationDisposition,
        selection: Option<judgement::SelectionObservation>,
    ) {
        if selection.as_ref().is_some_and(|value| {
            value.candidate_count > 32 || value.ordered.len() > 32 || value.admitted.len() > 32
        }) {
            self.update(|op| {
                op.application = Some(judgement::ApplicationDisposition::Unavailable);
                op.inference_complete = false;
            });
            return;
        }
        self.update(|op| {
            if op.application.is_some() {
                return;
            }
            op.application = Some(application);
            op.selection = selection;
        });
    }

    /// Acknowledge an installed advisory catalog permutation with original identities.
    pub fn finish_recommendation(
        &self,
        application: judgement::ApplicationDisposition,
        recommendation: judgement::RecommendationSelectionObservation,
    ) {
        self.update(|op| {
            if op.application.is_some() {
                return;
            }
            op.selection = Some(recommendation.selection.clone());
            op.recommendation = Some(recommendation);
            op.application = Some(application);
        });
    }

    fn update(&self, apply: impl FnOnce(&mut judgement::ConsumerOperationObservation)) {
        if let Ok(mut snapshot) = self.turn.0.lock() {
            if snapshot.terminal.is_some() {
                return;
            }
            if let Some(op) = self.index.and_then(|index| {
                snapshot
                    .operations
                    .iter_mut()
                    .find(|op| op.operation_id == index)
            }) {
                apply(op);
            } else if self.index.is_some() {
                // A late mutation of an already compacted fact cannot silently acquire authority.
                snapshot.omitted = snapshot.omitted.saturating_add(1);
            }
        }
    }
}

/// Ephemeral application observation window. It is never a history projection.
#[derive(Debug, Default)]
pub struct ProgressWindow {
    objective: String,
    sequence: u64,
    observations: std::collections::VecDeque<serde_json::Value>,
    pub(crate) advice: Option<ProgressAdvice>,
}

/// Portable consumer reply, validated before transient presentation.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressAdvice {
    scope: serde_json::Value,
    through_sequence: u64,
    kind: String,
    pub(crate) message: String,
    progress_probability: f64,
    stall_probability: f64,
    drift_probability: f64,
}

/// Identity of a successful canonical terminal record, not a claim of objective progress.
#[derive(Debug, Clone, Default)]
pub enum SettledProgressEvidence {
    #[default]
    Unknown,
    SuccessfulInvocation {
        session_id: bcode_session_models::SessionId,
        invocation_id: String,
        sequence: u64,
    },
}

impl SettledProgressEvidence {
    pub(crate) fn from_persisted_result(
        session_id: bcode_session_models::SessionId,
        invocation_id: &str,
        settlement: bcode_agent_runtime::ToolCallSettlement,
        event: Option<&bcode_session_models::SessionEvent>,
    ) -> Self {
        let Some(event) = event else {
            return Self::Unknown;
        };
        if settlement != bcode_agent_runtime::ToolCallSettlement::Settled
            || event.schema_version != bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION
            || event.session_id != session_id
            || event.sequence == 0
        {
            return Self::Unknown;
        }
        let bcode_session_models::SessionEventKind::ToolInvocationResultRecorded { record } =
            &event.kind
        else {
            return Self::Unknown;
        };
        if record.is_error || record.invocation_id != invocation_id || invocation_id.is_empty() {
            return Self::Unknown;
        }
        Self::SuccessfulInvocation {
            session_id,
            invocation_id: invocation_id.to_owned(),
            sequence: event.sequence,
        }
    }

    fn key(&self) -> Option<String> {
        match self {
            Self::Unknown => None,
            Self::SuccessfulInvocation {
                session_id,
                invocation_id,
                sequence,
            } => {
                use sha2::{Digest as _, Sha256};
                let identity = serde_json::to_vec(&(session_id, invocation_id, sequence)).ok()?;
                Some(format!(
                    "settled-invocation:{}",
                    hex::encode(Sha256::digest(identity))
                ))
            }
        }
    }
}

impl ProgressWindow {
    pub(crate) fn set_objective(&mut self, objective: String) {
        if self.objective != objective {
            self.objective = objective;
            self.observations.clear();
            self.advice = None;
        }
    }

    pub(crate) fn observe(
        &mut self,
        scope: &serde_json::Value,
        action: &str,
        failure_key: Option<String>,
    ) -> Option<serde_json::Value> {
        self.observe_with_evidence(
            scope,
            action,
            failure_key,
            &SettledProgressEvidence::Unknown,
        )
    }

    pub(crate) fn observe_with_evidence(
        &mut self,
        scope: &serde_json::Value,
        action: &str,
        failure_key: Option<String>,
        evidence: &SettledProgressEvidence,
    ) -> Option<serde_json::Value> {
        if self.objective.is_empty() || action.is_empty() || action.len() > 256 {
            return None;
        }
        if failure_key.is_none() {
            // Preserve the existing success reset; receipt evidence is supplied separately.
            self.observations.clear();
        }
        self.sequence = self.sequence.checked_add(1)?;
        self.advice = None;
        if self.observations.len() == 8 {
            self.observations.pop_front();
        }
        let evidence_key = failure_key.is_none().then(|| evidence.key()).flatten();
        self.observations.push_back(serde_json::json!({
            "sequence": self.sequence,
            "action": action,
            "failure_key": serde_json::Value::from(failure_key),
            "evidence_key": evidence_key,
            "summary": if evidence_key.is_some() {
                "Canonical successful invocation settled; objective progress not certified"
            } else {
                "Settled outcome; no verified evidence supplied"
            },
        }));
        let request = serde_json::json!({
            "version": 1, "scope": scope, "objective": self.objective,
            "observations": self.observations,
        });
        (serde_json::to_vec(&request).ok()?.len() <= 16_384).then_some(request)
    }

    pub(crate) fn accept(
        &mut self,
        scope: &serde_json::Value,
        request: &serde_json::Value,
        reply: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        if reply.get("version")?.as_u64()? != 1
            || request.get("objective")?.as_str()? != self.objective
        {
            return None;
        }
        let payload = reply.get("advice")?.clone();
        let advice: ProgressAdvice = serde_json::from_value(payload.clone()).ok()?;
        if advice.scope != *scope
            || advice.through_sequence != self.sequence
            || !matches!(advice.kind.as_str(), "repeated_failure" | "drift")
            || advice.message.trim().is_empty()
            || advice.message.len() > 1024
            || [
                advice.progress_probability,
                advice.stall_probability,
                advice.drift_probability,
            ]
            .iter()
            .any(|p| !p.is_finite() || !(0.0..=1.0).contains(p))
        {
            return None;
        }
        self.advice = Some(advice);
        Some(payload)
    }
}

/// Cache-only bridge suitable for credential-refresh closures.
pub type JudgementInvalidationHandle = optional::JudgementInvalidationHandle;

const DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const JUDGEMENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub async fn validate_setup_selection(
    state: &ServerState,
    selection: &judgement::SetupSelection,
) -> bool {
    if selection.provider_plugin_id.len() > 256
        || selection.model_id.len() > 256
        || selection.auth_profile.len() > 256
        || selection.provider_plugin_id.is_empty()
        || selection.model_id.is_empty()
    {
        return false;
    }
    let Some(manifest) = state
        .plugins
        .registry()
        .manifests()
        .get(&selection.provider_plugin_id)
    else {
        return false;
    };
    if !manifest
        .services
        .iter()
        .any(|service| service.interface_id == judgement::INTERFACE_ID)
    {
        return false;
    }
    let matching: Vec<_> = state
        .plugins
        .auth_provider_registry()
        .providers()
        .into_iter()
        .filter(|provider| provider.plugin_id == selection.provider_plugin_id)
        .collect();
    if matching.len() != 1 {
        return false;
    }
    let provider = matching[0];
    let context = if selection.auth_profile.is_empty() {
        bcode_provider_auth::resolve_declared_environment_context(&provider.contribution)
    } else {
        bcode_provider_auth::resolve_owned_profile_context(
            &state.startup_config,
            &provider.contribution.provider_id,
            &selection.provider_plugin_id,
            &selection.auth_profile,
        )
    };
    let Ok(context) = context else {
        return false;
    };
    if context
        .auth
        .as_ref()
        .is_none_or(|auth| auth.credentials.is_empty())
    {
        return false;
    }
    let listing: Result<ModelList, _> = state
        .plugins
        .invoke_service_json_scoped_with_timeout(
            &selection.provider_plugin_id,
            judgement::INTERFACE_ID,
            judgement::OP_MODELS,
            &context,
            PluginInvocationScope::Global,
            DISCOVERY_TIMEOUT,
        )
        .await;
    listing.is_ok_and(|list| {
        list.provider_id == selection.provider_plugin_id
            && list
                .question_kinds
                .contains(&judgement::QuestionKind::YesNo)
            && list.models.iter().any(|model| {
                model.model_id == selection.model_id
                    && model
                        .question_kinds
                        .contains(&judgement::QuestionKind::YesNo)
            })
    })
}

pub async fn setup_catalog(state: &ServerState) -> judgement::SetupCatalog {
    setup_catalog_with_config(state, &state.startup_config).await
}

async fn setup_catalog_with_config(
    state: &ServerState,
    config: &bcode_config::BcodeConfig,
) -> judgement::SetupCatalog {
    setup_catalog_with_config_observed(
        state,
        config,
        &mut judgement::OptionalOperationObservation::default(),
    )
    .await
}

async fn setup_catalog_with_config_observed(
    state: &ServerState,
    config: &bcode_config::BcodeConfig,
    observation: &mut judgement::OptionalOperationObservation,
) -> judgement::SetupCatalog {
    observation.discovery_invocations += 1;
    let mut providers = Vec::new();
    for (plugin_id, manifest) in state.plugins.registry().manifests() {
        if providers.len() >= 32 {
            break;
        }
        if !manifest
            .services
            .iter()
            .any(|service| service.interface_id == judgement::INTERFACE_ID)
        {
            continue;
        }
        if let Some(provider) = setup_provider(state, config, plugin_id, observation).await {
            providers.push(provider);
            if serde_json::to_vec(&judgement::SetupCatalog {
                providers: providers.clone(),
            })
            .map_or(true, |bytes| bytes.len() > 240_000)
            {
                providers.pop();
            }
        }
    }
    judgement::SetupCatalog { providers }
}

fn setup_profiles(
    config: &bcode_config::BcodeConfig,
    plugin_id: &str,
    provider_id: &str,
) -> Vec<String> {
    let mut profiles = Vec::new();
    let mut names: std::collections::BTreeSet<String> =
        config.auth.profiles.keys().cloned().collect();
    let runtime = if config.active_context.is_none() {
        bcode_config::try_load_runtime_auth_subscriptions().ok()
    } else {
        None
    };
    if config.active_context.is_none() && runtime.is_none() {
        return Vec::new();
    }
    if let Some(runtime) = runtime.as_ref() {
        names.extend(runtime.profiles.keys().cloned());
    }
    for name in names.into_iter().take(256) {
        if profiles.len() >= 32 {
            break;
        }
        if name.len() > 256 {
            continue;
        }
        let empty = bcode_config::RuntimeAuthSubscriptions::default();
        let registry = runtime.as_ref().unwrap_or(&empty);
        if bcode_provider_auth::resolve_auth_provider_profile(
            config,
            provider_id,
            plugin_id,
            Some(&name),
            registry,
        )
        .is_ok()
            && bcode_provider_auth::resolve_owned_profile_context(
                config,
                provider_id,
                plugin_id,
                &name,
            )
            .is_ok()
        {
            profiles.push(name);
        }
    }
    profiles
}

async fn setup_provider(
    state: &ServerState,
    config: &bcode_config::BcodeConfig,
    plugin_id: &str,
    observation: &mut judgement::OptionalOperationObservation,
) -> Option<judgement::SetupProvider> {
    let matching: Vec<_> = state
        .plugins
        .auth_provider_registry()
        .providers()
        .into_iter()
        .filter(|auth| auth.plugin_id == plugin_id)
        .collect();
    if matching.len() != 1 {
        return None;
    }
    let auth = matching[0];
    let profiles = setup_profiles(config, plugin_id, &auth.contribution.provider_id);
    let environment_available =
        bcode_provider_auth::resolve_declared_environment_context(&auth.contribution).is_ok();
    let context = profiles
        .first()
        .and_then(|name| {
            bcode_provider_auth::resolve_owned_profile_context(
                config,
                &auth.contribution.provider_id,
                plugin_id,
                name,
            )
            .ok()
        })
        .or_else(|| {
            environment_available
                .then(|| {
                    bcode_provider_auth::resolve_declared_environment_context(&auth.contribution)
                        .ok()
                })
                .flatten()
        });
    // Model discovery is optional: an unavailable auth account still permits static
    // capability discovery, but does not permit judgement execution.
    let discovery_context = context.unwrap_or_default();
    observation.model_listing_invocations += 1;
    let listing: Result<ModelList, _> = state
        .plugins
        .invoke_service_json_scoped_with_timeout(
            plugin_id,
            judgement::INTERFACE_ID,
            judgement::OP_MODELS,
            &discovery_context,
            PluginInvocationScope::Global,
            DISCOVERY_TIMEOUT,
        )
        .await;
    let (models, unavailable_reason) = match listing {
        Ok(list)
            if list.models.len() <= 64
                && list.models.iter().all(|model| model.model_id.len() <= 256)
                && list.provider_id == plugin_id =>
        {
            (
                list.models
                    .into_iter()
                    .filter(|model| {
                        model
                            .question_kinds
                            .contains(&judgement::QuestionKind::YesNo)
                            && list
                                .question_kinds
                                .contains(&judgement::QuestionKind::YesNo)
                    })
                    .collect(),
                None,
            )
        }
        _ => (
            Vec::new(),
            Some("model discovery unavailable; refresh or inspect provider setup".into()),
        ),
    };
    let unavailable_reason = if profiles.is_empty() && !environment_available {
        Some(format!(
            "No usable account; run bcode auth login {}",
            auth.contribution.provider_id
        ))
    } else {
        unavailable_reason
    };
    Some(judgement::SetupProvider {
        plugin_id: plugin_id.into(),
        display_name: auth.contribution.display_name.chars().take(128).collect(),
        auth_provider_id: auth.contribution.provider_id.clone(),
        profiles,
        environment_available,
        models,
        unavailable_reason,
    })
}

pub async fn invoke_judgement_model(
    state: &ServerState,
    provider_plugin_id: &str,
    auth_profile: &str,
    request: Request,
) -> Result<Response, &'static str> {
    invoke_judgement_model_with_config(
        state,
        &state.startup_config,
        provider_plugin_id,
        auth_profile,
        request,
    )
    .await
}

async fn invoke_judgement_model_with_config(
    state: &ServerState,
    config: &bcode_config::BcodeConfig,
    provider_plugin_id: &str,
    auth_profile: &str,
    request: Request,
) -> Result<Response, &'static str> {
    invoke_judgement_model_with_config_observed(
        state,
        config,
        provider_plugin_id,
        auth_profile,
        request,
        &mut judgement::OptionalOperationObservation::default(),
    )
    .await
}

async fn invoke_judgement_model_with_config_observed(
    state: &ServerState,
    config: &bcode_config::BcodeConfig,
    provider_plugin_id: &str,
    auth_profile: &str,
    request: Request,
    observation: &mut judgement::OptionalOperationObservation,
) -> Result<Response, &'static str> {
    judgement::validate_request(&request)?;
    if provider_plugin_id.is_empty()
        || provider_plugin_id.len() > 256
        || auth_profile.len() > 256
        || !state
            .plugins
            .registry()
            .manifests()
            .get(provider_plugin_id)
            .is_some_and(|manifest| {
                manifest
                    .services
                    .iter()
                    .any(|service| service.interface_id == judgement::INTERFACE_ID)
            })
    {
        return Err("judgement provider is not available");
    }
    let mut auth_providers = state
        .plugins
        .auth_provider_registry()
        .providers()
        .into_iter()
        .filter(|entry| entry.plugin_id == provider_plugin_id);
    let registered = auth_providers
        .next()
        .ok_or("judgement provider authentication is unavailable")?;
    if auth_providers.next().is_some() {
        return Err("judgement provider authentication is ambiguous");
    }
    let provider_context = if auth_profile.is_empty() {
        bcode_provider_auth::resolve_declared_environment_context(&registered.contribution)?
    } else {
        bcode_provider_auth::resolve_owned_profile_context(
            config,
            &registered.contribution.provider_id,
            provider_plugin_id,
            auth_profile,
        )?
    };
    if provider_context.auth.is_none() {
        return Err("judgement provider credentials are unavailable");
    }
    observation.model_listing_invocations += 1;
    let listing: ModelList = state
        .plugins
        .invoke_service_json_scoped_with_timeout(
            provider_plugin_id,
            judgement::INTERFACE_ID,
            judgement::OP_MODELS,
            &provider_context,
            PluginInvocationScope::Global,
            DISCOVERY_TIMEOUT,
        )
        .await
        .map_err(|_| "judgement model discovery failed")?;
    if listing.models.len() > 4096 {
        return Err("judgement model listing exceeds size limit");
    }
    let resolved = state
        .model_catalog
        .resolve_judgement_request(provider_plugin_id, &request, &listing)
        .await?;
    let model = judgement::select_supported_model(&resolved, &listing)?;
    let envelope = ProviderRequest {
        judgement: resolved.clone(),
        provider_context,
    };
    observation.effective_provider = Some(provider_plugin_id.to_owned());
    observation.effective_model = Some(resolved.model_id.clone());
    observation.inference_invocations += 1;
    let result: Response = state
        .plugins
        .invoke_service_json_scoped_with_timeout(
            provider_plugin_id,
            judgement::INTERFACE_ID,
            judgement::OP_JUDGE,
            &envelope,
            PluginInvocationScope::Global,
            JUDGEMENT_TIMEOUT,
        )
        .await
        .map_err(|_| "judgement provider invocation failed")?;
    judgement::validate_response(&resolved, model, &result)?;
    observation.inference_usage.clone_from(&result.usage);
    Ok(result)
}

#[cfg(test)]
mod observation_tests {
    use super::*;

    fn successful_progress_record(
        session_id: bcode_session_models::SessionId,
    ) -> bcode_session_models::SessionEvent {
        bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 7,
            timestamp_ms: 0,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "original-call".into(),
                    model_output: "opaque result".into(),
                    is_error: false,
                    content: Vec::new(),
                    presentation: None,
                    result: None,
                },
            },
        }
    }

    #[test]
    fn canonical_success_evidence_is_stable_and_resets_failure_streak() {
        let session = bcode_session_models::SessionId::new();
        let event = successful_progress_record(session);
        let evidence = SettledProgressEvidence::from_persisted_result(
            session,
            "original-call",
            bcode_agent_runtime::ToolCallSettlement::Settled,
            Some(&event),
        );
        assert!(evidence.key().is_some());
        assert_eq!(
            evidence.key(),
            SettledProgressEvidence::from_persisted_result(
                session,
                "original-call",
                bcode_agent_runtime::ToolCallSettlement::Settled,
                Some(&event),
            )
            .key()
        );
        let scope = serde_json::json!({"kind":"session", "id":session});
        let mut window = ProgressWindow::default();
        window.set_objective("objective".into());
        window
            .observe(&scope, "action", Some("failure".into()))
            .unwrap();
        let request = window
            .observe_with_evidence(&scope, "action", None, &evidence)
            .unwrap();
        assert_eq!(request["observations"].as_array().unwrap().len(), 1);
        assert_eq!(
            request["observations"][0]["evidence_key"],
            evidence.key().unwrap()
        );
        assert!(request["observations"][0]["failure_key"].is_null());
        assert!(window.advice.is_none());
        // A repeated delivery retains identity; it cannot invent another evidence key.
        let repeated = window
            .observe_with_evidence(&scope, "action", None, &evidence)
            .unwrap();
        assert_eq!(
            repeated["observations"][0]["evidence_key"],
            request["observations"][0]["evidence_key"]
        );
    }

    #[test]
    fn uncertain_failed_or_mismatched_records_have_no_evidence_authority() {
        use bcode_agent_runtime::ToolCallSettlement;
        let session = bcode_session_models::SessionId::new();
        let event = successful_progress_record(session);
        for settlement in [
            ToolCallSettlement::Unresolved,
            ToolCallSettlement::NeverDispatched,
        ] {
            assert!(
                SettledProgressEvidence::from_persisted_result(
                    session,
                    "original-call",
                    settlement,
                    Some(&event),
                )
                .key()
                .is_none()
            );
        }
        // A failed append has no canonical receipt to supply.
        assert!(
            SettledProgressEvidence::from_persisted_result(
                session,
                "original-call",
                ToolCallSettlement::Settled,
                None,
            )
            .key()
            .is_none()
        );
        for (owner, id) in [
            (bcode_session_models::SessionId::new(), "original-call"),
            (session, "wrong-call"),
        ] {
            assert!(
                SettledProgressEvidence::from_persisted_result(
                    owner,
                    id,
                    ToolCallSettlement::Settled,
                    Some(&event),
                )
                .key()
                .is_none()
            );
        }
        let mut failed = event.clone();
        let bcode_session_models::SessionEventKind::ToolInvocationResultRecorded { record } =
            &mut failed.kind
        else {
            unreachable!()
        };
        record.is_error = true;
        let mut unsequenced = event;
        unsequenced.sequence = 0;
        for invalid in [failed, unsequenced] {
            let evidence = SettledProgressEvidence::from_persisted_result(
                session,
                "original-call",
                ToolCallSettlement::Settled,
                Some(&invalid),
            );
            let mut window = ProgressWindow::default();
            window.set_objective("objective".into());
            let request = window
                .observe_with_evidence(
                    &serde_json::json!({"kind":"session", "id":session}),
                    "action",
                    None,
                    &evidence,
                )
                .unwrap();
            assert!(request["observations"][0]["evidence_key"].is_null());
            assert!(window.advice.is_none());
        }
    }

    use judgement::{
        ApplicationDisposition as Application, Feature, ObservationTerminal as Terminal,
        TriggerDisposition as Trigger,
    };

    const ALL: [Feature; 5] = [
        Feature::ToolResultTriage,
        Feature::ContextReranking,
        Feature::ProgressMonitoring,
        Feature::Recommendations,
        Feature::SessionCategorization,
    ];

    #[test]
    fn explicit_no_work_prerequisite_closes_without_inventing_inference() {
        let session = bcode_session_models::SessionId::new();
        let mut registry = super::TurnObservationStore::default();
        let turn = registry.begin(session, "prerequisite-test").unwrap();
        let operation = turn.begin_operation(
            judgement::Feature::ToolResultTriage,
            judgement::TriggerDisposition::Triggered,
        );
        operation.record_not_applicable();
        operation.finish(judgement::ApplicationDisposition::BaselineRetained, None);
        let json = registry
            .snapshot_json(session, "prerequisite-test")
            .unwrap();
        let snapshot: judgement::TurnJudgementObservation = serde_json::from_str(&json).unwrap();
        assert_eq!(
            snapshot.operations[0].trigger,
            judgement::TriggerDisposition::NotApplicable
        );
        assert!(snapshot.operations[0].inference_complete);
        assert!(snapshot.operations[0].inference.is_empty());
        let pending = turn.begin_operation(
            judgement::Feature::ToolResultTriage,
            judgement::TriggerDisposition::Triggered,
        );
        pending.inference_started();
        pending.record_not_applicable();
        let snapshot = turn.0.lock().unwrap();
        assert_eq!(
            snapshot.operations[1].trigger,
            judgement::TriggerDisposition::Triggered
        );
        assert!(!snapshot.operations[1].inference_complete);
        drop(snapshot);
    }

    #[test]
    fn disabled_complete_closure_is_stable_and_turns_are_isolated() {
        let session = bcode_session_models::SessionId::new();
        let mut store = TurnObservationStore::default();
        let first = store.begin(session, "first").unwrap();
        let second = store.begin(session, "second").unwrap();
        assert!(store.begin(session, "first").is_none());
        for feature in ALL {
            first
                .begin_operation(feature, Trigger::Disabled)
                .finish(Application::BaselineRetained, None);
        }
        first.close(Terminal::Completed, &ALL);
        let before = store.snapshot_json(session, "first").unwrap();
        first
            .begin_operation(Feature::Recommendations, Trigger::Triggered)
            .finish(Application::Applied, None);
        first.close(Terminal::Cancelled, &[]);
        assert_eq!(store.snapshot_json(session, "first").unwrap(), before);
        let snapshot: judgement::TurnJudgementObservation = serde_json::from_str(&before).unwrap();
        assert!(snapshot.complete);
        assert!(snapshot.operations.iter().all(|op| op.inference.is_empty()));
        second.close(Terminal::Completed, &ALL);
        let snapshot: judgement::TurnJudgementObservation =
            serde_json::from_str(&store.snapshot_json(session, "second").unwrap()).unwrap();
        assert!(!snapshot.complete); // Claimed coverage without checked triggers is not enough.
        assert!(store.snapshot_json(session, "legacy").is_none());
    }

    #[test]
    fn active_capacity_pending_application_and_future_versions_fail_closed() {
        let session = bcode_session_models::SessionId::new();
        let mut store = TurnObservationStore::default();
        let turn = store.begin(session, "pending").unwrap();
        let operation = turn.begin_operation(Feature::ToolResultTriage, Trigger::Triggered);
        operation.inference_started();
        operation.finish(Application::BaselineRetained, None);
        operation.record_inference(&judgement::OptionalResponse {
            response: None,
            observation: Some(judgement::OptionalOperationObservation::default()),
        });
        let snapshot: judgement::TurnJudgementObservation =
            serde_json::from_str(&store.snapshot_json(session, "pending").unwrap()).unwrap();
        assert_eq!(snapshot.operations[0].inference.len(), 1);
        assert_eq!(
            snapshot.operations[0].application,
            Some(Application::BaselineRetained)
        );
        for index in 0..31 {
            assert!(store.begin(session, &format!("active-{index}")).is_some());
        }
        assert!(store.begin(session, "over-capacity").is_none());
        assert!(store.begin(session, "pending").is_none());
        turn.close(Terminal::Completed, &[]);
        assert!(
            store
                .begin(session, "new-after-terminal-eviction")
                .is_some()
        );
        assert!(store.snapshot_json(session, "pending").is_none());
        let mut future = snapshot;
        future.schema_version += 1;
        assert!(future.validate().is_err());
        future.schema_version = judgement::TURN_OBSERVATION_VERSION;
        future.complete = true;
        assert!(future.validate().is_err());
    }

    #[test]
    fn finalized_passive_operations_aggregate_without_losing_totals_or_handles() {
        let session = bcode_session_models::SessionId::new();
        let mut store = TurnObservationStore::default();
        let turn = store.begin(session, "passive").unwrap();
        let active = turn.begin_operation(Feature::ToolResultTriage, Trigger::NotApplicable);
        // Keep this handle unfinished across compaction; its ordinal must remain stable.
        for feature in ALL.into_iter().cycle().take(200) {
            turn.begin_operation(feature, Trigger::Disabled)
                .finish(Application::BaselineRetained, None);
        }
        active.finish(Application::BaselineRetained, None);
        turn.close(Terminal::Completed, &ALL);
        let snapshot: judgement::TurnJudgementObservation =
            serde_json::from_str(&store.snapshot_json(session, "passive").unwrap()).unwrap();
        assert!(snapshot.complete);
        assert_eq!(snapshot.omitted, 0);
        assert_eq!(snapshot.accounting.as_ref().unwrap().begun, 201);
        assert_eq!(snapshot.accounting.as_ref().unwrap().omitted_operations, 0);
        assert_eq!(
            snapshot.operations.len() as u64
                + snapshot
                    .passive_aggregates
                    .iter()
                    .map(|op| op.count)
                    .sum::<u64>(),
            201
        );
        assert!(snapshot.operations.len() <= judgement::MAX_TURN_OPERATIONS);
        assert!(snapshot.operations.iter().any(
            |op| op.operation_id == 0 && op.application == Some(Application::BaselineRetained)
        ));
        assert!(
            snapshot
                .passive_aggregates
                .iter()
                .all(|op| op.application == Application::BaselineRetained)
        );
        assert!(snapshot.validate().is_ok());
        let mut malformed = snapshot;
        malformed.accounting.as_mut().unwrap().begun += 1;
        assert!(malformed.validate().is_err());
    }

    #[test]
    fn passive_detail_pending_and_late_mutation_cannot_be_certified() {
        let session = bcode_session_models::SessionId::new();
        let mut store = TurnObservationStore::default();
        let turn = store.begin(session, "details").unwrap();
        for _ in 0..65 {
            turn.begin_operation(Feature::ContextReranking, Trigger::Disabled)
                .finish(
                    Application::BaselineRetained,
                    Some(judgement::SelectionObservation {
                        candidate_count: 1,
                        ordered: vec![0],
                        admitted: vec![0],
                    }),
                );
        }
        turn.close(Terminal::Completed, &ALL);
        let snapshot: judgement::TurnJudgementObservation =
            serde_json::from_str(&store.snapshot_json(session, "details").unwrap()).unwrap();
        assert!(snapshot.passive_aggregates.is_empty());
        assert_eq!(snapshot.omitted, 1);
        assert_eq!(snapshot.accounting.unwrap().omitted_operations, 1);
        assert!(!snapshot.complete);
        let turn = store.begin(session, "pending-passive").unwrap();
        for _ in 0..65 {
            let operation =
                turn.begin_operation(Feature::ProgressMonitoring, Trigger::NotApplicable);
            operation.inference_started();
            operation.finish(Application::BaselineRetained, None);
        }
        turn.close(Terminal::Completed, &ALL);
        let snapshot: judgement::TurnJudgementObservation =
            serde_json::from_str(&store.snapshot_json(session, "pending-passive").unwrap())
                .unwrap();
        assert!(snapshot.passive_aggregates.is_empty());
        assert_eq!(snapshot.omitted, 1);
        assert!(!snapshot.complete);
        let turn = store.begin(session, "late").unwrap();
        let early = turn.begin_operation(Feature::ProgressMonitoring, Trigger::Disabled);
        early.finish(Application::BaselineRetained, None);
        for feature in ALL.into_iter().cycle().take(65) {
            turn.begin_operation(feature, Trigger::Disabled)
                .finish(Application::BaselineRetained, None);
        }
        early.inference_started();
        turn.close(Terminal::Completed, &ALL);
        let snapshot: judgement::TurnJudgementObservation =
            serde_json::from_str(&store.snapshot_json(session, "late").unwrap()).unwrap();
        assert!(!snapshot.complete);
        assert!(snapshot.omitted > 0);
    }

    #[test]
    fn racing_finalization_and_inference_cannot_certify_passive_work() {
        let session = bcode_session_models::SessionId::new();
        let mut store = TurnObservationStore::default();
        let turn = store.begin(session, "race").unwrap();
        let operation = turn.begin_operation(Feature::ProgressMonitoring, Trigger::NotApplicable);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        std::thread::scope(|scope| {
            let pending = operation.clone();
            let start = barrier.clone();
            scope.spawn(move || {
                start.wait();
                pending.inference_started();
            });
            barrier.wait();
            operation.finish(Application::BaselineRetained, None);
            for feature in ALL.into_iter().cycle().take(100) {
                turn.begin_operation(feature, Trigger::Disabled)
                    .finish(Application::BaselineRetained, None);
            }
        });
        turn.close(Terminal::Completed, &ALL);
        let snapshot: judgement::TurnJudgementObservation =
            serde_json::from_str(&store.snapshot_json(session, "race").unwrap()).unwrap();
        assert!(!snapshot.complete);
        // If start won the lock it remains pending; if compaction won, late start marks omitted.
        assert!(
            snapshot.omitted > 0
                || snapshot
                    .operations
                    .iter()
                    .any(|op| op.operation_id == 0 && !op.inference_complete)
        );
    }

    #[test]
    fn dropped_unfinished_handles_and_interleaved_detail_remain_truthful() {
        let session = bcode_session_models::SessionId::new();
        let mut store = TurnObservationStore::default();
        let turn = store.begin(session, "interleaved").unwrap();
        let dropped = turn.begin_operation(Feature::ProgressMonitoring, Trigger::Disabled);
        drop(dropped);
        for ordinal in 0..100 {
            let operation = turn.begin_operation(Feature::ContextReranking, Trigger::Disabled);
            let selection = (ordinal % 10 == 0).then(|| judgement::SelectionObservation {
                candidate_count: 1,
                ordered: vec![0],
                admitted: vec![0],
            });
            operation.finish(Application::BaselineRetained, selection);
        }
        turn.close(Terminal::Completed, &ALL);
        let snapshot: judgement::TurnJudgementObservation =
            serde_json::from_str(&store.snapshot_json(session, "interleaved").unwrap()).unwrap();
        assert!(!snapshot.complete);
        assert_eq!(snapshot.omitted, 0);
        assert_eq!(
            snapshot
                .operations
                .iter()
                .filter(|op| op.selection.is_some())
                .count(),
            10
        );
        assert_eq!(
            snapshot.operations.len() as u64
                + snapshot
                    .passive_aggregates
                    .iter()
                    .map(|op| op.count)
                    .sum::<u64>(),
            101
        );
        assert!(snapshot.validate().is_ok());
        let aggregate = &snapshot.passive_aggregates[0];
        assert!(aggregate.count < aggregate.last_operation_id - aggregate.first_operation_id + 1);
    }

    #[test]
    fn cancellation_overflow_and_missing_inference_fail_closed() {
        let session = bcode_session_models::SessionId::new();
        let mut store = TurnObservationStore::default();
        let turn = store.begin(session, "cancelled").unwrap();
        let operation = turn.begin_operation(Feature::ToolResultTriage, Trigger::Triggered);
        operation.inference_started();
        operation.finish(Application::Cancelled, None);
        turn.close(Terminal::Cancelled, &ALL);
        operation.record_inference(&judgement::OptionalResponse {
            response: None,
            observation: Some(judgement::OptionalOperationObservation::default()),
        });
        let snapshot: judgement::TurnJudgementObservation =
            serde_json::from_str(&store.snapshot_json(session, "cancelled").unwrap()).unwrap();
        assert!(!snapshot.complete);
        assert!(snapshot.operations[0].inference.is_empty());
        let turn = store.begin(session, "overflow").unwrap();
        for _ in 0..65 {
            turn.begin_operation(Feature::ToolResultTriage, Trigger::Triggered)
                .finish(Application::BaselineRetained, None);
        }
        turn.close(Terminal::Completed, &ALL);
        let snapshot: judgement::TurnJudgementObservation =
            serde_json::from_str(&store.snapshot_json(session, "overflow").unwrap()).unwrap();
        assert_eq!(snapshot.operations.len(), 64);
        assert_eq!(snapshot.omitted, 1);
        assert!(!snapshot.complete);
    }
}
