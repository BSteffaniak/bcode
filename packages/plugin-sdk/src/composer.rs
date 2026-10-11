//! Lightweight local composer activation adapters.
//!
//! Activation must be bounded and perform no filesystem/network IO. Search runs
//! through `bcode.composer/v1` off the input/render thread. Each enabled plugin
//! supplies its own registry through its existing native UI factory. Registry
//! replacement drops local adapters; callers cancel searches and discard snapshots.

pub use bcode_plugin_models::composer::*;
use std::collections::BTreeMap;
use std::fmt;

/// Decode and validate a bounded service reply before exposing its candidates.
///
/// Hosts must also check that the originating document is still current and the
/// owning plugin/registry has not been disabled or replaced.
///
/// # Errors
/// Returns `InvalidPayload` for oversized or malformed JSON and contract-invalid
/// candidates, or `Stale` for request/document mismatch. Payloads are not logged.
pub fn decode_composer_snapshot(
    payload: &[u8],
    request: &ComposerSearchRequest,
) -> Result<ComposerSnapshot, ComposerValidationError> {
    if payload.len() > MAX_COMPOSER_RESPONSE_BYTES {
        return Err(ComposerValidationError::InvalidPayload);
    }
    let snapshot: ComposerSnapshot =
        serde_json::from_slice(payload).map_err(|_| ComposerValidationError::InvalidPayload)?;
    snapshot.validate_for(request)?;
    Ok(snapshot)
}

/// Plugin-owned grammar adapter. Hosts never inspect trigger syntax.
pub trait PluginComposerContributor: Send + Sync + 'static {
    /// Exact activation kind advertised by the manifest.
    fn kind(&self) -> &'static str;

    /// Inspect one document revision without IO or content attachment.
    /// Returning `None` leaves ordinary input untouched.
    fn activate(&self, document: &ComposerDocument) -> Option<ComposerActivation>;
}

/// Normalized local-adapter failures; no text payload appears in diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginComposerRegistryError {
    /// The requested kind has no local adapter.
    UnsupportedKind,
    /// Registration advertised an invalid kind.
    InvalidKind,
    /// Conflicting registrations fail closed until registry replacement.
    ConflictingContributors,
    /// Document or plugin activation failed contract validation.
    InvalidActivation,
}

impl fmt::Display for PluginComposerRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnsupportedKind => "unsupported composer activation kind",
            Self::InvalidKind => "invalid composer activation kind",
            Self::ConflictingContributors => "conflicting composer contributors",
            Self::InvalidActivation => "invalid composer activation",
        })
    }
}
impl std::error::Error for PluginComposerRegistryError {}

/// Per-enabled-plugin local activation registry; no global plugin lifetime state.
#[derive(Default)]
pub struct PluginComposerRegistry {
    contributors: BTreeMap<String, Option<Box<dyn PluginComposerContributor>>>,
}

impl fmt::Debug for PluginComposerRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginComposerRegistry")
            .field("kind_count", &self.contributors.len())
            .finish()
    }
}

impl PluginComposerRegistry {
    /// Register a local grammar adapter. Duplicate kinds become unavailable.
    ///
    /// # Errors
    /// Returns `InvalidKind` for invalid identifiers, or `ConflictingContributors`
    /// for a duplicate (including subsequent registrations of that kind).
    pub fn register(
        &mut self,
        contributor: Box<dyn PluginComposerContributor>,
    ) -> Result<(), PluginComposerRegistryError> {
        let kind = contributor.kind();
        let declaration = ComposerContribution {
            id: kind.to_owned(),
            activation_kind: kind.to_owned(),
            service_interface_id: COMPOSER_INTERFACE_ID.to_owned(),
            priority: 0,
        };
        if !declaration.is_supported() {
            return Err(PluginComposerRegistryError::InvalidKind);
        }
        match self.contributors.entry(kind.to_owned()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(Some(contributor));
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.insert(None);
                Err(PluginComposerRegistryError::ConflictingContributors)
            }
        }
    }

    /// Whether one unambiguous local adapter is available.
    #[must_use]
    pub fn supports(&self, kind: &str) -> bool {
        self.contributors.get(kind).is_some_and(Option::is_some)
    }

    /// Run local activation and validate its UTF-8 range and payload bounds.
    ///
    /// # Errors
    /// Returns `UnsupportedKind`, `ConflictingContributors` or `InvalidActivation`.
    /// Errors leave ordinary text entry available; they never trigger a search.
    pub fn activate(
        &self,
        kind: &str,
        document: &ComposerDocument,
    ) -> Result<Option<ComposerActivation>, PluginComposerRegistryError> {
        document
            .validate()
            .map_err(|_| PluginComposerRegistryError::InvalidActivation)?;
        if !document.allows_activation() {
            return Ok(None);
        }
        let contributor = self
            .contributors
            .get(kind)
            .ok_or(PluginComposerRegistryError::UnsupportedKind)?
            .as_ref()
            .ok_or(PluginComposerRegistryError::ConflictingContributors)?;
        let activation = contributor.activate(document);
        if let Some(activation) = &activation {
            activation
                .validate(document)
                .map_err(|_| PluginComposerRegistryError::InvalidActivation)?;
        }
        Ok(activation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct FakeContributor(Arc<AtomicUsize>);
    impl Drop for FakeContributor {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl PluginComposerContributor for FakeContributor {
        fn kind(&self) -> &'static str {
            "example.topic"
        }
        fn activate(&self, document: &ComposerDocument) -> Option<ComposerActivation> {
            document.text.starts_with('#').then(|| ComposerActivation {
                range: ComposerTextRange {
                    start: 0,
                    end: document.text.len(),
                },
                query: document.text[1..].to_owned(),
            })
        }
    }

    #[test]
    fn non_file_grammar_and_registry_lifecycle() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut registry = PluginComposerRegistry::default();
        registry
            .register(Box::new(FakeContributor(drops.clone())))
            .unwrap();
        let mut document = ComposerDocument {
            document_id: "draft".into(),
            revision: 1,
            text: "#rust".into(),
            cursor: 5,
            selection: None,
        };
        assert_eq!(
            registry
                .activate("example.topic", &document)
                .unwrap()
                .unwrap()
                .query,
            "rust"
        );
        document.text = "ordinary".into();
        document.cursor = 8;
        assert!(
            registry
                .activate("example.topic", &document)
                .unwrap()
                .is_none()
        );
        drop(registry);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let fresh = PluginComposerRegistry::default();
        assert!(!fresh.supports("example.topic"));
        assert_eq!(
            fresh.activate("example.topic", &document),
            Err(PluginComposerRegistryError::UnsupportedKind)
        );
    }

    #[test]
    fn duplicates_fail_closed_and_drop_adapters() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut registry = PluginComposerRegistry::default();
        registry
            .register(Box::new(FakeContributor(drops.clone())))
            .unwrap();
        assert_eq!(
            registry.register(Box::new(FakeContributor(drops.clone()))),
            Err(PluginComposerRegistryError::ConflictingContributors)
        );
        assert!(!registry.supports("example.topic"));
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn invalid_local_activation_cannot_start_search() {
        struct Invalid;
        impl PluginComposerContributor for Invalid {
            fn kind(&self) -> &'static str {
                "example.invalid"
            }
            fn activate(&self, _: &ComposerDocument) -> Option<ComposerActivation> {
                Some(ComposerActivation {
                    range: ComposerTextRange { start: 1, end: 2 },
                    query: String::new(),
                })
            }
        }
        let mut registry = PluginComposerRegistry::default();
        registry.register(Box::new(Invalid)).unwrap();
        let document = ComposerDocument {
            document_id: "draft".into(),
            revision: 1,
            text: "é".into(),
            cursor: 2,
            selection: None,
        };
        assert_eq!(
            registry.activate("example.invalid", &document),
            Err(PluginComposerRegistryError::InvalidActivation)
        );
    }

    fn search_request() -> ComposerSearchRequest {
        ComposerSearchRequest {
            contribution_id: "example.topic".into(),
            request_id: "request".into(),
            document: ComposerDocument {
                document_id: "draft".into(),
                revision: 1,
                text: "#rust".into(),
                cursor: 5,
                selection: None,
            },
            activation: ComposerActivation {
                range: ComposerTextRange { start: 0, end: 5 },
                query: "rust".into(),
            },
            max_results: 8,
        }
    }

    fn ready_snapshot(request: &ComposerSearchRequest) -> ComposerSnapshot {
        ComposerSnapshot {
            request_id: request.request_id.clone(),
            document_id: request.document.document_id.clone(),
            revision: request.document.revision,
            candidates: Vec::new(),
            incomplete: false,
            state: ComposerSnapshotState::Ready,
        }
    }

    #[test]
    fn request_serde_and_contribution_dispatch_preserve_legacy_uniqueness() {
        let request = search_request();
        let json = serde_json::to_vec(&request).unwrap();
        assert_eq!(
            serde_json::from_slice::<ComposerSearchRequest>(&json).unwrap(),
            request
        );
        let mut second_request = request.clone();
        second_request.contribution_id = "example.symbol".into();
        let second_json = serde_json::to_vec(&second_request).unwrap();
        assert_ne!(json, second_json);
        assert_eq!(
            serde_json::from_slice::<ComposerSearchRequest>(&second_json).unwrap(),
            second_request
        );
        let mut legacy = serde_json::to_value(&request).unwrap();
        legacy.as_object_mut().unwrap().remove("contribution_id");
        let legacy: ComposerSearchRequest = serde_json::from_value(legacy).unwrap();
        assert!(legacy.contribution_id.is_empty());
        legacy.validate().unwrap();
        let declarations: Vec<_> = ["example.topic", "example.symbol"]
            .into_iter()
            .map(|id| ComposerContribution {
                id: id.into(),
                activation_kind: id.into(),
                service_interface_id: COMPOSER_INTERFACE_ID.into(),
                priority: 0,
            })
            .collect();
        assert_eq!(
            legacy.validate_for_contributions(&declarations),
            Err(ComposerValidationError::InvalidPayload)
        );
        assert_eq!(
            legacy
                .validate_for_contributions(&declarations[..1])
                .unwrap()
                .id,
            "example.topic"
        );
        assert_eq!(
            second_request
                .validate_for_contributions(&declarations)
                .unwrap()
                .id,
            "example.symbol"
        );
    }

    #[test]
    fn snapshot_decode_is_bounded_and_revision_fenced() {
        let request = search_request();
        let snapshot = ready_snapshot(&request);
        let reply = serde_json::to_vec(&snapshot).unwrap();
        assert_eq!(
            decode_composer_snapshot(&reply, &request).unwrap(),
            snapshot
        );
        assert_eq!(
            decode_composer_snapshot(b"not json", &request),
            Err(ComposerValidationError::InvalidPayload)
        );
        assert_eq!(
            decode_composer_snapshot(&vec![b' '; MAX_COMPOSER_RESPONSE_BYTES + 1], &request),
            Err(ComposerValidationError::InvalidPayload)
        );
        let mut stale_request = request;
        stale_request.document.revision += 1;
        assert_eq!(
            decode_composer_snapshot(&reply, &stale_request),
            Err(ComposerValidationError::Stale)
        );
    }

    #[test]
    fn legacy_document_snapshot_candidate_and_discovery_defaults() {
        let request = search_request();
        let snapshot = ready_snapshot(&request);
        let mut legacy_document = serde_json::to_value(&request.document).unwrap();
        legacy_document.as_object_mut().unwrap().remove("selection");
        assert!(
            serde_json::from_value::<ComposerDocument>(legacy_document)
                .unwrap()
                .selection
                .is_none()
        );
        let mut legacy_snapshot = serde_json::to_value(&snapshot).unwrap();
        legacy_snapshot.as_object_mut().unwrap().remove("state");
        assert_eq!(
            serde_json::from_value::<ComposerSnapshot>(legacy_snapshot)
                .unwrap()
                .state,
            ComposerSnapshotState::Ready
        );
        let candidate_json = serde_json::json!({ "id": "example", "label": "Example", "edit": {
            "document_id": "draft", "revision": 1, "range": { "start": 0, "end": 5 },
            "expected_text": "#rust", "replacement": "Rust", "cursor": 4
        }});
        assert_eq!(
            serde_json::from_value::<ComposerCandidate>(candidate_json)
                .unwrap()
                .continuation,
            ComposerContinuation::Finish
        );
        let discovery: crate::PluginContributions = serde_json::from_str("{}").unwrap();
        assert!(discovery.composer_contributions.is_empty());
    }
}
