//! Asynchronous portable session-search effect state for TUI adapters.

use std::time::Duration;

/// Default debounce applied before dispatching a changed transcript query.
pub const SESSION_SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);

/// Terminal result accepted for the latest query generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSearchCompletion {
    /// Monotonic renderer-local query generation.
    pub generation: u64,
    /// Portable terminal aggregate from the application boundary.
    pub response: bcode_session_search::FederatedSessionSearchResponse,
    /// Optional exact canonical hydration outcomes.
    pub hydrated_hits: Vec<bcode_session_search::HydratedSessionSearchHit>,
}

/// One renderer-ready row preserving portable result and coverage semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSearchPresentationRow {
    pub session_id: bcode_session_models::SessionId,
    pub session_title: Option<String>,
    pub sequence: u64,
    pub content_kind: bcode_session_search::SearchContentKind,
    pub provider_id: String,
    pub provider_rank: u32,
    pub preview: Option<String>,
    pub preview_truncated: bool,
    pub timestamp_ms: Option<u64>,
    pub hydration: Option<bcode_session_search::SearchHitHydrationOutcome>,
}

/// Renderer-ready terminal search presentation without provider or canonical-state ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSearchPresentation {
    pub rows: Vec<SessionSearchPresentationRow>,
    pub query_complete: bool,
    pub coverage_complete: bool,
    pub degraded: bool,
    pub provider_reports: usize,
    pub failures: usize,
}

impl SessionSearchCompletion {
    /// Adapt portable hits and exact hydration outcomes into bounded renderer-ready rows.
    #[must_use]
    pub fn presentation(&self) -> SessionSearchPresentation {
        self.presentation_with_summaries(&[])
    }

    /// Adapt hits while resolving display titles only from canonical catalog summaries.
    #[must_use]
    pub fn presentation_with_summaries(
        &self,
        summaries: &[bcode_session_models::SessionSummary],
    ) -> SessionSearchPresentation {
        let titles = summaries
            .iter()
            .map(|summary| (summary.id, summary.display_title().to_owned()))
            .collect::<std::collections::BTreeMap<_, _>>();
        let hydration = self
            .hydrated_hits
            .iter()
            .map(|hit| (hit.hit.locator.clone(), hit))
            .collect::<std::collections::BTreeMap<_, _>>();
        let rows = self
            .response
            .hits
            .iter()
            .map(|hit| {
                let hydrated = hydration.get(&hit.locator);
                SessionSearchPresentationRow {
                    session_id: hit.locator.session_id,
                    session_title: titles.get(&hit.locator.session_id).cloned(),
                    sequence: hit.locator.sequence,
                    content_kind: hit.content_kind,
                    provider_id: hit.provider_id.clone(),
                    provider_rank: hit.provider_rank,
                    preview: hit.preview.clone(),
                    preview_truncated: hit.preview_truncated,
                    timestamp_ms: hydrated
                        .and_then(|hydrated| hydrated.event.as_ref())
                        .map(|event| event.timestamp_ms),
                    hydration: hydrated.map(|hydrated| hydrated.outcome),
                }
            })
            .collect();
        SessionSearchPresentation {
            rows,
            query_complete: self.response.query_complete,
            coverage_complete: self.response.coverage_complete,
            degraded: !self.response.query_complete
                || !self.response.coverage_complete
                || !self.response.failures.is_empty(),
            provider_reports: self.response.providers.len(),
            failures: self.response.failures.len(),
        }
    }
}

/// Owns one replaceable debounced TUI search task.
#[derive(Debug, Default)]
pub struct SessionSearchEffect {
    generation: u64,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl SessionSearchEffect {
    /// Return the latest renderer-local query generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Replace any pending/running query with a debounced portable search request.
    ///
    /// The prior task is aborted locally. Provider deadlines and cancellation remain enforced by
    /// the application/plugin boundary; stale completions are also rejected by generation.
    pub fn replace(
        &mut self,
        client: bcode_client::BcodeClient,
        request: bcode_session_search::SessionSearchRequest,
        policy: bcode_session_search::SessionSearchPlanPolicy,
        routes: Vec<bcode_session_search::SessionSearchContentRoute>,
        hydrate: bool,
        completion_tx: tokio::sync::mpsc::UnboundedSender<SessionSearchCompletion>,
    ) -> u64 {
        self.cancel();
        self.generation = self.generation.saturating_add(1);
        let generation = self.generation;
        self.task = Some(tokio::spawn(async move {
            tokio::time::sleep(SESSION_SEARCH_DEBOUNCE).await;
            if let Ok((response, hydrated_hits)) = client
                .session_search(request, policy, routes, hydrate)
                .await
            {
                let _ = completion_tx.send(SessionSearchCompletion {
                    generation,
                    response,
                    hydrated_hits,
                });
            }
        }));
        generation
    }

    /// Abort pending/running renderer work and advance the generation so late results are stale.
    pub fn cancel(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.generation = self.generation.saturating_add(1);
    }

    /// Accept a completion only when it belongs to the latest generation.
    #[must_use]
    pub fn accept(&self, completion: SessionSearchCompletion) -> Option<SessionSearchCompletion> {
        (completion.generation == self.generation).then_some(completion)
    }
}

impl Drop for SessionSearchEffect {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion(generation: u64) -> SessionSearchCompletion {
        SessionSearchCompletion {
            generation,
            response: bcode_session_search::FederatedSessionSearchResponse {
                hits: Vec::new(),
                query_complete: true,
                coverage_complete: true,
                providers: Vec::new(),
                failures: Vec::new(),
            },
            hydrated_hits: Vec::new(),
        }
    }

    #[test]
    fn presentation_keeps_content_locator_preview_timestamp_and_degradation_explicit() {
        let session_id = bcode_session_models::SessionId::new();
        let hit = bcode_session_search::SessionSearchHit {
            locator: bcode_session_search::SessionSearchLocator {
                session_id,
                sequence: 7,
                record_id: Some("record".to_owned()),
            },
            content_kind: bcode_session_search::SearchContentKind::UserMessage,
            matched_field: bcode_session_search::SearchField::Text,
            provider_id: "provider".to_owned(),
            provider_rank: 1,
            provider_score: None,
            preview: Some("bounded preview".to_owned()),
            preview_truncated: true,
        };
        let completion = SessionSearchCompletion {
            generation: 1,
            response: bcode_session_search::FederatedSessionSearchResponse {
                hits: vec![hit.clone()],
                query_complete: false,
                coverage_complete: false,
                providers: Vec::new(),
                failures: Vec::new(),
            },
            hydrated_hits: vec![bcode_session_search::HydratedSessionSearchHit {
                hit,
                outcome: bcode_session_search::SearchHitHydrationOutcome::Hydrated,
                event: Some(Box::new(bcode_session_models::SessionEvent {
                    schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                    sequence: 7,
                    timestamp_ms: 99,
                    session_id,
                    provenance: None,
                    kind: bcode_session_models::SessionEventKind::UserMessage {
                        client_id: bcode_session_models::ClientId::new(),
                        text: "canonical".to_owned(),
                        admission: bcode_session_models::TurnAdmissionMetadata::default(),
                    },
                })),
                message: None,
            }],
        };

        let mut summary = bcode_session_models::SessionSummary {
            id: session_id,
            name: Some("Canonical title".to_owned()),
            explicit_name: None,
            derived_title: None,
            title_source: bcode_session_models::SessionTitleSource::Explicit,
            client_count: 0,
            created_at_ms: 1,
            updated_at_ms: 1,
            working_directory: std::path::PathBuf::new(),
            import: None,
            fork: None,
            execution: None,
        };
        let presentation = completion.presentation_with_summaries(std::slice::from_ref(&summary));
        summary.name = Some("Changed later".to_owned());
        assert!(presentation.degraded);
        assert_eq!(presentation.rows.len(), 1);
        assert_eq!(presentation.rows[0].session_id, session_id);
        assert_eq!(
            presentation.rows[0].session_title.as_deref(),
            Some("Canonical title")
        );
        assert_eq!(presentation.rows[0].sequence, 7);
        assert_eq!(presentation.rows[0].timestamp_ms, Some(99));
        assert_eq!(
            presentation.rows[0].preview.as_deref(),
            Some("bounded preview")
        );
        assert!(presentation.rows[0].preview_truncated);
        assert_eq!(
            presentation.rows[0].hydration,
            Some(bcode_session_search::SearchHitHydrationOutcome::Hydrated)
        );
    }

    #[test]
    fn stale_completions_cannot_replace_latest_terminal_state() {
        let mut effect = SessionSearchEffect::default();
        effect.generation = 2;
        assert!(effect.accept(completion(1)).is_none());
        assert!(effect.accept(completion(2)).is_some());
    }

    #[test]
    fn partial_timeout_and_disabled_terminal_aggregates_remain_explicit() {
        let partial = SessionSearchCompletion {
            generation: 1,
            response: bcode_session_search::FederatedSessionSearchResponse {
                hits: Vec::new(),
                query_complete: false,
                coverage_complete: false,
                providers: Vec::new(),
                failures: vec![bcode_session_search::SessionSearchProviderFailure {
                    plugin_id: "slow".to_owned(),
                    error: bcode_session_search::SessionSearchServiceError {
                        code: bcode_session_search::SearchErrorCode::DeadlineExceeded,
                        message: "deadline".to_owned(),
                        retryable: true,
                    },
                    stage: bcode_session_search::SessionSearchProviderStage::Execution,
                    elapsed_ms: 100,
                    content: Vec::new(),
                }],
            },
            hydrated_hits: Vec::new(),
        };
        let disabled = SessionSearchCompletion {
            generation: 2,
            response: bcode_session_search::FederatedSessionSearchResponse {
                hits: Vec::new(),
                query_complete: false,
                coverage_complete: false,
                providers: Vec::new(),
                failures: Vec::new(),
            },
            hydrated_hits: Vec::new(),
        };
        let mut effect = SessionSearchEffect::default();
        effect.generation = 1;
        let accepted = effect.accept(partial).expect("latest partial result");
        assert!(!accepted.response.query_complete);
        assert!(!accepted.response.coverage_complete);
        assert_eq!(
            accepted.response.failures[0].error.code,
            bcode_session_search::SearchErrorCode::DeadlineExceeded
        );
        effect.generation = 2;
        let accepted = effect.accept(disabled).expect("latest disabled result");
        assert!(accepted.response.providers.is_empty());
        assert!(!accepted.response.query_complete);
    }

    #[tokio::test]
    async fn cancel_aborts_pending_debounce_and_advances_generation() {
        let mut effect = SessionSearchEffect::default();
        effect.generation = 4;
        effect.task = Some(tokio::spawn(async {
            tokio::time::sleep(Duration::from_mins(1)).await;
        }));
        effect.cancel();
        assert_eq!(effect.generation(), 5);
        assert!(effect.task.is_none());
    }
}
