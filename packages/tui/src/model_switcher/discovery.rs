//! Discovery for the model switcher: reports each source as soon as it is known.
//!
//! Sources are announced as loading first, so the switcher can show their headings while
//! their models are checked. A source that fails is reported with a user-facing reason; other
//! sources are unaffected. Discovery never selects, launches, or persists anything except
//! registering native tools that are already signed in, so their models can be offered.

use super::inventory::{
    Access, Entry, Fix, Readiness, Source, SourceKey, SourceState, SourceUpdate,
};
use bcode_model::execution::{
    ExecutionBillingRoute, ExecutionModel, ExecutionModelProvenance, ExecutionModelReadiness,
    ExecutionRoute,
};
use bcode_provider_auth_models::external::{
    ExternalAuthApplication as _, ExternalAuthConnection, ExternalConnectionStatus,
};
use bcode_session_view::external_projection::harness_display_label;
use std::collections::BTreeMap;
use tokio::sync::mpsc::Sender;

/// Longest a single source may take to answer before it is reported as not responding.
const SOURCE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);

/// What `request` answered, or nothing if it failed or missed the source deadline.
async fn bounded<T, E>(request: impl std::future::Future<Output = Result<T, E>>) -> Option<T> {
    tokio::time::timeout(SOURCE_DEADLINE, request)
        .await
        .ok()
        .and_then(Result::ok)
}

/// Clients and settings shared by every discovery phase.
struct Context {
    discovery_enabled: bool,
    client: bcode_client::BcodeClient,
    managed: bcode_client::ManagedSessionClient,
    providers: BTreeMap<String, String>,
}

/// Discover everything the switcher can offer, sending each source's report to `updates`.
/// Returns when every source has reported or the receiver is gone.
pub async fn discover(updates: Sender<SourceUpdate>) {
    let config = bcode_config::load_config().ok();
    let client = bcode_client::BcodeClient::default_endpoint();
    let providers = provider_names(&client, config.as_ref()).await;
    let context = Context {
        discovery_enabled: config.as_ref().is_some_and(|config| {
            config
                .onboarding
                .credential_discovery_enabled(false, &bcode_config::ProcessConfigEnvironment)
        }),
        client,
        managed: bcode_client::ManagedSessionClient::default_endpoint(),
        providers,
    };
    for (plugin_id, name) in &context.providers {
        let source = provider_source(plugin_id, name, SourceState::Loading);
        if !send(&updates, source, Vec::new()).await {
            return;
        }
    }
    let (instances, setup) = native_instances(&context.managed, context.discovery_enabled).await;
    if !report_native(&context, &instances, &updates).await {
        return;
    }
    let (providers, accounts) = tokio::join!(
        report_providers(&context, &updates),
        report_accounts(&context, &updates),
    );
    let Some(mut provider_models) = providers else {
        return;
    };
    let Some(account_models) = accounts else {
        return;
    };
    provider_models.extend(account_models);
    let connections = bounded(context.client.external_auth_status())
        .await
        .unwrap_or_default();
    for connection in &connections {
        let update = connection_update(connection, &context.providers, &provider_models);
        if updates.send(update).await.is_err() {
            return;
        }
    }
    let offers = offers(&context, &connections, setup).await;
    if !offers.is_empty() {
        let source = Source {
            key: SourceKey::Connect,
            title: "Available to connect".into(),
            detail: String::new(),
            state: SourceState::Ready,
        };
        let _ = send(&updates, source, offers).await;
    }
}

async fn send(updates: &Sender<SourceUpdate>, source: Source, entries: Vec<Entry>) -> bool {
    updates.send(SourceUpdate { source, entries }).await.is_ok()
}

/// Report each native tool: loading first, then its models.
async fn report_native(
    context: &Context,
    instances: &BTreeMap<String, bcode_harness_models::HarnessInstance>,
    updates: &Sender<SourceUpdate>,
) -> bool {
    if instances.is_empty() {
        return true;
    }
    for instance in instances.values() {
        let source = native_source(
            &instance.instance_id,
            &instance.plugin_id,
            None,
            SourceState::Loading,
        );
        if !send(updates, source, Vec::new()).await {
            return false;
        }
    }
    let inventory = bounded(context.managed.discover_execution_catalog(
        bcode_ipc::harness::ManagedExecutionCatalogRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            instances: instances.values().cloned().collect(),
            provider_plugin_id: None,
        },
    ))
    .await;
    let failed = inventory
        .as_ref()
        .is_none_or(|inventory| !inventory.issues.is_empty());
    let models = inventory
        .map(|inventory| inventory.models)
        .unwrap_or_default();
    for instance in instances.values() {
        let models: Vec<_> = models
            .iter()
            .filter(|model| {
                matches!(&model.route, ExecutionRoute::ManagedHarness { instance_id, .. }
                    if *instance_id == instance.instance_id)
            })
            .cloned()
            .collect();
        let billing = models.first().map(|model| model.billing_route);
        let state = if models.is_empty() && failed {
            SourceState::Failed("Could not list its models; open Setup to check sign-in".into())
        } else {
            SourceState::Ready
        };
        let source = native_source(&instance.instance_id, &instance.plugin_id, billing, state);
        let entries = models
            .into_iter()
            .map(|model| {
                let readiness = Readiness::from_execution(model.readiness, true);
                Entry::new(source.key.clone(), model, Access::Direct, readiness)
            })
            .collect();
        if !send(updates, source, entries).await {
            return false;
        }
    }
    true
}

/// Report each provider's models; returns every provider model, for connection rows.
async fn report_providers(
    context: &Context,
    updates: &Sender<SourceUpdate>,
) -> Option<Vec<ExecutionModel>> {
    let mut provider_models = Vec::new();
    for (plugin_id, name) in &context.providers {
        let inventory = bounded(context.managed.discover_execution_catalog(
            bcode_ipc::harness::ManagedExecutionCatalogRequest {
                request_id: uuid::Uuid::new_v4().to_string(),
                instances: Vec::new(),
                provider_plugin_id: Some(plugin_id.clone()),
            },
        ))
        .await;
        let (state, models) = match inventory {
            Some(inventory) => {
                let models: Vec<_> = inventory
                    .models
                    .into_iter()
                    .filter(|model| {
                        matches!(&model.route, ExecutionRoute::ModelProvider { provider_plugin_id }
                            if provider_plugin_id == plugin_id)
                    })
                    .collect();
                let state = if models.is_empty() && !inventory.issues.is_empty() {
                    SourceState::Failed("Not signed in or unreachable; open Setup".into())
                } else {
                    SourceState::Ready
                };
                (state, models)
            }
            // Without the native harness service, providers still list their own models.
            None => {
                match bounded(context.client.session_model_list(Some(plugin_id.clone()))).await {
                    Some(list) => (
                        SourceState::Ready,
                        list.models
                            .into_iter()
                            .map(|model| provider_model(plugin_id, model))
                            .collect(),
                    ),
                    None => (
                        SourceState::Failed("Could not list its models".into()),
                        Vec::new(),
                    ),
                }
            }
        };
        let source = provider_source(plugin_id, name, state);
        provider_models.extend(models.iter().cloned());
        let entries = models
            .into_iter()
            .map(|model| {
                let readiness = Readiness::from_execution(model.readiness, false);
                Entry::new(source.key.clone(), model, Access::Direct, readiness)
            })
            .collect();
        if !send(updates, source, entries).await {
            return None;
        }
    }
    Some(provider_models)
}

/// Enrolled accounts are discovered independently of default provider routes.
async fn report_accounts(
    context: &Context,
    updates: &Sender<SourceUpdate>,
) -> Option<Vec<ExecutionModel>> {
    let mut models = Vec::new();
    if let Some(accounts) = bounded(context.client.provider_account_models()).await {
        for account in accounts {
            let update = account_update(account);
            models.extend(update.entries.iter().map(|entry| entry.model.clone()));
            if !send(updates, update.source, update.entries).await {
                return None;
            }
        }
    } else {
        let source = Source {
            key: SourceKey::Account {
                plugin_id: String::new(),
                account_reference: String::new(),
            },
            title: "Connected accounts".to_owned(),
            detail: String::new(),
            state: SourceState::Failed(
                "Could not list connected accounts; try reopening /model".to_owned(),
            ),
        };
        if !send(updates, source, Vec::new()).await {
            return None;
        }
    }
    Some(models)
}

/// Credentials that could be connected and native tools that need setup.
async fn offers(
    context: &Context,
    connections: &[ExternalAuthConnection],
    setup: BTreeMap<String, String>,
) -> Vec<Entry> {
    let mut offers = Vec::new();
    if context.discovery_enabled {
        for candidate in bounded(context.client.discover_external_auth())
            .await
            .unwrap_or_default()
        {
            if !candidate.available
                || connections
                    .iter()
                    .any(|connection| connection.grant.binding_id == candidate.binding_id)
            {
                continue;
            }
            let model = placeholder(
                ExecutionRoute::ModelProvider {
                    provider_plugin_id: candidate.target.plugin_id.clone(),
                },
                &candidate.application,
            );
            let reason = format!(
                "{} is signed in on this machine. Connect to use its credentials in Bcode; nothing is copied.",
                candidate.application
            );
            offers.push(Entry::new(
                SourceKey::Connect,
                model,
                Access::Connect(Box::new(candidate)),
                Readiness::Action {
                    fix: Fix::Connect,
                    reason,
                },
            ));
        }
    }
    for (plugin_id, tool) in setup {
        offers.push(Entry::new(
            SourceKey::Connect,
            placeholder(
                ExecutionRoute::ManagedHarness {
                    instance_id: String::new(),
                    plugin_id,
                },
                &tool,
            ),
            Access::Setup { tool: tool.clone() },
            Readiness::Action {
                fix: Fix::Setup,
                reason: format!("{tool} is installed. Set it up or sign in to run it here."),
            },
        ));
    }
    offers
}

async fn provider_names(
    client: &bcode_client::BcodeClient,
    config: Option<&bcode_config::BcodeConfig>,
) -> BTreeMap<String, String> {
    let mut providers: BTreeMap<String, String> = client
        .plugin_services()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|service| service.interface_id == bcode_model::MODEL_PROVIDER_INTERFACE_ID)
        .map(|service| {
            let name = service
                .name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| harness_display_label(&service.plugin_id));
            (service.plugin_id, name)
        })
        .collect();
    if let Some(selected) =
        config.and_then(|config| config.resolved_model_selection().provider_plugin_id)
    {
        providers
            .entry(selected.clone())
            .or_insert_with(|| harness_display_label(&selected));
    }
    providers
}

/// Configured native instances, after registering installations that are already signed in,
/// plus the tools that still need setup, by plugin.
async fn native_instances(
    managed: &bcode_client::ManagedSessionClient,
    discovery_enabled: bool,
) -> (
    BTreeMap<String, bcode_harness_models::HarnessInstance>,
    BTreeMap<String, String>,
) {
    let mut instances = crate::execution_flow::configured_instances()
        .await
        .unwrap_or_default();
    let mut setup = BTreeMap::new();
    let Some(found) = bounded(managed.discover_candidates(
        bcode_ipc::harness::ManagedCandidatesRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
        },
    ))
    .await
    else {
        return (instances, setup);
    };
    for plugin in found.plugins {
        let bcode_harness_models::HarnessCandidatesResponseKind::Candidates { candidates, .. } =
            plugin.kind
        else {
            continue;
        };
        for candidate in candidates {
            if instances.values().any(|instance| {
                instance.plugin_id == candidate.plugin_id
                    && instance.executable == candidate.executable.invocation()
                    && instance.config_namespace == candidate.config_namespace
            }) {
                continue;
            }
            // An installation that is already signed in is offered directly: its models
            // appear as rows, and choosing one is the decision to run it.
            if discovery_enabled
                && candidate.readiness
                    == bcode_harness_models::HarnessCandidateReadiness::ExistingNativeLogin
                && let Ok(instance) =
                    Box::pin(crate::native_setup::auto_register(candidate.clone())).await
            {
                instances.insert(instance.instance_id.clone(), instance);
            } else if !instances
                .values()
                .any(|instance| instance.plugin_id == candidate.plugin_id)
            {
                setup.insert(
                    candidate.plugin_id.clone(),
                    harness_display_label(&candidate.plugin_id),
                );
            }
        }
    }
    (instances, setup)
}

fn native_source(
    instance_id: &str,
    plugin_id: &str,
    billing: Option<ExecutionBillingRoute>,
    state: SourceState,
) -> Source {
    Source {
        key: SourceKey::Native {
            instance_id: instance_id.to_owned(),
        },
        title: harness_display_label(plugin_id),
        detail: match billing {
            Some(ExecutionBillingRoute::NativeSubscription) => "subscription".into(),
            Some(ExecutionBillingRoute::Api | ExecutionBillingRoute::ProviderApi) => {
                "API key".into()
            }
            Some(ExecutionBillingRoute::Unknown) | None => "runs the tool".into(),
        },
        state,
    }
}

fn provider_source(plugin_id: &str, name: &str, state: SourceState) -> Source {
    Source {
        key: SourceKey::Provider {
            plugin_id: plugin_id.to_owned(),
        },
        title: name.to_owned(),
        detail: "Bcode provider".into(),
        state,
    }
}

/// Rows for one approved connection: the models it recommends, on its provider.
fn connection_update(
    connection: &ExternalAuthConnection,
    providers: &BTreeMap<String, String>,
    provider_models: &[ExecutionModel],
) -> SourceUpdate {
    let target = &connection.grant.target;
    let application = connection.grant.application.clone();
    let provider = providers
        .get(&target.plugin_id)
        .cloned()
        .unwrap_or_else(|| harness_display_label(&target.plugin_id));
    let readiness = match connection.status {
        ExternalConnectionStatus::Ready { .. } => Readiness::Available,
        ExternalConnectionStatus::Expired => Readiness::Unavailable(format!(
            "{application} sign-in expired. Open {application} to renew it."
        )),
        ExternalConnectionStatus::AccountChanged => Readiness::Action {
            fix: Fix::Reapprove,
            reason: format!("{application} is signed in to a different account. Approve it again."),
        },
        ExternalConnectionStatus::SourceUnavailable => Readiness::Unavailable(format!(
            "{application} credentials were not found. Sign in to {application}."
        )),
        ExternalConnectionStatus::DeclarationChanged => Readiness::Unavailable(
            "This connection is no longer offered. Run `bcode auth disconnect` to remove it."
                .into(),
        ),
    };
    let source = Source {
        key: SourceKey::Connection {
            binding_id: connection.grant.binding_id.clone(),
        },
        title: provider,
        detail: format!("via {application}"),
        state: SourceState::Ready,
    };
    let route = ExecutionRoute::ModelProvider {
        provider_plugin_id: target.plugin_id.clone(),
    };
    let entries = connection
        .recommended_models
        .iter()
        .map(|model_id| {
            let model = provider_models
                .iter()
                .find(|model| model.route == route && model.model.model_id == *model_id)
                .cloned()
                .map_or_else(
                    || placeholder(route.clone(), model_id),
                    |mut model| {
                        model.account_reference =
                            Some(format!("external:{}", connection.grant.binding_id));
                        model
                    },
                );
            Entry::new(
                source.key.clone(),
                model,
                Access::Connection {
                    binding_id: connection.grant.binding_id.clone(),
                    application: application.clone(),
                    target: target.clone(),
                },
                readiness.clone(),
            )
        })
        .collect();
    SourceUpdate { source, entries }
}

/// Project one enrolled account's application-owned listing into a distinct source.
#[must_use]
pub fn account_update(account: bcode_model::ProviderAccountModels) -> SourceUpdate {
    let key = SourceKey::Account {
        plugin_id: account.provider_plugin_id.clone(),
        account_reference: account.account_reference.clone(),
    };
    let source = Source {
        key: key.clone(),
        title: account.display_name,
        detail: "Connected account".to_owned(),
        state: SourceState::Ready,
    };
    let entries = account
        .models
        .models
        .into_iter()
        .map(|model| {
            let mut model = provider_model(&account.provider_plugin_id, model);
            model.account_reference = Some(account.account_reference.clone());
            let readiness = Readiness::from_execution(model.readiness, false);
            Entry::new(
                key.clone(),
                model,
                Access::Account {
                    account_reference: account.account_reference.clone(),
                },
                readiness,
            )
        })
        .collect();
    SourceUpdate { source, entries }
}

/// A provider's own model listing as a provider route; access is checked on first use.
fn provider_model(plugin_id: &str, model: bcode_model::ModelInfo) -> ExecutionModel {
    ExecutionModel {
        native_model_id: model.model_id.clone(),
        model,
        route: ExecutionRoute::ModelProvider {
            provider_plugin_id: plugin_id.to_owned(),
        },
        account_reference: None,
        billing_route: ExecutionBillingRoute::ProviderApi,
        confirmed_model_id: None,
        provenance: ExecutionModelProvenance::ProviderCatalog,
        readiness: ExecutionModelReadiness::Unknown,
    }
}

/// A row for something that is not yet a known model: a recommended model missing from its
/// provider's listing, or a connect/setup offer.
fn placeholder(route: ExecutionRoute, model_id: &str) -> ExecutionModel {
    ExecutionModel {
        model: bcode_model::ModelInfo {
            model_id: model_id.to_owned(),
            display_name: model_id.to_owned(),
            is_default: false,
            context_window: None,
            max_output_tokens: None,
            max_image_input_base64_bytes: None,
            capabilities: std::collections::BTreeSet::new(),
            feature_support: bcode_model::ModelFeatureSupport::default(),
            reasoning: None,
            cache: bcode_model::ModelCacheInfo::default(),
            metadata_source: None,
            pricing: None,
            api_surface: None,
            visibility: bcode_model::ModelVisibility::Visible,
        },
        route,
        account_reference: None,
        billing_route: ExecutionBillingRoute::Unknown,
        native_model_id: model_id.to_owned(),
        confirmed_model_id: None,
        provenance: ExecutionModelProvenance::Configuration,
        readiness: ExecutionModelReadiness::Unknown,
    }
}
