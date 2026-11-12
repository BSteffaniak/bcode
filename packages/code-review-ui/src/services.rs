//! Typed access to review data.
//!
//! [`ReviewServices`] is the UI's handle on everything a review reads or writes:
//!
//! * repository reads (sources, file indexes, hunks, blob lines, search, attribution) through a
//!   shared [`ReviewData`];
//! * stored review state (workspaces, drafts and threads, AI exchanges, suggestions, publish
//!   records, imports) through a [`ReviewStore`];
//! * review-source providers, publishers, and importers through the host's plugin services
//!   ([`ReviewServices::transport`]).
//!
//! Every request type implements [`Operation`], so a call is `services.call(request).await` and
//! the response type follows from the request. Calls run on the blocking pool (repository reads
//! spawn git processes and stored state opens a database), never on the UI thread. The data and
//! store are the same code the code review plugin serves, so the UI and the daemon agree on
//! every record.

use std::sync::{Arc, Mutex};

use bcode_code_review::provider::{
    DiscoveryContributor, JsonCall, JsonDiscovery, JsonReviewSource, ProviderCallError,
    ProviderRegistry, ReviewSourceProvider, plugin_manifests,
};
use bcode_code_review::{DataError, ReviewData, ReviewStore, StoreError};
use bcode_code_review_models as models;
use bcode_code_review_models::ReviewStackLimit;

use crate::host::{HostTransport, ReviewHost};

/// Error code of stored-state failures.
pub const STORE_ERROR_FAILED: &str = "review_store_failed";

/// Result of one [`ReviewServices`] call.
pub type ServiceResult<T> = Result<T, ProviderCallError>;

/// One request the review data layer answers.
pub trait Operation: Send + 'static {
    /// Response of the request.
    type Response: Send + 'static;

    /// Stable operation name (the code review service operation of the same request).
    const NAME: &'static str;

    /// Answer the request.
    ///
    /// # Errors
    ///
    /// * The data layer's failure, with its stable error code.
    fn run(self, data: &Mutex<ReviewData>, store: &ReviewStore) -> ServiceResult<Self::Response>;
}

/// Preview a client-built bundle with a built-in publisher.
#[derive(Debug, Clone)]
pub struct PreviewBundle(pub models::PublishBundleRequest);

/// Submit a client-built bundle with a built-in publisher.
#[derive(Debug, Clone)]
pub struct SubmitBundle(pub models::PublishBundleRequest);

fn data_error(error: &DataError) -> ProviderCallError {
    ProviderCallError {
        code: error.code().to_owned(),
        message: error.to_string(),
    }
}

fn store_error(error: &StoreError) -> ProviderCallError {
    ProviderCallError {
        code: STORE_ERROR_FAILED.to_owned(),
        message: error.to_string(),
    }
}

fn with_data<T>(
    data: &Mutex<ReviewData>,
    call: impl FnOnce(&mut ReviewData) -> Result<T, DataError>,
) -> ServiceResult<T> {
    let mut data = data.lock().map_err(|_| ProviderCallError {
        code: models::REVIEW_SOURCE_ERROR_FAILED.to_owned(),
        message: "repository reader is unavailable after a failed read".to_owned(),
    })?;
    // There is no background timer; idle `cat-file` processes stop on the next call.
    data.evict_idle();
    let result = call(&mut data);
    drop(data);
    result.map_err(|error| data_error(&error))
}

/// Implements [`Operation`] for repository reads.
macro_rules! data_operations {
    ($($request:ty => $response:ty, $name:expr, $method:ident;)*) => {
        $(
            impl Operation for $request {
                type Response = $response;
                const NAME: &'static str = $name;

                fn run(
                    self,
                    data: &Mutex<ReviewData>,
                    _store: &ReviewStore,
                ) -> ServiceResult<Self::Response> {
                    with_data(data, |data| data.$method(&self))
                }
            }
        )*
    };
}

/// Implements [`Operation`] for stored-state requests.
macro_rules! store_operations {
    ($($request:ty => $response:ty, $name:expr, |$store:ident, $req:ident| $body:expr;)*) => {
        $(
            impl Operation for $request {
                type Response = $response;
                const NAME: &'static str = $name;

                fn run(
                    self,
                    _data: &Mutex<ReviewData>,
                    $store: &ReviewStore,
                ) -> ServiceResult<Self::Response> {
                    let $req = self;
                    $body.map_err(|error| store_error(&error))
                }
            }
        )*
    };
}

data_operations! {
    models::ResolveReviewSourcesRequest => models::ResolveReviewSourcesResponse,
        models::OP_REVIEW_SOURCES_RESOLVE, resolve_sources;
    models::ReviewSourcesTokenRequest => models::ReviewSourcesTokenResponse,
        models::OP_REVIEW_SOURCES_TOKEN, sources_token;
    models::ReviewIndexRequest => models::ReviewIndexResponse,
        models::OP_REVIEW_INDEX_GET, index_get;
    models::ReviewFileHunksRequest => models::ReviewFileHunksResponse,
        models::OP_REVIEW_FILE_HUNKS_GET, file_hunks_get;
    models::ReviewBlobLinesRequest => models::ReviewBlobLinesResponse,
        models::OP_REVIEW_BLOB_LINES_GET, blob_lines_get;
    models::ReviewRefsSearchRequest => models::ReviewRefsSearchResponse,
        models::OP_REVIEW_REFS_SEARCH, refs_search;
    models::ReviewCommitsPageRequest => models::ReviewCommitsPageResponse,
        models::OP_REVIEW_COMMITS_PAGE, commits_page;
    models::ReviewFilesSearchRequest => models::ReviewFilesSearchResponse,
        models::OP_REVIEW_FILES_SEARCH, files_search;
    models::ReviewStackAttributionRequest => models::ReviewStackAttributionResponse,
        models::OP_REVIEW_STACK_ATTRIBUTION, stack_attribution;
    models::ReviewRepoStateRequest => models::ReviewRepoStateResponse,
        models::OP_REVIEW_REPO_STATE, repo_state;
    models::ReviewRevisionsCompareRequest => models::ReviewRevisionsCompareResponse,
        models::OP_REVIEW_REVISIONS_COMPARE, revisions_compare;
    models::ReviewPatchIdsRequest => models::ReviewPatchIdsResponse,
        models::OP_REVIEW_PATCH_IDS_GET, patch_ids_get;
}

store_operations! {
    models::ListReviewWorkspacesRequest => models::ListReviewWorkspacesResponse,
        models::OP_REVIEW_WORKSPACE_LIST, |store, request| store.list_workspaces(&request);
    models::CreateReviewWorkspaceRequest => models::CreateReviewWorkspaceResponse,
        models::OP_REVIEW_WORKSPACE_CREATE, |store, request| store.create_workspace(request);
    models::GetReviewWorkspaceRequest => models::GetReviewWorkspaceResponse,
        models::OP_REVIEW_WORKSPACE_GET, |store, request| store.get_workspace(request);
    models::UpdateReviewWorkspaceRequest => models::UpdateReviewWorkspaceResponse,
        models::OP_REVIEW_WORKSPACE_UPDATE, |store, request| store.update_workspace(request);
    models::ArchiveReviewWorkspaceRequest => models::ArchiveReviewWorkspaceResponse,
        models::OP_REVIEW_WORKSPACE_ARCHIVE, |store, request| store.archive_workspace(request);
    models::ListDraftsRequest => models::ListDraftsResponse,
        "draft.list", |store, request| store.list_drafts(&request);
    models::SaveDraftRequest => models::SaveDraftResponse,
        "draft.save", |store, request| store.save_draft(request);
    models::DeleteDraftRequest => models::DeleteDraftResponse,
        "draft.delete", |store, request| store.delete_draft(request);
    models::UpdateDraftRequest => models::UpdateDraftResponse,
        "draft.update", |store, request| store.update_draft(request);
    models::LinkThreadSessionRequest => models::LinkThreadSessionResponse,
        "thread.link_session", |store, request| store.link_thread_session(request);
    models::ResolveThreadRequest => models::ResolveThreadResponse,
        "thread.resolve", |store, request| store.resolve_thread(request);
    models::ClassifyThreadRequest => models::ClassifyThreadResponse,
        models::OP_THREAD_CLASSIFY, |store, request| store.classify_thread(request);
    models::ListReviewAiExchangesRequest => models::ListReviewAiExchangesResponse,
        models::OP_REVIEW_AI_EXCHANGES_LIST, |store, request| store.list_ai_exchanges(&request);
    models::SaveReviewAiExchangeRequest => models::SaveReviewAiExchangeResponse,
        models::OP_REVIEW_AI_EXCHANGE_SAVE, |store, request| store.save_ai_exchange(request);
    models::DeleteReviewAiExchangeRequest => models::DeleteReviewAiExchangeResponse,
        models::OP_REVIEW_AI_EXCHANGE_DELETE, |store, request| store.delete_ai_exchange(request);
    models::ListReviewSuggestionsRequest => models::ListReviewSuggestionsResponse,
        models::OP_REVIEW_SUGGESTIONS_LIST, |store, request| store.list_suggestions(&request);
    models::SaveReviewSuggestionRequest => models::SaveReviewSuggestionResponse,
        models::OP_REVIEW_SUGGESTION_SAVE, |store, request| store.save_suggestion(request);
    models::DeleteReviewSuggestionRequest => models::DeleteReviewSuggestionResponse,
        models::OP_REVIEW_SUGGESTION_DELETE, |store, request| store.delete_suggestion(request);
    models::ListPublishRecordsRequest => models::ListPublishRecordsResponse,
        models::OP_REVIEW_PUBLISH_RECORDS_LIST,
        |store, request| store.list_publish_records(&request);
    models::SavePublishRecordRequest => models::SavePublishRecordResponse,
        models::OP_REVIEW_PUBLISH_RECORD_SAVE, |store, request| store.save_publish_record(request);
    models::ListExternalReviewImportsRequest => models::ListExternalReviewImportsResponse,
        models::OP_REVIEW_EXTERNAL_IMPORTS_LIST,
        |store, request| store.list_external_imports(&request);
    models::SaveExternalReviewImportRequest => models::SaveExternalReviewImportResponse,
        models::OP_REVIEW_EXTERNAL_IMPORT_SAVE,
        |store, request| store.save_external_import(request);
    models::BindReviewAssistantRequest => models::BindReviewAssistantResponse,
        "review.assistant.bind", |store, request| store.bind_assistant(request);
    models::GetReviewAssistantRequest => models::GetReviewAssistantResponse,
        "review.assistant.get", |store, request| store.assistant_binding(&request);
    PreviewBundle => models::PublishReviewPreviewResponse,
        models::OP_REVIEW_PUBLISH_BUNDLE_PREVIEW,
        |_store, request| bcode_code_review::store::preview_bundle(&request.0);
    SubmitBundle => models::PublishReviewResponse,
        models::OP_REVIEW_PUBLISH_BUNDLE_SUBMIT,
        |_store, request| bcode_code_review::store::submit_bundle(&request.0);
}

/// Where calls run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Execution {
    /// On the Tokio blocking pool.
    Pool,
    /// On the calling task, in call order (deterministic test scenarios).
    #[cfg(test)]
    Inline,
}

/// The UI's handle on review data; cheap to clone, shared by every task.
#[derive(Clone)]
pub struct ReviewServices {
    data: Arc<Mutex<ReviewData>>,
    store: ReviewStore,
    host: Arc<dyn ReviewHost>,
    execution: Execution,
    stack_limit: ReviewStackLimit,
    /// Providers and contributors: built-in ones, plus plugin-backed ones once found.
    providers: Arc<Mutex<ProviderRegistry>>,
    /// Whether plugin-backed providers were looked for (once, in the background).
    plugins_sought: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    probe: Arc<probe::Probe>,
    /// Test state (such as a temporary state directory) that lives as long as any clone.
    #[cfg(test)]
    _guard: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl std::fmt::Debug for ReviewServices {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReviewServices")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

impl ReviewServices {
    /// Services over `store` and the plugin services of `host`.
    #[must_use]
    pub fn new(store: ReviewStore, host: Arc<dyn ReviewHost>) -> Self {
        Self {
            data: Arc::default(),
            store,
            host,
            execution: Execution::Pool,
            stack_limit: ReviewStackLimit::default(),
            providers: Arc::default(),
            plugins_sought: Arc::default(),
            #[cfg(test)]
            probe: Arc::default(),
            #[cfg(test)]
            _guard: None,
        }
    }

    /// Use `limit` for stacks: what providers are asked to cut, and when the app warns.
    #[must_use]
    pub const fn with_stack_limit(mut self, limit: ReviewStackLimit) -> Self {
        self.stack_limit = limit;
        self
    }

    /// Register a built-in review-source provider (called in-process). Built-in providers win
    /// over plugin-backed ones with the same id.
    #[must_use]
    pub fn with_source(self, provider: Arc<dyn ReviewSourceProvider>) -> Self {
        lock(&self.providers).add_source(provider);
        self
    }

    /// Register a built-in discovery contributor (called in-process).
    #[must_use]
    pub fn with_contributor(self, contributor: Arc<dyn DiscoveryContributor>) -> Self {
        lock(&self.providers).add_contributor(contributor);
        self
    }

    /// The providers and contributors to ask now: the built-in ones, plus those a running
    /// Bcode's plugins offer once they are found. Finding them starts on the first call, in the
    /// background, so nothing built in waits for Bcode (or starts it) and the standalone app works
    /// without it.
    #[must_use]
    pub fn providers(&self) -> ProviderRegistry {
        if !self
            .plugins_sought
            .swap(true, std::sync::atomic::Ordering::AcqRel)
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            let services = self.clone();
            handle.spawn(async move { services.add_plugin_providers().await });
        }
        lock(&self.providers).clone()
    }

    /// Add the providers and contributors a running Bcode's plugins offer that are not built in.
    /// Their calls block on the host from the blocking threads provider work runs on.
    async fn add_plugin_providers(&self) {
        let Ok((sources, contributors)) = plugin_manifests(&self.transport()).await else {
            return;
        };
        let handle = tokio::runtime::Handle::current();
        let call = |plugin_id: String, interface: &'static str| -> JsonCall {
            let (host, handle) = (Arc::clone(&self.host), handle.clone());
            Box::new(move |operation, payload| {
                handle
                    .block_on(host.invoke_plugin(
                        plugin_id.clone(),
                        interface.to_owned(),
                        operation.to_owned(),
                        payload,
                    ))
                    .map_err(ProviderCallError::from)
            })
        };
        let mut registry = lock(&self.providers);
        for (plugin_id, manifest) in sources {
            if registry.source(&manifest.id).is_none() {
                let invoke = call(plugin_id, models::REVIEW_SOURCE_INTERFACE_ID);
                registry.add_source(Arc::new(JsonReviewSource::new(manifest, invoke)));
            }
        }
        for (plugin_id, manifest) in contributors {
            let invoke = call(plugin_id, models::REVIEW_DISCOVERY_INTERFACE_ID);
            registry.add_contributor(Arc::new(JsonDiscovery::new(manifest, invoke)));
        }
    }

    /// Run blocking provider work: on the blocking pool, or on the calling task in scenario tests.
    ///
    /// # Errors
    ///
    /// * `work`'s error, or a failure when the blocking task did not finish.
    pub async fn blocking<T, F>(&self, work: F) -> Result<T, ProviderCallError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, ProviderCallError> + Send + 'static,
    {
        match self.execution {
            Execution::Pool => tokio::task::spawn_blocking(work).await.map_err(|error| {
                ProviderCallError::new(
                    models::REVIEW_SOURCE_ERROR_FAILED,
                    format!("provider work did not finish: {error}"),
                )
            })?,
            #[cfg(test)]
            Execution::Inline => work(),
        }
    }

    /// The configured stack layer limit.
    #[must_use]
    pub const fn stack_limit(&self) -> ReviewStackLimit {
        self.stack_limit
    }

    /// Services that answer on the calling task, so scenario tests observe completions in a
    /// fixed order.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn inline(
        store: ReviewStore,
        host: Arc<dyn ReviewHost>,
        guard: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Self {
        Self {
            execution: Execution::Inline,
            _guard: Some(guard),
            // Scenario tests register their providers; nothing is looked for in the background.
            plugins_sought: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            ..Self::new(store, host)
        }
    }

    /// The stored-state configuration.
    #[must_use]
    pub const fn store(&self) -> &ReviewStore {
        &self.store
    }

    /// The host whose plugin services serve providers, publishers, and importers.
    #[must_use]
    pub fn host(&self) -> &dyn ReviewHost {
        self.host.as_ref()
    }

    /// Provider transport over the host's plugin services.
    #[must_use]
    pub fn transport(&self) -> HostTransport<'_> {
        HostTransport(self.host.as_ref())
    }

    /// Answer `request`.
    ///
    /// # Errors
    ///
    /// * The data layer's failure, with its stable error code.
    pub async fn call<O: Operation>(&self, request: O) -> ServiceResult<O::Response> {
        #[cfg(test)]
        self.probe.observe(O::NAME)?;
        match self.execution {
            Execution::Pool => {
                let (data, store) = (Arc::clone(&self.data), self.store.clone());
                tokio::task::spawn_blocking(move || request.run(&data, &store))
                    .await
                    .map_err(|error| ProviderCallError {
                        code: models::REVIEW_SOURCE_ERROR_FAILED.to_owned(),
                        message: format!("{} did not finish: {error}", O::NAME),
                    })?
            }
            #[cfg(test)]
            Execution::Inline => request.run(&self.data, &self.store),
        }
    }

    /// Answer `request` on the calling thread, outside the call log and injected failures.
    #[cfg(test)]
    pub(crate) fn answer<O: Operation>(&self, request: O) -> ServiceResult<O::Response> {
        request.run(&self.data, &self.store)
    }

    /// Make every later call of `operation` fail (or succeed again when `fail` is false).
    #[cfg(test)]
    pub(crate) fn set_failing(&self, operation: &str, fail: bool) {
        self.probe.set_failing(operation, fail);
    }
}

/// Lock `mutex`, recovering the guard if a holder panicked (the registry stays usable).
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod probe {
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    use bcode_code_review::provider::ProviderCallError;

    /// Fails chosen operations.
    #[derive(Debug, Default)]
    pub struct Probe {
        failing: Mutex<BTreeSet<String>>,
    }

    impl Probe {
        pub fn observe(&self, name: &str) -> Result<(), ProviderCallError> {
            if self.failing.lock().expect("failing lock").contains(name) {
                return Err(ProviderCallError {
                    code: "test_failure".to_owned(),
                    message: format!("{name} failed (test)"),
                });
            }
            Ok(())
        }

        pub fn set_failing(&self, name: &str, fail: bool) {
            let mut failing = self.failing.lock().expect("failing lock");
            if fail {
                failing.insert(name.to_owned());
            } else {
                failing.remove(name);
            }
        }
    }
}
