//! Behavior of secret-safe scoped evidence and trusted catalog publication.
use bcode_model_catalog::{
    EvidenceCapability as C, EvidenceDocument, EvidenceOutcome as O, EvidenceResolution as R,
    EvidenceScope, EvidenceStore, ModelCatalog, ModelSupportEvidence, load_embedded_catalog,
    resolve_evidence,
    support_patch::{SupportTrustPolicy, apply_support_patch, prepare_support_patch},
};
use bcode_model_catalog_models::ModelSupportTarget;
use std::collections::BTreeSet;

fn scope(account: char) -> EvidenceScope {
    EvidenceScope {
        target: ModelSupportTarget::new(
            "openai",
            "chatgpt_subscription",
            "chatgpt_codex",
            Some("bcode"),
        ),
        endpoint_fingerprint: "e".repeat(64),
        account_fingerprint: account.to_string().repeat(64),
    }
}
fn fact(account: char, outcome: O, start: u64, completed: u64) -> ModelSupportEvidence {
    ModelSupportEvidence {
        scope: scope(account),
        model_id: "test-working-model".into(),
        outcome,
        capabilities: BTreeSet::from([C::TextGeneration]),
        attempt_started_at_ms: start,
        attempt_ordinal: start,
        observed_at_ms: completed,
        expires_at_ms: completed + 1000,
    }
}
fn document(observations: Vec<ModelSupportEvidence>) -> EvidenceDocument {
    EvidenceDocument {
        schema_version: 1,
        observations,
    }
}
#[test]
fn atomic_roundtrip_does_not_resurrect_delayed_or_expired_success() {
    let root = tempfile::tempdir().unwrap();
    let store = EvidenceStore::new(root.path());
    assert_eq!(
        store
            .resolve(&scope('a'), "test-working-model", C::TextGeneration, 20)
            .unwrap(),
        R::Unverified
    );
    store.record(&fact('a', O::Success, 10, 100), 100).unwrap();
    store
        .record(&fact('a', O::ProviderSurfaceUnsupported, 20, 90), 100)
        .unwrap();
    // Late completion of the earlier admitted request cannot restore support.
    store.record(&fact('a', O::Success, 10, 200), 200).unwrap();
    assert_eq!(
        store
            .resolve(&scope('a'), "test-working-model", C::TextGeneration, 200)
            .unwrap(),
        R::Unsupported
    );
    assert_eq!(
        store
            .resolve(&scope('a'), "test-working-model", C::TextGeneration, 1200)
            .unwrap(),
        R::Unverified
    );
    assert_eq!(store.snapshot(1200).unwrap().observations.len(), 1);
}
#[test]
fn account_denial_and_missing_capabilities_never_globalize() {
    let doc = document(vec![fact('a', O::AccountDenied, 10, 20)]);
    assert_eq!(
        resolve_evidence(
            &doc,
            &scope('a'),
            "test-working-model",
            C::TextGeneration,
            30
        ),
        R::AccountDenied
    );
    assert_eq!(
        resolve_evidence(
            &doc,
            &scope('b'),
            "test-working-model",
            C::TextGeneration,
            30
        ),
        R::Unverified
    );
    let doc = document(vec![fact('a', O::Success, 10, 20)]);
    assert_eq!(
        resolve_evidence(&doc, &scope('a'), "test-working-model", C::Tools, 30),
        R::Unverified
    );
    let mut other = scope('a');
    other.endpoint_fingerprint = "f".repeat(64);
    assert_eq!(
        resolve_evidence(&doc, &other, "test-working-model", C::TextGeneration, 30),
        R::Unverified
    );
}
#[test]
fn unknown_or_secret_bearing_state_is_preserved() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("model-support-evidence");
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("evidence-v1.json");
    let bytes = b"{\"schema_version\":99,\"observations\":[]}";
    std::fs::write(&path, bytes).unwrap();
    let store = EvidenceStore::new(root.path());
    assert!(store.record(&fact('a', O::Success, 10, 20), 30).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let mut unsafe_fact = fact('a', O::Success, 10, 20);
    unsafe_fact.scope.endpoint_fingerprint =
        "https://user:password@example.com/?token=secret".into();
    assert!(store.record(&unsafe_fact, 30).is_err());
}
#[test]
fn union_surfaces_working_missing_models_without_inventing_capabilities() {
    let catalog = ModelCatalog::new(load_embedded_catalog().unwrap());
    let (models, availability) = catalog
        .merge_provider_models_with_evidence(
            "openai",
            Vec::new(),
            false,
            &scope('a'),
            &document(vec![fact('a', O::Success, 10, 20)]),
            30,
        )
        .unwrap();
    assert_eq!(models.len(), 1);
    assert!(models[0].capabilities.is_empty());
    assert_eq!(
        availability["test-working-model"].status,
        bcode_model::ModelAvailabilityStatus::Verified
    );
}
#[test]
fn trusted_patch_is_executable_scoped_and_revision_guarded() {
    let mut catalog = load_embedded_catalog().unwrap();
    let policy = SupportTrustPolicy {
        trusted_scopes: BTreeSet::from([scope('a'), scope('b')]),
        removal_account_quorum: 2,
    };
    let patch = prepare_support_patch(
        &catalog,
        document(vec![fact('a', O::Success, 10, 20)]),
        &policy,
        30,
    )
    .unwrap();
    apply_support_patch(&mut catalog, &patch, &policy, 30, "test-revision-1").unwrap();
    let entry = &mut catalog
        .providers
        .get_mut("openai")
        .unwrap()
        .models
        .get_mut("test-working-model")
        .unwrap();
    let other = ModelSupportTarget::new("openai", "api_key", "responses_api", Some("bcode"));
    entry.supported_by.insert(other.clone());
    let patch = prepare_support_patch(
        &catalog,
        document(vec![
            fact('a', O::ProviderSurfaceUnsupported, 30, 40),
            fact('b', O::ProviderSurfaceUnsupported, 31, 41),
        ]),
        &policy,
        50,
    )
    .unwrap();
    apply_support_patch(&mut catalog, &patch, &policy, 50, "test-revision-2").unwrap();
    let entry = &catalog.providers["openai"].models["test-working-model"];
    assert!(entry.supported_by.contains(&other));
    assert!(entry.unsupported_by.contains(&scope('a').target));
    let before = catalog.clone();
    assert!(apply_support_patch(&mut catalog, &patch, &policy, 50, "test-revision-3").is_err());
    assert_eq!(catalog, before);
}
#[test]
fn conflicting_admissions_and_writer_contention_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let store = EvidenceStore::new(root.path());
    store.record(&fact('a', O::Success, 10, 20), 30).unwrap();
    assert!(
        store
            .record(&fact('a', O::AccountDenied, 10, 21), 30)
            .is_err()
    );
    let directory = root.path().join("model-support-evidence");
    std::fs::write(directory.join("writer.lock"), b"").unwrap();
    assert!(matches!(
        store.record(&fact('a', O::Success, 11, 22), 30),
        Err(bcode_model_catalog::EvidenceError::Busy)
    ));
    assert_eq!(
        store
            .resolve(&scope('a'), "test-working-model", C::TextGeneration, 30)
            .unwrap(),
        R::Supported
    );
}

#[test]
fn later_success_reconciles_surface_rejection_but_not_other_endpoints() {
    let doc = document(vec![
        fact('a', O::ProviderSurfaceUnsupported, 10, 20),
        fact('b', O::Success, 11, 21),
    ]);
    assert_eq!(
        resolve_evidence(
            &doc,
            &scope('b'),
            "test-working-model",
            C::TextGeneration,
            30
        ),
        R::Supported
    );
    assert_eq!(
        resolve_evidence(
            &doc,
            &scope('a'),
            "test-working-model",
            C::TextGeneration,
            30
        ),
        R::Unsupported
    );
    let mut other = scope('c');
    other.endpoint_fingerprint = "f".repeat(64);
    assert_eq!(
        resolve_evidence(&doc, &other, "test-working-model", C::TextGeneration, 30),
        R::Unverified
    );
}

#[tokio::test]
async fn resolver_user_view_unions_missing_working_models_and_defaults() {
    let resolver = bcode_model_catalog::ModelCatalogResolver::embedded();
    let active = scope('a');
    let observations = document(vec![fact('a', O::Success, 10, 20)]);
    let list = bcode_model::ModelList {
        models: Vec::new(),
        catalog: Default::default(),
    };
    let result = resolver
        .resolve_view_with_evidence(
            list,
            None,
            None,
            bcode_model_catalog::ModelListView::UserVisible,
            bcode_model_catalog::ModelEvidenceContext {
                scope: &active,
                document: &observations,
                now_ms: 30,
            },
        )
        .await
        .unwrap();
    assert_eq!(result.models.len(), 1);
    assert!(result.models[0].is_default);
    assert_eq!(
        result.catalog.availability["test-working-model"].status,
        bcode_model::ModelAvailabilityStatus::Verified
    );
}

#[test]
fn trust_and_quorum_are_validated_and_denials_produce_no_removals() {
    let catalog = load_embedded_catalog().unwrap();
    let policy = SupportTrustPolicy {
        trusted_scopes: BTreeSet::from([scope('a'), scope('b')]),
        removal_account_quorum: 2,
    };
    let patch = prepare_support_patch(
        &catalog,
        document(vec![
            fact('a', O::AccountDenied, 10, 20),
            fact('b', O::AccountDenied, 11, 21),
        ]),
        &policy,
        30,
    )
    .unwrap();
    let mut target = catalog.clone();
    assert!(
        apply_support_patch(&mut target, &patch, &policy, 30, "denials")
            .unwrap()
            .is_empty()
    );
    assert!(
        prepare_support_patch(
            &catalog,
            document(vec![fact('c', O::Success, 10, 20)]),
            &policy,
            30
        )
        .is_err()
    );
}
