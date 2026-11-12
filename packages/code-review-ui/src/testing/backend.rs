//! The real data layer for scenario tests and benchmarks, without a daemon.
//!
//! [`TestBackend`] owns [`ReviewServices`] over a private state directory, so stored-state
//! operations (drafts, threads, AI exchanges, suggestions, workspaces) go to the real store the
//! daemon plugin also serves. Its [`ScriptedHost`] records sessions instead of creating them
//! and serves the optional fake provider and publisher plugins. Calls answer on the calling
//! task, so scenarios observe completions in a fixed order.

use std::sync::Arc;

use bcode_code_review::{CodeReviewConfig, CodeReviewStateLocation, ReviewStore};

use super::fake_provider::FakeProvider;
use super::fake_publisher::FakePublisher;
use super::harness::ScriptedHost;
use crate::host::ReviewHost;
use crate::services::{Operation, ReviewServices};

/// Review data services for tests.
pub struct TestBackend {
    host: Arc<ScriptedHost>,
    services: ReviewServices,
}

impl Default for TestBackend {
    fn default() -> Self {
        let state = tempfile::tempdir().expect("state directory");
        let store = ReviewStore::new(CodeReviewConfig {
            state_location: CodeReviewStateLocation::User,
            state_dir: Some(state.path().to_path_buf()),
        });
        let host = Arc::new(ScriptedHost::new());
        // The services keep the state directory alive as long as any screen uses them.
        let services = ReviewServices::inline(
            store,
            Arc::clone(&host) as Arc<dyn ReviewHost>,
            Arc::new(state),
        );
        Self { host, services }
    }
}

impl std::fmt::Debug for TestBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestBackend").finish_non_exhaustive()
    }
}

impl TestBackend {
    /// Enable `provider` as the only review-source provider, registered in-process through the
    /// JSON adapter (the way a plugin-backed provider is reached) so its calls are recorded.
    ///
    /// # Panics
    ///
    /// Panics when the fake cannot answer its manifest (a test bug).
    #[must_use]
    pub fn with_provider(mut self, provider: FakeProvider) -> Self {
        let provider = Arc::new(provider);
        self.host.set_provider(Arc::clone(&provider));
        let manifest = serde_json::from_slice(
            &provider
                .answer(bcode_code_review_models::OP_REVIEW_SOURCE_MANIFEST, b"{}")
                .expect("fake manifest"),
        )
        .expect("fake manifest");
        self.services = self.services.with_source(Arc::new(
            bcode_code_review::provider::JsonReviewSource::new(
                manifest,
                Box::new(move |operation, payload| provider.answer(operation, &payload)),
            ),
        ));
        self
    }

    /// Enable `publisher` as the only external review publisher and importer.
    #[must_use]
    pub fn with_publisher(self, publisher: FakePublisher) -> Self {
        self.host.set_publisher(publisher);
        self
    }

    /// Use `limit` for stacks.
    #[must_use]
    pub fn with_stack_limit(mut self, limit: bcode_code_review_models::ReviewStackLimit) -> Self {
        self.services = self.services.with_stack_limit(limit);
        self
    }

    /// The services screens are built with.
    #[must_use]
    pub fn services(&self) -> ReviewServices {
        self.services.clone()
    }

    /// The enabled fake publisher.
    #[must_use]
    pub fn publisher(&self) -> Option<&FakePublisher> {
        self.host.publisher()
    }

    /// The enabled fake provider.
    #[must_use]
    pub fn provider(&self) -> Option<&FakeProvider> {
        self.host.provider()
    }

    /// Sessions created so far: `(id, title)`.
    #[must_use]
    pub fn sessions(&self) -> Vec<(String, String)> {
        self.host.sessions()
    }

    /// Origins recorded on session creation so far, in request order.
    #[must_use]
    pub fn session_origins(&self) -> Vec<bcode_session_models::SessionOrigin> {
        self.host.session_origins()
    }

    /// Messages sent so far: `(session id, text)`.
    #[must_use]
    pub fn messages(&self) -> Vec<(String, String)> {
        self.host.messages()
    }

    /// Fail creating sessions with `message` (see [`ScriptedHost::fail_sessions`]).
    pub fn fail_sessions(&self, message: Option<&str>) {
        self.host.fail_sessions(message);
    }

    /// Make every later call of `operation` fail (or succeed again when `fail` is false).
    pub fn set_failing(&self, operation: &str, fail: bool) {
        self.services.set_failing(operation, fail);
    }

    /// Answer `request` directly (for test setup and assertions), outside the call log and
    /// injected failures.
    ///
    /// # Panics
    ///
    /// Panics when the request fails.
    #[must_use]
    pub fn call_now<O: Operation>(&self, request: O) -> O::Response {
        let name = O::NAME;
        self.services
            .answer(request)
            .unwrap_or_else(|error| panic!("{name}: {}", error.message))
    }
}
