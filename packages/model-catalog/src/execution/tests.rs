use super::*;
use crate::ModelCatalogResolver;

fn model() -> bcode_model::ModelInfo {
    bcode_model::ModelInfo {
        model_id: "observed-model".into(),
        display_name: "Observed model".into(),
        is_default: true,
        context_window: None,
        max_output_tokens: None,
        max_image_input_base64_bytes: None,
        capabilities: std::collections::BTreeSet::default(),
        feature_support: bcode_model::ModelFeatureSupport::default(),
        reasoning: None,
        cache: bcode_model::ModelCacheInfo::default(),
        metadata_source: None,
        pricing: None,
        api_surface: None,
        visibility: bcode_model::ModelVisibility::Visible,
    }
}

fn harness(readiness: ExecutionModelReadiness) -> ManagedHarnessExecutionCatalog {
    ManagedHarnessExecutionCatalog {
        instance_id: "configured-native".into(),
        plugin_id: "test.harness".into(),
        account_reference: Some("native-account".into()),
        billing_route: ExecutionBillingRoute::NativeSubscription,
        models: vec![ManagedHarnessModelObservation {
            model: model(),
            native_model_id: "native-selector".into(),
            confirmed_model_id: None,
            provenance: ExecutionModelProvenance::HarnessDiscovery,
            readiness,
        }],
    }
}

fn selection() -> ExecutionModelSelection {
    ExecutionModelSelection {
        model_id: "observed-model".into(),
        route: None,
        account_reference: None,
    }
}

#[test]
fn provider_projection_preserves_existing_resolution() {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let catalog = ModelCatalogResolver::embedded();
            let list = bcode_model::ModelList {
                models: vec![model()],
                catalog: bcode_model::ModelCatalogHints::default(),
            };
            let expected = catalog.resolve_selection(list.clone(), None, None).await;
            let entries = catalog
                .resolve_execution_catalog(
                    vec![ProviderExecutionCatalog {
                        provider_plugin_id: "test.provider".into(),
                        list,
                        selected_model_id: None,
                        configured_model_id: None,
                        account_reference: Some("api-account".into()),
                        billing_route: ExecutionBillingRoute::ProviderApi,
                        readiness: BTreeMap::from([(
                            "observed-model".into(),
                            ExecutionModelReadiness::Ready,
                        )]),
                    }],
                    vec![],
                )
                .await;
            assert_eq!(entries.len(), expected.models.len());
            assert_eq!(entries[0].model, expected.models[0]);
            let resolved =
                ModelCatalogResolver::resolve_execution_selection(&entries, selection()).unwrap();
            assert_eq!(
                resolved.target.route,
                ExecutionRoute::ModelProvider {
                    provider_plugin_id: "test.provider".into(),
                }
            );
            assert_eq!(resolved.target.confirmed_model_id, None);
        });
}

#[test]
fn exact_harness_only_selection_keeps_requested_and_native_identity_separate() {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let catalog = ModelCatalogResolver::embedded();
            let entries = catalog
                .resolve_execution_catalog(vec![], vec![harness(ExecutionModelReadiness::Ready)])
                .await;
            let mut request = selection();
            request.route = Some(entries[0].route.clone());
            request.account_reference = Some("native-account".into());
            let resolved =
                ModelCatalogResolver::resolve_execution_selection(&entries, request).unwrap();
            assert_eq!(resolved.requested.model_id, "observed-model");
            assert_eq!(resolved.target.native_model_id, "native-selector");
            assert_eq!(resolved.target.confirmed_model_id, None);
            assert_eq!(
                resolved.target.billing_route,
                ExecutionBillingRoute::NativeSubscription
            );
            let mut wrong_account = selection();
            wrong_account.account_reference = Some("unrelated-account".into());
            assert!(matches!(
                ModelCatalogResolver::resolve_execution_selection(&entries, wrong_account),
                Err(ExecutionSelectionError::NotFound)
            ));
            let mut wrong_route = selection();
            wrong_route.route = Some(ExecutionRoute::ModelProvider {
                provider_plugin_id: "test.provider".into(),
            });
            assert!(matches!(
                ModelCatalogResolver::resolve_execution_selection(&entries, wrong_route),
                Err(ExecutionSelectionError::NotFound)
            ));
            let mut absent = selection();
            absent.model_id = "not-advertised".into();
            assert!(matches!(
                ModelCatalogResolver::resolve_execution_selection(&entries, absent),
                Err(ExecutionSelectionError::NotFound)
            ));
        });
}

#[test]
fn ambiguity_never_falls_back_to_the_ready_route() {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let catalog = ModelCatalogResolver::embedded();
            let mut other = harness(ExecutionModelReadiness::Unknown);
            other.instance_id = "other-native".into();
            let entries = catalog
                .resolve_execution_catalog(
                    vec![],
                    vec![harness(ExecutionModelReadiness::Ready), other],
                )
                .await;
            assert!(matches!(
                ModelCatalogResolver::resolve_execution_selection(&entries, selection()),
                Err(ExecutionSelectionError::Ambiguous)
            ));
            let mut exact = selection();
            exact.route = Some(entries[0].route.clone());
            assert!(ModelCatalogResolver::resolve_execution_selection(&entries, exact).is_ok());
        });
}

#[test]
fn authenticated_discovery_is_attempt_eligible_without_claiming_access() {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let catalog = ModelCatalogResolver::embedded();
            let readiness = ExecutionModelReadiness::RouteReady {
                model_access: ExecutionModelAccess::Unknown,
            };
            let entries = catalog
                .resolve_execution_catalog(vec![], vec![harness(readiness)])
                .await;
            let resolved =
                ModelCatalogResolver::resolve_execution_selection(&entries, selection()).unwrap();
            assert_eq!(
                resolved.eligibility,
                ExecutionAttemptEligibility::EligibleToAttempt
            );
            assert_eq!(resolved.target.confirmed_model_id, None);
            assert_eq!(resolved.target.readiness, readiness);
            assert_eq!(
                ModelCatalogResolver::execution_model_eligibility(&entries[0]),
                Ok(ExecutionAttemptEligibility::EligibleToAttempt)
            );
            let mut confirmed = entries[0].clone();
            confirmed.readiness = ExecutionModelReadiness::RouteReady {
                model_access: ExecutionModelAccess::Confirmed,
            };
            assert_eq!(
                ModelCatalogResolver::execution_model_eligibility(&confirmed),
                Ok(ExecutionAttemptEligibility::ModelAccessConfirmed)
            );
            assert_eq!(confirmed.confirmed_model_id, None);
            let mut configured = entries[0].clone();
            configured.provenance = ExecutionModelProvenance::Configuration;
            assert_eq!(
                ModelCatalogResolver::execution_model_eligibility(&configured),
                Err(ExecutionSelectionError::NotReady)
            );
            let mut missing_account = entries[0].clone();
            missing_account.account_reference = None;
            assert_eq!(
                ModelCatalogResolver::execution_model_eligibility(&missing_account),
                Err(ExecutionSelectionError::NotReady)
            );
            let mut denied = entries[0].clone();
            denied.readiness = ExecutionModelReadiness::RouteReady {
                model_access: ExecutionModelAccess::Unavailable,
            };
            assert_eq!(
                ModelCatalogResolver::execution_model_eligibility(&denied),
                Err(ExecutionSelectionError::NotReady)
            );
            let mut unknown_auth = entries[0].clone();
            unknown_auth.readiness = ExecutionModelReadiness::Unknown;
            assert_eq!(
                ModelCatalogResolver::execution_model_eligibility(&unknown_auth),
                Err(ExecutionSelectionError::NotReady)
            );
            let mut other = entries[0].clone();
            other.account_reference = Some("other-account".into());
            assert!(matches!(
                ModelCatalogResolver::resolve_execution_selection(
                    &[entries[0].clone(), other],
                    selection()
                ),
                Err(ExecutionSelectionError::Ambiguous)
            ));
        });
}

#[test]
fn api_billing_does_not_imply_native_credential_custody() {
    assert_eq!(
        serde_json::to_value(ExecutionBillingRoute::Api).unwrap(),
        serde_json::json!("api")
    );
}

#[test]
fn unknown_readiness_and_configuration_are_not_ready() {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let catalog = ModelCatalogResolver::embedded();
            for readiness in [
                ExecutionModelReadiness::Unknown,
                ExecutionModelReadiness::Unavailable,
            ] {
                let entries = catalog
                    .resolve_execution_catalog(vec![], vec![harness(readiness)])
                    .await;
                assert!(matches!(
                    ModelCatalogResolver::resolve_execution_selection(&entries, selection()),
                    Err(ExecutionSelectionError::NotReady)
                ));
            }
            let mut configured = harness(ExecutionModelReadiness::Ready);
            configured.models[0].provenance = ExecutionModelProvenance::Configuration;
            let entries = catalog
                .resolve_execution_catalog(vec![], vec![configured])
                .await;
            assert_eq!(entries[0].readiness, ExecutionModelReadiness::Unknown);
            assert!(matches!(
                ModelCatalogResolver::resolve_execution_selection(&entries, selection()),
                Err(ExecutionSelectionError::NotReady)
            ));
        });
}
