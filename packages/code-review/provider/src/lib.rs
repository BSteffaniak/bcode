#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! The interfaces code review uses to learn about pull requests and stacks.
//!
//! * [`ReviewSourceProvider`]: a forge (GitHub, …) answering exact questions about pull
//!   requests: lookups, stack resolution, fetching their commits. Authoritative.
//! * [`DiscoveryContributor`]: local evidence (git history, a stacking tool's metadata) about how
//!   branches relate, plus notes about them. Hints only.
//!
//! Built-in implementations are libraries the review links and calls in-process; the same
//! implementations are served to Bcode's plugins as JSON operations ([`serve_review_source`],
//! [`serve_discovery`]), and a plugin's provider can be called through the trait
//! ([`JsonReviewSource`], [`JsonDiscovery`]). Methods block (they run git and talk to the
//! network); callers run them on blocking threads, which is also how calls proceed in parallel.
//! See `docs/review-loading-architecture.md`.

use bcode_code_review_models::{
    OP_REVIEW_DISCOVERY_DISCOVER, OP_REVIEW_DISCOVERY_MANIFEST, OP_REVIEW_SOURCE_MANIFEST,
    OP_REVIEW_SOURCE_PULL_FETCH, OP_REVIEW_SOURCE_PULLS_FETCH, OP_REVIEW_SOURCE_PULLS_GET,
    OP_REVIEW_SOURCE_PULLS_LIST, OP_REVIEW_SOURCE_STACK_RESOLVE, REVIEW_SOURCE_ERROR_FAILED,
    ReviewDiscoveryManifest, ReviewDiscoveryRequest, ReviewDiscoveryResponse,
    ReviewPullFetchResult, ReviewSourceProviderManifest, ReviewSourcePullFetchRequest,
    ReviewSourcePullsFetchRequest, ReviewSourcePullsFetchResponse, ReviewSourcePullsGetRequest,
    ReviewSourcePullsGetResponse, ReviewSourcePullsListRequest, ReviewSourcePullsListResponse,
    ReviewSourceStackResolveRequest, ReviewSourceStackResolveResponse,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Error code: the operation is not one the interface defines.
pub const ERROR_UNSUPPORTED_OPERATION: &str = "unsupported_operation";
/// Error code: the request could not be read.
pub const ERROR_INVALID_REQUEST: &str = "invalid_request";

/// A failed provider or contributor call: a stable error code and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCallError {
    /// Stable error code (for review sources, the `REVIEW_SOURCE_ERROR_*` codes).
    pub code: String,
    /// Human-readable message.
    pub message: String,
}

impl ProviderCallError {
    /// An error with `code` and `message`.
    #[must_use]
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ProviderCallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProviderCallError {}

/// Result of a provider or contributor call.
pub type ProviderResult<T> = Result<T, ProviderCallError>;

/// A flag the caller of a provider sets when it no longer wants the answer.
///
/// Providers are called synchronously; the caller makes its flag current for the call with
/// [`with_cancel`], and a provider that makes several requests checks [`cancelled`] between them
/// and stops with [`REVIEW_SOURCE_ERROR_CANCELLED`](bcode_code_review_models::REVIEW_SOURCE_ERROR_CANCELLED).
/// The trait methods keep their signatures, so plugin-backed providers (whose calls run in their
/// plugin) are not reached: their hosts would have to forward cancellation.
#[derive(Debug, Clone, Default)]
pub struct Cancel(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Cancel {
    /// Stop wanting the answer.
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether [`Self::cancel`] was called.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

thread_local! {
    static CURRENT: std::cell::RefCell<Option<Cancel>> = const { std::cell::RefCell::new(None) };
}

/// Run `call` with `cancel` as the current thread's cancellation (see [`cancelled`]); the one
/// current before is restored after.
pub fn with_cancel<T>(cancel: &Cancel, call: impl FnOnce() -> T) -> T {
    let previous = CURRENT.with(|current| current.replace(Some(cancel.clone())));
    // Restored even when `call` panics.
    let _restore = Restore(previous);
    call()
}

/// Makes the cancellation it holds current again when dropped (see [`with_cancel`]).
struct Restore(Option<Cancel>);

impl Drop for Restore {
    fn drop(&mut self) {
        let previous = self.0.take();
        CURRENT.with(|current| *current.borrow_mut() = previous);
    }
}

/// Whether the current thread's caller stopped wanting the answer (see [`with_cancel`]).
#[must_use]
pub fn cancelled() -> bool {
    CURRENT.with(|current| current.borrow().as_ref().is_some_and(Cancel::is_cancelled))
}

/// A forge's answers about pull requests (`bcode.review_source/v1`). Every method blocks.
pub trait ReviewSourceProvider: Send + Sync {
    /// The provider's id, label, and capabilities.
    fn manifest(&self) -> ReviewSourceProviderManifest;

    /// Recent open pull requests, or those a query names.
    ///
    /// # Errors
    /// The provider's failure (for example `unauthenticated`).
    fn list_pulls(
        &self,
        request: &ReviewSourcePullsListRequest,
    ) -> ProviderResult<ReviewSourcePullsListResponse>;

    /// Several pull requests by number, as they are now: one round trip however many, which
    /// is what confirms a remembered stack.
    ///
    /// # Errors
    /// The provider's failure; [`ERROR_UNSUPPORTED_OPERATION`] when the provider does not
    /// support it (its manifest says so), which is the default.
    fn get_pulls(
        &self,
        request: &ReviewSourcePullsGetRequest,
    ) -> ProviderResult<ReviewSourcePullsGetResponse> {
        let _ = request;
        Err(ProviderCallError::new(
            ERROR_UNSUPPORTED_OPERATION,
            format!(
                "{} does not get pull requests by number",
                self.manifest().label
            ),
        ))
    }

    /// The stack containing a pull request.
    ///
    /// # Errors
    /// The provider's failure (for example `pull_not_found`).
    fn resolve_stack(
        &self,
        request: &ReviewSourceStackResolveRequest,
    ) -> ProviderResult<ReviewSourceStackResolveResponse>;

    /// Make one pull request's commits local.
    ///
    /// # Errors
    /// The provider's failure (for example `fetch_failed`).
    fn fetch_pull(
        &self,
        request: &ReviewSourcePullFetchRequest,
    ) -> ProviderResult<ReviewPullFetchResult>;

    /// Make several pull requests' commits local at once.
    ///
    /// # Errors
    /// The provider's failure; [`ERROR_UNSUPPORTED_OPERATION`] when the provider does not batch
    /// (its manifest says so).
    fn fetch_pulls(
        &self,
        request: &ReviewSourcePullsFetchRequest,
    ) -> ProviderResult<ReviewSourcePullsFetchResponse>;
}

/// Local evidence about branches (`bcode.review_discovery/v1`). Every method blocks and must
/// stay local and fast.
pub trait DiscoveryContributor: Send + Sync {
    /// The contributor's id and label.
    fn manifest(&self) -> ReviewDiscoveryManifest;

    /// Links, related branches, and annotations around the requested branches.
    ///
    /// # Errors
    /// The contributor's failure; callers treat it as a warning.
    fn discover(&self, request: &ReviewDiscoveryRequest)
    -> ProviderResult<ReviewDiscoveryResponse>;
}

fn decode<T: DeserializeOwned>(payload: &[u8]) -> ProviderResult<T> {
    serde_json::from_slice(payload)
        .map_err(|error| ProviderCallError::new(ERROR_INVALID_REQUEST, error.to_string()))
}

fn encode<T: Serialize>(value: &T) -> ProviderResult<Vec<u8>> {
    serde_json::to_vec(value)
        .map_err(|error| ProviderCallError::new(REVIEW_SOURCE_ERROR_FAILED, error.to_string()))
}

/// Answer one `bcode.review_source/v1` operation with `provider` (JSON in, JSON out).
///
/// # Errors
/// [`ERROR_INVALID_REQUEST`] for an unreadable request, [`ERROR_UNSUPPORTED_OPERATION`] for an
/// unknown operation, else the provider's failure.
pub fn serve_review_source(
    provider: &dyn ReviewSourceProvider,
    operation: &str,
    payload: &[u8],
) -> ProviderResult<Vec<u8>> {
    match operation {
        OP_REVIEW_SOURCE_MANIFEST => encode(&provider.manifest()),
        OP_REVIEW_SOURCE_PULLS_LIST => encode(&provider.list_pulls(&decode(payload)?)?),
        OP_REVIEW_SOURCE_PULLS_GET => encode(&provider.get_pulls(&decode(payload)?)?),
        OP_REVIEW_SOURCE_STACK_RESOLVE => encode(&provider.resolve_stack(&decode(payload)?)?),
        OP_REVIEW_SOURCE_PULL_FETCH => encode(&provider.fetch_pull(&decode(payload)?)?),
        OP_REVIEW_SOURCE_PULLS_FETCH => encode(&provider.fetch_pulls(&decode(payload)?)?),
        _ => Err(ProviderCallError::new(
            ERROR_UNSUPPORTED_OPERATION,
            format!("unsupported review-source operation {operation}"),
        )),
    }
}

/// Answer one `bcode.review_discovery/v1` operation with `contributor` (JSON in, JSON out).
///
/// # Errors
/// As [`serve_review_source`].
pub fn serve_discovery(
    contributor: &dyn DiscoveryContributor,
    operation: &str,
    payload: &[u8],
) -> ProviderResult<Vec<u8>> {
    match operation {
        OP_REVIEW_DISCOVERY_MANIFEST => encode(&contributor.manifest()),
        OP_REVIEW_DISCOVERY_DISCOVER => encode(&contributor.discover(&decode(payload)?)?),
        _ => Err(ProviderCallError::new(
            ERROR_UNSUPPORTED_OPERATION,
            format!("unsupported review discovery operation {operation}"),
        )),
    }
}

/// A blocking JSON call: `(operation, payload) → response payload`.
pub type JsonCall = Box<dyn Fn(&str, Vec<u8>) -> ProviderResult<Vec<u8>> + Send + Sync>;

fn call<Req: Serialize, Resp: DeserializeOwned>(
    invoke: &JsonCall,
    operation: &str,
    request: &Req,
) -> ProviderResult<Resp> {
    let response = invoke(operation, encode(request)?)?;
    serde_json::from_slice(&response).map_err(|error| {
        ProviderCallError::new(
            REVIEW_SOURCE_ERROR_FAILED,
            format!("unexpected {operation} response: {error}"),
        )
    })
}

/// A review-source provider reached through JSON operations (for example a Bcode plugin's).
pub struct JsonReviewSource {
    manifest: ReviewSourceProviderManifest,
    invoke: JsonCall,
}

impl JsonReviewSource {
    /// The provider `manifest` describes, answering through `invoke`.
    #[must_use]
    pub fn new(manifest: ReviewSourceProviderManifest, invoke: JsonCall) -> Self {
        Self { manifest, invoke }
    }
}

impl ReviewSourceProvider for JsonReviewSource {
    fn manifest(&self) -> ReviewSourceProviderManifest {
        self.manifest.clone()
    }

    fn list_pulls(
        &self,
        request: &ReviewSourcePullsListRequest,
    ) -> ProviderResult<ReviewSourcePullsListResponse> {
        call(&self.invoke, OP_REVIEW_SOURCE_PULLS_LIST, request)
    }

    fn get_pulls(
        &self,
        request: &ReviewSourcePullsGetRequest,
    ) -> ProviderResult<ReviewSourcePullsGetResponse> {
        call(&self.invoke, OP_REVIEW_SOURCE_PULLS_GET, request)
    }

    fn resolve_stack(
        &self,
        request: &ReviewSourceStackResolveRequest,
    ) -> ProviderResult<ReviewSourceStackResolveResponse> {
        call(&self.invoke, OP_REVIEW_SOURCE_STACK_RESOLVE, request)
    }

    fn fetch_pull(
        &self,
        request: &ReviewSourcePullFetchRequest,
    ) -> ProviderResult<ReviewPullFetchResult> {
        call(&self.invoke, OP_REVIEW_SOURCE_PULL_FETCH, request)
    }

    fn fetch_pulls(
        &self,
        request: &ReviewSourcePullsFetchRequest,
    ) -> ProviderResult<ReviewSourcePullsFetchResponse> {
        call(&self.invoke, OP_REVIEW_SOURCE_PULLS_FETCH, request)
    }
}

/// A discovery contributor reached through JSON operations (for example a Bcode plugin's).
pub struct JsonDiscovery {
    manifest: ReviewDiscoveryManifest,
    invoke: JsonCall,
}

impl JsonDiscovery {
    /// The contributor `manifest` describes, answering through `invoke`.
    #[must_use]
    pub fn new(manifest: ReviewDiscoveryManifest, invoke: JsonCall) -> Self {
        Self { manifest, invoke }
    }
}

impl DiscoveryContributor for JsonDiscovery {
    fn manifest(&self) -> ReviewDiscoveryManifest {
        self.manifest.clone()
    }

    fn discover(
        &self,
        request: &ReviewDiscoveryRequest,
    ) -> ProviderResult<ReviewDiscoveryResponse> {
        call(&self.invoke, OP_REVIEW_DISCOVERY_DISCOVER, request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::ReviewSourceCapabilities;
    use std::sync::Arc;

    struct Echo;

    impl DiscoveryContributor for Echo {
        fn manifest(&self) -> ReviewDiscoveryManifest {
            ReviewDiscoveryManifest {
                id: "echo".into(),
                label: "Echo".into(),
            }
        }

        fn discover(
            &self,
            request: &ReviewDiscoveryRequest,
        ) -> ProviderResult<ReviewDiscoveryResponse> {
            Ok(ReviewDiscoveryResponse {
                related: request.branches.clone(),
                ..ReviewDiscoveryResponse::default()
            })
        }
    }

    #[test]
    fn a_cancellation_is_current_only_within_its_call() {
        let cancel = Cancel::default();
        assert!(!cancelled());
        with_cancel(&cancel, || {
            assert!(!cancelled());
            cancel.cancel();
            assert!(cancelled());
            // A nested call with its own flag sees that one, and the outer one comes back.
            with_cancel(&Cancel::default(), || assert!(!cancelled()));
            assert!(cancelled());
        });
        assert!(!cancelled());
        // Other threads do not see it.
        with_cancel(&cancel, || {
            assert!(!std::thread::spawn(cancelled).join().expect("thread"));
        });
    }

    #[test]
    fn json_round_trips_reach_the_same_answers_as_direct_calls() {
        let served: Arc<dyn DiscoveryContributor> = Arc::new(Echo);
        let through_json = JsonDiscovery::new(
            served.manifest(),
            Box::new(move |operation, payload| {
                serve_discovery(served.as_ref(), operation, &payload)
            }),
        );
        let request = ReviewDiscoveryRequest {
            repo_path: "/repo".into(),
            branches: vec!["a".into()],
            trunk: None,
        };
        assert_eq!(through_json.discover(&request), Echo.discover(&request));
        assert_eq!(through_json.manifest().id, "echo");
        let unknown = serve_discovery(&Echo, "review_discovery.nope", b"{}").unwrap_err();
        assert_eq!(unknown.code, ERROR_UNSUPPORTED_OPERATION);
        let invalid = serve_discovery(&Echo, OP_REVIEW_DISCOVERY_DISCOVER, b"nope").unwrap_err();
        assert_eq!(invalid.code, ERROR_INVALID_REQUEST);
        // Review sources answer unknown operations the same way.
        let manifest = ReviewSourceProviderManifest {
            id: "x".into(),
            label: "X".into(),
            capabilities: ReviewSourceCapabilities::default(),
        };
        let source = JsonReviewSource::new(
            manifest,
            Box::new(|operation, _| {
                Err(ProviderCallError::new(
                    ERROR_UNSUPPORTED_OPERATION,
                    operation,
                ))
            }),
        );
        assert_eq!(source.manifest().id, "x");
    }
}
