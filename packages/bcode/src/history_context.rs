//! Explicit bounded indexed-history context for SDK requests.

use crate::{BcodeClient, MemoryPrivacy, MessageRole, ModelContentBlock, ModelMessage};
use bcode_session_search::{
    FederatedSessionSearchResponse, SessionSearchContentRoute, SessionSearchPlanPolicy,
    SessionSearchRequest,
};
use std::sync::{Arc, Mutex};

/// Opt-in indexed-history retrieval with caller-owned scope and independent judgement consent.
///
/// One configured search runs per SDK request with hydration disabled. The query, filters,
/// routes and search policy are never inferred from the current prompt or broadened. Only
/// whole bounded previews are optional context; canonical history and transcript stay unchanged.
/// Dropping the local future does not issue a remote cancellation request.
///
/// Attach a caller-configured query and inspect its report after sending a normal SDK request:
///
/// ```no_run
/// # #[cfg(feature = "daemon-client")]
/// # fn example(request: bcode_session_search::SessionSearchRequest,
/// # policy: bcode_session_search::SessionSearchPlanPolicy,
/// # routes: Vec<bcode_session_search::SessionSearchContentRoute>) {
/// use bcode::{Agent, BcodeClient, DaemonHistoryContextProvider, MemoryPrivacy};
/// let history = DaemonHistoryContextProvider::new(
///     BcodeClient::default_endpoint(), request, policy, routes, 4096, MemoryPrivacy::Private,
/// );
/// // No judgement transfer is authorized. Opt in separately only when appropriate:
/// // let history = history.with_judgement_transfer(MemoryPrivacy::Private);
/// let session = Agent::builder().build().session().with_history_context_provider(history.clone());
/// # let _ = (session, history.report());
/// # }
/// ```
#[derive(Clone)]
pub struct DaemonHistoryContextProvider {
    client: BcodeClient,
    request: SessionSearchRequest,
    policy: SessionSearchPlanPolicy,
    routes: Vec<SessionSearchContentRoute>,
    budget_bytes: usize,
    privacy: MemoryPrivacy,
    judgement_transfer: Option<MemoryPrivacy>,
    report: Arc<Mutex<Option<FederatedSessionSearchResponse>>>,
}

impl DaemonHistoryContextProvider {
    /// Configure one explicit search and the optional whole-preview context budget.
    ///
    /// `privacy` is the caller's classification of the requested history. This constructor
    /// grants incorporation into the conversational model, not transfer to judgement.
    #[must_use]
    pub fn new(
        client: BcodeClient,
        request: SessionSearchRequest,
        policy: SessionSearchPlanPolicy,
        routes: Vec<SessionSearchContentRoute>,
        budget_bytes: usize,
        privacy: MemoryPrivacy,
    ) -> Self {
        Self {
            client,
            request,
            policy,
            routes,
            budget_bytes,
            privacy,
            judgement_transfer: None,
            report: Arc::new(Mutex::new(None)),
        }
    }

    /// Separately authorize transfer of task and history previews to optional judgement.
    ///
    /// No history transfers by default, including Public history. Private/sensitive history
    /// requires an explicit matching grant here; remote search authorization is not this grant.
    #[must_use]
    pub const fn with_judgement_transfer(mut self, maximum_privacy: MemoryPrivacy) -> Self {
        self.judgement_transfer = Some(maximum_privacy);
        self
    }

    /// Last bounded response, including incomplete coverage, provider outcomes and failures.
    ///
    /// A transport failure clears the prior report rather than substituting stale history.
    #[must_use]
    pub fn report(&self) -> Option<FederatedSessionSearchResponse> {
        self.report.lock().ok()?.clone()
    }

    pub(crate) async fn context_messages(&self, task: &str) -> Vec<ModelMessage> {
        if let Ok(mut report) = self.report.lock() {
            *report = None;
        }
        if self.budget_bytes == 0
            || self.policy.execution_class
                != bcode_session_search::SessionSearchExecutionClass::Ordinary
            || self.request.validate().is_err()
            || self.policy.validate(&self.request).is_err()
        {
            return Vec::new();
        }
        let Ok((response, hydrated)) = self
            .client
            .session_search(
                self.request.clone(),
                self.policy.clone(),
                self.routes.clone(),
                false,
            )
            .await
        else {
            return Vec::new();
        };
        if let Ok(mut report) = self.report.lock() {
            *report = Some(response.clone());
        }
        // Fail closed on a boundary violation; never use unexpected hydration payloads.
        if !hydrated.is_empty() {
            return Vec::new();
        }
        let mut candidates = Vec::new();
        let mut ids = std::collections::BTreeSet::new();
        for hit in &response.hits {
            if let Some(candidate) = hit.judgement_context_candidate()
                && ids.insert(candidate.id.clone())
            {
                candidates.push(candidate);
            }
            if candidates.len() == 32 {
                break;
            }
        }
        let pressure = candidates.iter().fold(0usize, |sum, candidate| {
            sum.saturating_add(candidate.snippet.len())
        }) > self.budget_bytes;
        let order = if pressure
            && candidates.len() > 1
            && task.len() <= 8192
            && self
                .judgement_transfer
                .is_some_and(|maximum| self.privacy <= maximum)
        {
            self.client
                .rank_optional_context(task, &candidates)
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        admit_history(&candidates, order.as_deref(), self.budget_bytes)
    }
}

fn admit_history(
    candidates: &[bcode_session_models::JudgementContextCandidate],
    advice: Option<&[String]>,
    budget_bytes: usize,
) -> Vec<ModelMessage> {
    let baseline = candidates
        .iter()
        .map(|candidate| candidate.id.clone())
        .collect::<Vec<_>>();
    let order = advice
        .filter(|order| {
            let unique = order.iter().collect::<std::collections::BTreeSet<_>>();
            order.len() == candidates.len()
                && unique.len() == candidates.len()
                && baseline.iter().all(|id| unique.contains(id))
        })
        .unwrap_or(&baseline);
    let mut remaining = budget_bytes;
    let mut messages = Vec::new();
    for id in order {
        if let Some(candidate) = candidates.iter().find(|candidate| &candidate.id == id)
            && candidate.snippet.len() <= remaining
        {
            remaining -= candidate.snippet.len();
            messages.push(ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text {
                    text: format!(
                        "Optional indexed history [{}]:\n{}",
                        candidate.provenance, candidate.snippet
                    ),
                }],
            });
        }
    }
    messages
}
