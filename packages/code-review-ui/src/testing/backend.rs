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
    /// Enable `provider` as the only review-source provider.
    #[must_use]
    pub fn with_provider(self, provider: FakeProvider) -> Self {
        self.host.set_provider(provider);
        self
    }

    /// Enable `publisher` as the only external review publisher and importer.
    #[must_use]
    pub fn with_publisher(self, publisher: FakePublisher) -> Self {
        self.host.set_publisher(publisher);
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

    /// Messages sent so far: `(session id, text)`.
    #[must_use]
    pub fn messages(&self) -> Vec<(String, String)> {
        self.host.messages()
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
