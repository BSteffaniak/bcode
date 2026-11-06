//! Execution membership projection and exact selection on the shared catalog resolver.

use std::collections::BTreeMap;

#[cfg(test)]
mod tests;

use bcode_model::execution::{
    ExecutionAttemptEligibility, ExecutionBillingRoute, ExecutionModel, ExecutionModelAccess,
    ExecutionModelProvenance, ExecutionModelReadiness, ExecutionModelSelection, ExecutionRoute,
    ExecutionSelectionError, ResolvedExecutionModel,
};

/// Existing provider discovery plus nonsecret application routing facts.
#[derive(Debug, Clone)]
pub struct ProviderExecutionCatalog {
    /// Existing provider identity, never a harness ID.
    pub provider_plugin_id: String,
    /// Provider-discovered models with their existing catalog policy.
    pub list: bcode_model::ModelList,
    /// Selected/configured membership is resolved by the existing provider path.
    pub selected_model_id: Option<String>,
    /// Configured model used by the existing provider path.
    pub configured_model_id: Option<String>,
    /// Nonsecret account reference, not credentials.
    pub account_reference: Option<String>,
    /// Adapter-reported billing route, independent of credential custody.
    pub billing_route: ExecutionBillingRoute,
    /// Exact-model readiness. Missing entries remain unknown, including catalog-only models.
    pub readiness: BTreeMap<String, ExecutionModelReadiness>,
}

/// One model observed by a harness adapter; no static provider catalog membership is invented.
#[derive(Debug, Clone)]
pub struct ManagedHarnessModelObservation {
    /// Normalized identity and display/capability metadata.
    pub model: bcode_model::ModelInfo,
    /// Requested native selector, not evidence of which model executed.
    pub native_model_id: String,
    /// Separately confirmed identity, absent unless supported by native evidence.
    pub confirmed_model_id: Option<String>,
    /// Discovery or configuration provenance. Configuration cannot establish readiness.
    pub provenance: ExecutionModelProvenance,
    /// Exact route/account/model readiness reported by the adapter.
    pub readiness: ExecutionModelReadiness,
}

/// Normalized observations for one configured harness instance/account.
///
/// The application adapter consumes generic harness observations and supplies this projection;
/// this crate does not depend on a harness implementation or access native credentials.
#[derive(Debug, Clone)]
pub struct ManagedHarnessExecutionCatalog {
    /// Exact configured instance, not merely the plugin type.
    pub instance_id: String,
    /// Protocol-owning plugin identity.
    pub plugin_id: String,
    /// Nonsecret native account reference, not a Bcode provider credential.
    pub account_reference: Option<String>,
    /// Native billing route, never inferred from a model's name or credential custody.
    pub billing_route: ExecutionBillingRoute,
    /// Only models actually observed or explicitly configured by the adapter.
    pub models: Vec<ManagedHarnessModelObservation>,
}

impl crate::ModelCatalogResolver {
    /// Project provider and configured-harness observations into one execution catalog.
    ///
    /// Provider aliases, enrichment and default membership keep using `resolve_selection`.
    /// Harness entries retain their adapter-supplied exact identity and selectors: provider
    /// catalog metadata cannot establish native availability, account access or acceptance.
    /// Duplicates are retained so selection fails closed rather than silently preferring a source.
    pub async fn resolve_execution_catalog(
        &self,
        providers: Vec<ProviderExecutionCatalog>,
        harnesses: Vec<ManagedHarnessExecutionCatalog>,
    ) -> Vec<ExecutionModel> {
        let mut entries = Vec::new();
        for provider in providers {
            let list = self
                .resolve_selection(
                    provider.list,
                    provider.selected_model_id.as_deref(),
                    provider.configured_model_id.as_deref(),
                )
                .await;
            for model in list.models {
                let readiness = provider
                    .readiness
                    .get(&model.model_id)
                    .copied()
                    .unwrap_or(ExecutionModelReadiness::Unknown);
                entries.push(ExecutionModel {
                    native_model_id: model.model_id.clone(),
                    model,
                    route: ExecutionRoute::ModelProvider {
                        provider_plugin_id: provider.provider_plugin_id.clone(),
                    },
                    account_reference: provider.account_reference.clone(),
                    billing_route: provider.billing_route,
                    confirmed_model_id: None,
                    provenance: ExecutionModelProvenance::ProviderCatalog,
                    readiness,
                });
            }
        }
        for harness in harnesses {
            for observation in harness.models {
                entries.push(ExecutionModel {
                    model: observation.model,
                    route: ExecutionRoute::ManagedHarness {
                        instance_id: harness.instance_id.clone(),
                        plugin_id: harness.plugin_id.clone(),
                    },
                    account_reference: harness.account_reference.clone(),
                    billing_route: harness.billing_route,
                    native_model_id: observation.native_model_id,
                    confirmed_model_id: observation.confirmed_model_id,
                    provenance: observation.provenance,
                    readiness: if observation.provenance
                        == ExecutionModelProvenance::HarnessDiscovery
                    {
                        observation.readiness
                    } else {
                        ExecutionModelReadiness::Unknown
                    },
                });
            }
        }
        entries
    }

    /// Select exactly one observed route/account/model without submission, replay or fallback.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` for absent exact membership, `Ambiguous` for multiple matches (before
    /// checking readiness), or `NotReady` when evidence does not establish discovery/readiness.
    pub fn resolve_execution_selection(
        entries: &[ExecutionModel],
        selection: ExecutionModelSelection,
    ) -> Result<ResolvedExecutionModel, ExecutionSelectionError> {
        let mut matches = entries.iter().filter(|entry| {
            entry.model.model_id == selection.model_id
                && selection
                    .route
                    .as_ref()
                    .is_none_or(|route| route == &entry.route)
                && selection
                    .account_reference
                    .as_ref()
                    .is_none_or(|account| entry.account_reference.as_ref() == Some(account))
        });
        let target = matches.next().ok_or(ExecutionSelectionError::NotFound)?;
        if matches.next().is_some() {
            return Err(ExecutionSelectionError::Ambiguous);
        }
        let eligibility = Self::execution_model_eligibility(target)?;
        Ok(ResolvedExecutionModel {
            requested: selection,
            target: target.clone(),
            eligibility,
        })
    }

    /// Evaluate one catalog member for picker presentation and exact-selection admission.
    ///
    /// `EligibleToAttempt` must not be presented as confirmed model access. This read-only
    /// decision neither authorizes submission nor confirms effective inference identity. Native
    /// adapters may report `RouteReady` only after current route/auth inspection of the exact
    /// configured instance/account. Discovery alone must remain `Unknown`.
    ///
    /// # Errors
    ///
    /// Returns `NotReady` for unknown route/auth readiness, unavailable model access, missing
    /// native account identity, or insufficient discovery/selector evidence.
    pub fn execution_model_eligibility(
        target: &ExecutionModel,
    ) -> Result<ExecutionAttemptEligibility, ExecutionSelectionError> {
        let discovered = matches!(
            (&target.route, target.provenance),
            (
                ExecutionRoute::ModelProvider { .. },
                ExecutionModelProvenance::ProviderCatalog
            ) | (
                ExecutionRoute::ManagedHarness { .. },
                ExecutionModelProvenance::HarnessDiscovery
            )
        );
        let native_account_missing = matches!(&target.route, ExecutionRoute::ManagedHarness { .. })
            && target
                .account_reference
                .as_ref()
                .is_none_or(|account| account.trim().is_empty());
        if !discovered || target.native_model_id.trim().is_empty() || native_account_missing {
            return Err(ExecutionSelectionError::NotReady);
        }
        match target.readiness {
            ExecutionModelReadiness::Ready => Ok(ExecutionAttemptEligibility::ModelAccessConfirmed),
            ExecutionModelReadiness::RouteReady { model_access }
                if matches!(&target.route, ExecutionRoute::ManagedHarness { .. }) =>
            {
                match model_access {
                    ExecutionModelAccess::Unknown => {
                        Ok(ExecutionAttemptEligibility::EligibleToAttempt)
                    }
                    ExecutionModelAccess::Confirmed => {
                        Ok(ExecutionAttemptEligibility::ModelAccessConfirmed)
                    }
                    ExecutionModelAccess::Unavailable => Err(ExecutionSelectionError::NotReady),
                }
            }
            _ => Err(ExecutionSelectionError::NotReady),
        }
    }
}
