//! Session import orchestration for the local server.

use crate::{ErrorResponse, ServerError, ServerState, SharedWriter, send_response};
use bcode_ipc::{Response, ResponsePayload};
use bcode_session_import::{
    DiscoverImportableSessionsRequest, DiscoverImportableSessionsResponse, ImportableSessionEvent,
    ImportableSessionEventKind, LoadImportableSessionRequest, OP_DISCOVER_IMPORTABLE_SESSIONS,
    OP_LOAD_IMPORTABLE_SESSION, SESSION_IMPORT_INTERFACE_ID,
};
use bcode_session_models::{
    ClientId, SessionEventKind, SessionEventProvenance, SessionId, SessionImportSummary,
};
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Secret-safe failure from unpublished remote history retrieval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRetrievalError {
    /// Normalized diagnostic; never a provider-supplied message.
    pub message: &'static str,
    /// Provider-requested minimum retry delay, if supported metadata supplied it.
    pub retry_after_seconds: Option<u64>,
}

impl From<&'static str> for HistoryRetrievalError {
    fn from(message: &'static str) -> Self {
        Self {
            message,
            retry_after_seconds: None,
        }
    }
}

impl std::fmt::Display for HistoryRetrievalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for HistoryRetrievalError {}

/// Explicit connection and configuration scope for remote history operations.
#[derive(Clone, Copy)]
pub struct HistorySource<'a> {
    /// Registered provider plugin; no auth-pool fallback is used.
    pub provider_plugin_id: &'a str,
    /// Exact configured profile name.
    pub profile: &'a str,
    /// Configuration discovery scope, supplied by the application caller.
    pub working_directory: &'a Path,
}

/// Credential-free discovery result; coverage does not imply authorization.
pub struct HistoryProfileDiscovery {
    /// Provider-owned scope coverage, retained for truthful source status.
    pub capabilities: bcode_model::history::HistoryCapabilities,
    /// Compatible profiles and unresolved candidates requiring attention.
    pub profiles: std::collections::BTreeMap<
        String,
        Result<
            bcode_provider_auth::ResolvedAuthProfile,
            bcode_provider_auth::AuthProfileResolutionError,
        >,
    >,
}

impl ServerState {
    async fn history_capabilities(
        &self,
        provider_plugin_id: &str,
        cancellation: &bcode_plugin_sdk::ServiceCancellation,
    ) -> Result<bcode_model::history::HistoryCapabilities, HistoryRetrievalError> {
        if cancellation.is_cancelled() {
            return Err("history retrieval cancelled".into());
        }
        let interface = crate::model_provider_interface_for_plugin(self, provider_plugin_id)
            .ok_or("history provider is unavailable")?;
        let response = self
            .plugins
            .invoke_service_json_response_scoped_cancellable(
                provider_plugin_id,
                interface,
                bcode_model::history::OP_HISTORY_CAPABILITIES,
                &bcode_model::history::HistoryCapabilitiesRequest { schema_version: 1 },
                bcode_plugin::PluginInvocationScope::default(),
                std::time::Duration::from_secs(10),
                cancellation,
            )
            .await
            .map_err(|error| history_retrieval_error(&error))?;
        let capabilities: bcode_model::history::HistoryCapabilities =
            decode_history_response(response)?;
        if capabilities.schema_version != 1 {
            return Err("history provider response is incompatible".into());
        }
        Ok(capabilities)
    }

    /// Discover configured profiles for an enabled provider without reading secrets.
    ///
    /// Results exclude policy-disabled and incompatible profiles, retaining
    /// per-profile resolution errors for enabled candidates. A resolved profile
    /// does not imply verified remote identity or permission to publish history.
    ///
    /// # Errors
    /// Returns a normalized error for an unavailable provider, unreadable metadata,
    /// cancellation, timeout, or exhausted admission capacity. Blocking metadata
    /// work retains its admission permit until it actually finishes.
    pub async fn discover_history_profiles(
        &self,
        provider_id: &str,
        working_directory: &Path,
        cancellation: &bcode_plugin_sdk::ServiceCancellation,
    ) -> Result<HistoryProfileDiscovery, HistoryRetrievalError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        check_history_auth_active(cancellation, deadline)?;
        let provider = self
            .plugins
            .auth_provider_registry()
            .get(provider_id)
            .ok_or("history provider is unavailable")?;
        let owner = provider.plugin_id.clone();
        let discovery_owner = owner.clone();
        let provider_id = provider_id.to_owned();
        let paths = bcode_config::default_config_paths_from(working_directory);
        let permit = self
            .history_auth_gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| HistoryRetrievalError {
                message: "history profile discovery is busy; retry later",
                retry_after_seconds: Some(1),
            })?;
        let task_cancellation = cancellation.clone();
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            check_history_auth_active(&task_cancellation, deadline)?;
            let config = bcode_config::load_config_from_paths(&paths)
                .map_err(|_| HistoryRetrievalError::from("history configuration is unavailable"))?;
            check_history_auth_active(&task_cancellation, deadline)?;
            if !config.session_import.enabled || !config.session_import.chatgpt.enabled {
                return Err("history synchronization is disabled".into());
            }
            let runtime = if config.active_context.is_some() {
                bcode_config::RuntimeAuthSubscriptions::default()
            } else {
                bcode_config::try_load_runtime_auth_subscriptions().map_err(|_| {
                    HistoryRetrievalError::from("history profile metadata is unavailable")
                })?
            };
            check_history_auth_active(&task_cancellation, deadline)?;
            let mut profiles = bcode_provider_auth::discover_provider_profiles(
                &config,
                &provider_id,
                &discovery_owner,
                &runtime,
            );
            retain_history_policy_profiles(&mut profiles, &config.session_import);
            Ok(profiles)
        });
        let mut profiles =
            await_history_auth(task, cancellation, std::time::Duration::from_secs(30))
                .await
                .map_err(|error| match error.message {
                    "history authentication timed out; retry or unlock the vault" => {
                        "history profile discovery timed out; retry later".into()
                    }
                    "history authentication is unavailable" => {
                        "history profile discovery is unavailable".into()
                    }
                    _ => error,
                })?;
        let capabilities = self.history_capabilities(&owner, cancellation).await?;
        profiles.retain(|_, profile| {
            profile.as_ref().map_or(true, |profile| {
                history_scheme_supported(&capabilities, profile.profile.scheme.as_deref())
            })
        });
        Ok(HistoryProfileDiscovery {
            capabilities,
            profiles,
        })
    }

    async fn history_profile_context(
        &self,
        provider_plugin_id: &str,
        profile: &str,
        working_directory: &Path,
        cancellation: &bcode_plugin_sdk::ServiceCancellation,
    ) -> Result<(&'static str, bcode_model::ProviderRequestContext), HistoryRetrievalError> {
        if cancellation.is_cancelled() {
            return Err("history retrieval cancelled".into());
        }
        let interface = crate::model_provider_interface_for_plugin(self, provider_plugin_id)
            .ok_or("history provider is unavailable")?;
        let config_paths = bcode_config::default_config_paths_from(working_directory);
        let provider = provider_plugin_id.to_owned();
        let profile = profile.to_owned();
        let permit = self
            .history_auth_gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| HistoryRetrievalError {
                message: "history authentication is busy; retry later",
                retry_after_seconds: Some(1),
            })?;
        let auth_cancellation = cancellation.clone();
        let auth_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        // The blocking task owns the permit: a caller timing out cannot release
        // capacity while custody is still executing. No unbounded waiter queue.
        let context = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            check_history_auth_active(&auth_cancellation, auth_deadline)?;
            let config = history_enabled_config(
                bcode_config::load_config_from_paths(&config_paths),
                &profile,
            )?;
            check_history_auth_active(&auth_cancellation, auth_deadline)?;
            bcode_provider_auth::resolve_explicit_profile_context(&config, &provider, &profile)
                .map_err(HistoryRetrievalError::from)
        });
        let context =
            await_history_auth(context, cancellation, std::time::Duration::from_secs(30)).await?;
        Ok((interface, context))
    }

    /// Discover one bounded metadata-only page for an explicit history profile.
    ///
    /// A short page does not establish complete coverage or verified account scope.
    /// Entries are not canonical sessions or searchable imported content.
    ///
    /// # Errors
    /// Returns a normalized error for disabled policy, unavailable authentication,
    /// cancellation, provider failure, or incompatible response metadata.
    pub async fn list_chatgpt_history_page(
        &self,
        source: HistorySource<'_>,
        offset: u64,
        archived: bool,
        cancellation: &bcode_plugin_sdk::ServiceCancellation,
    ) -> Result<bcode_model::history::ListHistoryPageResponse, HistoryRetrievalError> {
        let HistorySource {
            provider_plugin_id,
            profile,
            working_directory,
        } = source;
        let (interface, provider_context) = self
            .history_profile_context(provider_plugin_id, profile, working_directory, cancellation)
            .await?;
        let request = bcode_model::history::ListHistoryPageRequest {
            schema_version: 1,
            provider_context,
            offset,
            limit: 50,
            archived,
        };
        let response = self
            .plugins
            .invoke_service_json_response_scoped_cancellable(
                provider_plugin_id,
                interface,
                bcode_model::history::OP_LIST_HISTORY_PAGE,
                &request,
                bcode_plugin::PluginInvocationScope::default(),
                std::time::Duration::from_mins(1),
                cancellation,
            )
            .await
            .map_err(|error| history_retrieval_error(&error))?;
        let page: bcode_model::history::ListHistoryPageResponse =
            decode_history_response(response)?;
        validate_history_page(&page, offset, request.limit)?;
        Ok(page)
    }

    /// Retrieve one unpublished history revision using an explicitly selected profile.
    ///
    /// This does not verify remote account scope, refresh credentials, or publish
    /// a session. Callers must establish scope before canonical publication. The
    /// daemon's import policy applies; no model selection or auth pool is inherited.
    ///
    /// # Errors
    /// Returns a secret-safe error if policy forbids retrieval, authentication or
    /// routing fails, cancellation occurs, or the provider response is invalid.
    pub async fn load_chatgpt_history_snapshot(
        &self,
        source: HistorySource<'_>,
        conversation_id: &str,
        selected_node: Option<&str>,
        cancellation: &bcode_plugin_sdk::ServiceCancellation,
    ) -> Result<bcode_session_import::ImportableHistorySnapshot, HistoryRetrievalError> {
        let HistorySource {
            provider_plugin_id,
            profile,
            working_directory,
        } = source;
        let (interface, provider_context) = self
            .history_profile_context(provider_plugin_id, profile, working_directory, cancellation)
            .await?;
        let request = bcode_model::history::LoadHistorySnapshotRequest {
            schema_version: 1,
            provider_context,
            conversation_id: conversation_id.to_owned(),
            selected_node: selected_node.map(str::to_owned),
        };
        let response = self
            .plugins
            .invoke_service_json_response_scoped_cancellable(
                provider_plugin_id,
                interface,
                bcode_model::history::OP_LOAD_HISTORY_SNAPSHOT,
                &request,
                bcode_plugin::PluginInvocationScope::default(),
                std::time::Duration::from_mins(1),
                cancellation,
            )
            .await
            .map_err(|error| history_retrieval_error(&error))?;
        let snapshot = decode_history_response(response)?;
        validate_history_snapshot(&snapshot, conversation_id, selected_node)?;
        Ok(snapshot)
    }
}

fn retain_history_policy_profiles<T>(
    profiles: &mut std::collections::BTreeMap<String, T>,
    policy: &bcode_config::SessionImportConfig,
) {
    profiles.retain(|name, _| policy.chatgpt_sync_enabled(name));
}

fn history_scheme_supported(
    capabilities: &bcode_model::history::HistoryCapabilities,
    scheme: Option<&str>,
) -> bool {
    scheme.is_some_and(|scheme| {
        !scheme.trim().is_empty() && capabilities.auth_schemes.contains(scheme)
    })
}

fn check_history_auth_active(
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
    deadline: std::time::Instant,
) -> Result<(), HistoryRetrievalError> {
    if cancellation.is_cancelled() {
        return Err("history retrieval cancelled".into());
    }
    if std::time::Instant::now() >= deadline {
        return Err("history authentication timed out; retry or unlock the vault".into());
    }
    Ok(())
}

async fn await_history_auth<T>(
    mut task: tokio::task::JoinHandle<Result<T, HistoryRetrievalError>>,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
    timeout: std::time::Duration,
) -> Result<T, HistoryRetrievalError> {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        if cancellation.is_cancelled() {
            task.abort();
            return Err("history retrieval cancelled".into());
        }
        tokio::select! {
            biased;
            () = &mut deadline => {
                task.abort();
                return Err("history authentication timed out; retry or unlock the vault".into());
            }
            result = &mut task => {
                if cancellation.is_cancelled() {
                    return Err("history retrieval cancelled".into());
                }
                return result.map_err(|_| HistoryRetrievalError::from("history authentication is unavailable"))?;
            }
            () = tokio::time::sleep(std::time::Duration::from_millis(25)) => {}
        }
    }
}

fn history_enabled_config(
    config: Result<bcode_config::BcodeConfig, bcode_config::ConfigError>,
    profile: &str,
) -> Result<bcode_config::BcodeConfig, HistoryRetrievalError> {
    let config =
        config.map_err(|_| HistoryRetrievalError::from("history configuration is unavailable"))?;
    if !config.session_import.chatgpt_sync_enabled(profile) {
        return Err("history synchronization is disabled for this profile".into());
    }
    Ok(config)
}

fn validate_history_page(
    page: &bcode_model::history::ListHistoryPageResponse,
    offset: u64,
    limit: u16,
) -> Result<(), HistoryRetrievalError> {
    let mut identities = std::collections::BTreeSet::new();
    if page.schema_version != 1
        || limit == 0
        || page.entries.len() > usize::from(limit)
        || page.next_offset.is_some_and(|next| next <= offset)
        || (page.entries.is_empty() && page.next_offset.is_some())
        || page.entries.iter().any(|entry| {
            entry.conversation_id.trim().is_empty()
                || !identities.insert(&entry.conversation_id)
                || entry
                    .updated_at
                    .is_some_and(|timestamp| !timestamp.is_finite() || timestamp < 0.0)
        })
    {
        return Err("history provider returned an incompatible page".into());
    }
    Ok(())
}

fn decode_history_response<R: serde::de::DeserializeOwned>(
    response: bcode_plugin_sdk::ServiceResponse,
) -> Result<R, HistoryRetrievalError> {
    if let Some(error) = response.error {
        let retry_after_seconds = if error.code == "history_rate_limited" {
            serde_json::from_slice::<bcode_model::history::HistoryRateLimitDetails>(
                &response.payload,
            )
            .ok()
            .filter(|details| details.schema_version == 1)
            .and_then(|details| details.retry_after_seconds)
        } else {
            None
        };
        return Err(HistoryRetrievalError {
            message: history_retrieval_error(&bcode_plugin::PluginServiceCallError::Service {
                code: error.code,
                message: String::new(),
            }),
            retry_after_seconds,
        });
    }
    serde_json::from_slice(&response.payload)
        .map_err(|_| HistoryRetrievalError::from("history provider response is incompatible"))
}

fn history_retrieval_error(error: &bcode_plugin::PluginServiceCallError) -> &'static str {
    use bcode_plugin::PluginServiceCallError;
    match error {
        PluginServiceCallError::Service { code, .. } => match code.as_str() {
            "cancelled" | "history_cancelled" => "history retrieval cancelled",
            "history_refresh_required" => "history credentials require provider-owned refresh",
            "history_auth_required" => "history authentication required; reconnect this profile",
            "history_profile_required" => "history requires an explicit compatible auth profile",
            "history_access_denied" => "history access denied for this profile",
            "history_access_challenge" => "history access requires an upstream challenge",
            "history_rate_limited" => "history rate limited; retry later",
            "history_not_found" => "remote history unavailable; retain imported history",
            "history_transient" => "temporary history transport or server failure; retry later",
            "history_too_large" => "history exceeds retrieval budget; import remains incomplete",
            "history_incomplete" => "history generation is incomplete; retry retrieval later",
            "history_decode_failed" | "history_revision_failed" => {
                "history conversion failed; import remains incomplete"
            }
            "history_incompatible_response" | "history_unsupported_version" => {
                "history provider response is incompatible"
            }
            "history_invalid_request" => "history request is invalid",
            "unsupported_operation" => "history retrieval is unsupported by this provider",
            _ => "history provider retrieval failed",
        },
        PluginServiceCallError::ResponseDecode(_) => "history provider response is incompatible",
        PluginServiceCallError::RequestEncode(_) => "history request could not be encoded",
        PluginServiceCallError::Invoke(_) => "history provider invocation failed",
    }
}

fn validate_history_snapshot(
    snapshot: &bcode_session_import::ImportableHistorySnapshot,
    conversation_id: &str,
    selected_node: Option<&str>,
) -> Result<(), &'static str> {
    let mut event_ids = std::collections::BTreeSet::new();
    if snapshot.schema_version != 1
        || snapshot.conversation_id != conversation_id
        || snapshot.revision_id.trim().is_empty()
        || snapshot.selected_node.trim().is_empty()
        || snapshot.events.iter().any(|event| {
            !matches!(
                event.kind,
                ImportableSessionEventKind::UserMessage { .. }
                    | ImportableSessionEventKind::AssistantMessage { .. }
            ) || event
                .external_event_id
                .as_deref()
                .is_none_or(|id| id.trim().is_empty() || !event_ids.insert(id))
        })
        || snapshot
            .message_metadata
            .keys()
            .any(|id| !event_ids.contains(id.as_str()))
        || selected_node.is_some_and(|node| node != snapshot.selected_node)
    {
        return Err("history provider returned an incompatible snapshot");
    }
    Ok(())
}

async fn all_cached_sessions(state: &ServerState) -> Vec<bcode_session_models::SessionSummary> {
    state.sessions.all_session_summaries().await
}

/// Stable synthetic ID for an external session while it is still importable.
#[must_use]
pub fn external_session_id(source_id: &str, external_session_id: &str) -> SessionId {
    let mut hasher = Sha256::new();
    hasher.update(b"bcode external session");
    hasher.update(source_id.as_bytes());
    hasher.update([0]);
    hasher.update(external_session_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    SessionId(Uuid::from_bytes(bytes))
}

fn imported_session_event_kind(
    event: ImportableSessionEventKind,
    compacted_through_sequence: &mut u64,
) -> SessionEventKind {
    match event {
        ImportableSessionEventKind::UserMessage { text } => SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text,
            admission: bcode_session_models::TurnAdmissionMetadata::default(),
        },
        ImportableSessionEventKind::AssistantMessage { text } => {
            SessionEventKind::AssistantMessage { text }
        }
        ImportableSessionEventKind::AssistantReasoningMessage { text } => {
            SessionEventKind::AssistantReasoningMessage { text }
        }
        ImportableSessionEventKind::ToolCallRequested {
            tool_call_id,
            tool_name,
            arguments_json,
        } => SessionEventKind::ToolCallRequested {
            tool_call_id,
            producer_plugin_id: None,
            tool_name,
            arguments_json,
            working_directory: None,
        },
        ImportableSessionEventKind::ToolCallFinished {
            tool_call_id,
            result,
            is_error,
        } => SessionEventKind::ToolInvocationResultRecorded {
            record: bcode_session_models::ToolInvocationResultRecord {
                invocation_id: tool_call_id,
                model_output: result.clone(),
                is_error,
                presentation: None,
                result: Some(bcode_session_models::ToolInvocationResult::Text { text: result }),
                content: Vec::new(),
            },
        },
        ImportableSessionEventKind::ModelUsage {
            input_tokens,
            output_tokens,
            total_tokens,
            cached_input_tokens,
            cache_write_input_tokens,
            reasoning_tokens,
        } => SessionEventKind::ModelUsage {
            turn_id: "imported".to_owned(),
            usage: bcode_session_models::SessionTokenUsage {
                input_tokens,
                output_tokens,
                total_tokens,
                cached_input_tokens,
                cache_write_input_tokens,
                reasoning_tokens,
                ..bcode_session_models::SessionTokenUsage::default()
            },
        },
        ImportableSessionEventKind::ModelChanged { provider, model } => {
            // Imported history carries no selection provenance, so treat it as a resolved default
            // rather than inferring a deliberate in-session choice.
            SessionEventKind::ModelChanged {
                provider,
                model,
                selection_source: bcode_session_models::ModelSelectionSource::ConfigDefault,
            }
        }
        ImportableSessionEventKind::AgentChanged { agent_id } => {
            SessionEventKind::AgentChanged { agent_id }
        }
        ImportableSessionEventKind::ContextCompacted { summary } => {
            *compacted_through_sequence = compacted_through_sequence.saturating_add(1);
            SessionEventKind::ContextCompacted {
                summary,
                compacted_through_sequence: *compacted_through_sequence,
            }
        }
        ImportableSessionEventKind::SystemMessage { text } => {
            SessionEventKind::SystemMessage { text }
        }
    }
}

/// Import an external session by source/external id.
///
/// # Errors
///
/// Returns an error if session import is disabled, no provider can load the
/// requested session, or native Bcode session creation fails.
#[allow(clippy::too_many_lines)]
pub async fn import_external_session(
    state: &ServerState,
    source_id: &str,
    external_session_id: &str,
    fallback_working_directory: &Path,
) -> Result<(SessionId, Vec<bcode_session_import::ImportWarning>), String> {
    let config_paths = bcode_config::default_config_paths_from(fallback_working_directory);
    require_import_enabled(bcode_config::load_config_from_paths(&config_paths))?;
    if let Some(existing) = all_cached_sessions(state)
        .await
        .into_iter()
        .find(|session| {
            session.import.as_ref().is_some_and(|import| {
                import.source_id == source_id && import.external_session_id == external_session_id
            })
        })
    {
        return Ok((existing.id, Vec::new()));
    }
    let providers = state
        .plugins
        .registry()
        .service_registry()
        .providers_for(SESSION_IMPORT_INTERFACE_ID)
        .cloned()
        .ok_or_else(|| "no session import providers are loaded".to_string())?;
    for plugin_id in providers {
        let discovery = state
            .plugins
            .invoke_service_json::<_, DiscoverImportableSessionsResponse>(
                &plugin_id,
                SESSION_IMPORT_INTERFACE_ID,
                OP_DISCOVER_IMPORTABLE_SESSIONS,
                &DiscoverImportableSessionsRequest::default(),
            )
            .await
            .map_err(|error| error.to_string())?;
        let Some(summary) = discovery.sessions.into_iter().find(|summary| {
            summary.status == bcode_session_import::ImportableSessionStatus::Available
                && summary.source_id == source_id
                && summary.external_session_id == external_session_id
        }) else {
            continue;
        };
        let importable = state
            .plugins
            .invoke_service_json::<_, bcode_session_import::ImportableSession>(
                &plugin_id,
                SESSION_IMPORT_INTERFACE_ID,
                OP_LOAD_IMPORTABLE_SESSION,
                &LoadImportableSessionRequest {
                    source_id: summary.source_id.clone(),
                    external_session_id: summary.external_session_id.clone(),
                    locator: summary.locator.clone(),
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        let name = importable
            .summary
            .title
            .clone()
            .or_else(|| Some(importable.summary.external_session_id.clone()));
        let working_directory = importable
            .summary
            .working_directory
            .clone()
            .unwrap_or_else(|| fallback_working_directory.to_path_buf());
        let events = importable
            .events
            .into_iter()
            .scan(0_u64, |compacted_through_sequence, event| {
                let provenance = import_event_provenance(&event, &importable.summary.locator);
                let kind = imported_session_event_kind(event.kind, compacted_through_sequence);
                Some((kind, provenance))
            })
            .collect();
        let session = state
            .sessions
            .import_session(
                name,
                working_directory,
                SessionImportSummary {
                    source_id: importable.summary.source_id,
                    source_display_name: importable.summary.source_display_name,
                    external_session_id: importable.summary.external_session_id,
                    imported_at_ms: current_unix_millis(),
                },
                events,
            )
            .await
            .map_err(|error| error.to_string())?;
        return Ok((session.id, importable.warnings));
    }
    Err("external session not found".to_string())
}

fn import_event_provenance(
    event: &ImportableSessionEvent,
    locator: &str,
) -> Option<SessionEventProvenance> {
    (event.external_event_id.is_some() || event.timestamp_ms.is_some() || !locator.is_empty()).then(
        || SessionEventProvenance {
            source_event_id: event.external_event_id.clone(),
            source_timestamp_ms: event.timestamp_ms,
            source_locator: (!locator.is_empty()).then(|| locator.to_owned()),
        },
    )
}

fn require_import_enabled(
    config: Result<bcode_config::BcodeConfig, bcode_config::ConfigError>,
) -> Result<(), String> {
    let config = config.map_err(|_| "session import configuration is unavailable".to_owned())?;
    if !config.session_import.enabled {
        return Err("session import is disabled".to_owned());
    }
    Ok(())
}

fn import_error_response(error: &str) -> ErrorResponse {
    let (code, message) = match error {
        "session import configuration is unavailable" => (
            "session_import_configuration_unavailable",
            "session import configuration is unavailable; fix configuration before retrying",
        ),
        "session import is disabled" => ("session_import_disabled", "session import is disabled"),
        "no session import providers are loaded" => (
            "session_import_unavailable",
            "session import provider is unavailable",
        ),
        "external session not found" => (
            "external_session_not_found",
            "external session was not found",
        ),
        _ => ("import_failed", "session import failed"),
    };
    ErrorResponse::new(code, message)
}

/// Send IPC response for an explicit external import request.
///
/// # Errors
///
/// Returns an error if the response cannot be written or the imported session
/// summary cannot be loaded.
pub async fn handle_import_external_session(
    request_id: u64,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    source_id: &str,
    external_session_id: &str,
    working_directory: Option<PathBuf>,
) -> Result<(), ServerError> {
    let Some(working_directory) = working_directory else {
        return send_response(
            writer,
            request_id,
            Response::Err(ErrorResponse::new(
                "import_cwd_required",
                "session import requests must include the caller working directory",
            )),
        )
        .await;
    };
    match import_external_session(state, source_id, external_session_id, &working_directory).await {
        Ok((session_id, warnings)) => {
            let session = state.sessions.session_summary(session_id).await?;
            state
                .session_catalog
                .upsert_native_session(session.clone())
                .await;
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::ExternalSessionImported { session, warnings }),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(import_error_response(&error)),
            )
            .await
        }
    }
}

/// Resolve a synthetic external session ID into an imported native Bcode session ID.
pub async fn resolve_attach_session_id(
    state: &Arc<ServerState>,
    session_id: SessionId,
    fallback_working_directory: Option<&Path>,
) -> SessionId {
    if state.sessions.session_summary(session_id).await.is_ok() {
        return session_id;
    }
    // Existing native/imported history remains accessible when import is disabled,
    // but resolving a synthetic ID must not invoke discovery before authorization.
    let Some(fallback_working_directory) = fallback_working_directory else {
        return session_id;
    };
    let config_paths = bcode_config::default_config_paths_from(fallback_working_directory);
    if let Err(error) = require_import_enabled(bcode_config::load_config_from_paths(&config_paths))
    {
        tracing::warn!("cannot discover external session: {error}");
        return session_id;
    }
    let Some((source_id, external_session_id)) =
        external_parts_from_session_id(state, session_id).await
    else {
        return session_id;
    };
    match import_external_session(
        state,
        &source_id,
        &external_session_id,
        fallback_working_directory,
    )
    .await
    {
        Ok((imported_session_id, warnings)) => {
            if let Ok(session) = state.sessions.session_summary(imported_session_id).await {
                state.session_catalog.upsert_native_session(session).await;
            }
            if !warnings.is_empty() {
                tracing::warn!(
                    "imported [{source_id}] session with {} warnings",
                    warnings.len()
                );
                for warning in warnings {
                    tracing::warn!("import warning: {}: {}", warning.code, warning.message);
                }
            }
            imported_session_id
        }
        Err(error) => {
            tracing::warn!(
                "failed to import external session {source_id}/{external_session_id}: {error}"
            );
            session_id
        }
    }
}

async fn external_parts_from_session_id(
    state: &ServerState,
    session_id: SessionId,
) -> Option<(String, String)> {
    let providers = state
        .plugins
        .registry()
        .service_registry()
        .providers_for(SESSION_IMPORT_INTERFACE_ID)?
        .clone();
    for plugin_id in providers {
        let response = state
            .plugins
            .invoke_service_json::<_, DiscoverImportableSessionsResponse>(
                &plugin_id,
                SESSION_IMPORT_INTERFACE_ID,
                OP_DISCOVER_IMPORTABLE_SESSIONS,
                &DiscoverImportableSessionsRequest::default(),
            )
            .await
            .ok()?;
        for summary in response.sessions {
            if summary.status != bcode_session_import::ImportableSessionStatus::Available {
                continue;
            }
            if external_session_id(&summary.source_id, &summary.external_session_id) == session_id {
                return Some((summary.source_id, summary.external_session_id));
            }
        }
    }
    None
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn history_snapshot_requires_unique_source_message_provenance() {
        let event = bcode_session_import::ImportableSessionEvent {
            external_event_id: Some("node-1".into()),
            timestamp_ms: Some(1),
            kind: bcode_session_import::ImportableSessionEventKind::UserMessage {
                text: "historical content".into(),
            },
        };
        let mut snapshot = bcode_session_import::ImportableHistorySnapshot {
            schema_version: 1,
            conversation_id: "conversation".into(),
            title: None,
            selected_node: "leaf".into(),
            revision_id: "revision".into(),
            events: vec![event.clone()],
            message_metadata: std::collections::BTreeMap::new(),
            warnings: vec![],
        };
        assert!(super::validate_history_snapshot(&snapshot, "conversation", None).is_ok());
        snapshot.events.push(event);
        assert!(super::validate_history_snapshot(&snapshot, "conversation", None).is_err());
        snapshot.events.pop();
        for id in [None, Some(String::new()), Some(" ".into())] {
            snapshot.events[0].external_event_id = id;
            assert_eq!(
                super::validate_history_snapshot(&snapshot, "conversation", None),
                Err("history provider returned an incompatible snapshot")
            );
        }
        // A genuinely empty branch is not fabricated into a message.
        snapshot.events.clear();
        assert!(super::validate_history_snapshot(&snapshot, "conversation", None).is_ok());
    }

    #[test]
    fn history_snapshot_rejects_operational_events() {
        use bcode_session_import::{
            ImportableHistorySnapshot, ImportableSessionEvent, ImportableSessionEventKind as Kind,
        };
        let mut snapshot = ImportableHistorySnapshot {
            schema_version: 1,
            conversation_id: "conversation".into(),
            title: None,
            selected_node: "node".into(),
            revision_id: "revision".into(),
            events: vec![],
            message_metadata: std::collections::BTreeMap::new(),
            warnings: vec![],
        };
        for kind in [
            Kind::SystemMessage {
                text: "untrusted".into(),
            },
            Kind::AgentChanged {
                agent_id: "untrusted".into(),
            },
            Kind::ModelChanged {
                provider: "untrusted".into(),
                model: "untrusted".into(),
            },
            Kind::ContextCompacted {
                summary: "untrusted".into(),
            },
            Kind::ToolCallRequested {
                tool_call_id: "call".into(),
                tool_name: "shell".into(),
                arguments_json: "{}".into(),
            },
            Kind::ToolCallFinished {
                tool_call_id: "call".into(),
                result: "untrusted".into(),
                is_error: false,
            },
            Kind::AssistantReasoningMessage {
                text: "untrusted".into(),
            },
            Kind::ModelUsage {
                input_tokens: None,
                output_tokens: None,
                total_tokens: None,
                cached_input_tokens: None,
                cache_write_input_tokens: None,
                reasoning_tokens: None,
            },
        ] {
            snapshot.events = vec![ImportableSessionEvent {
                external_event_id: Some("node".into()),
                timestamp_ms: None,
                kind,
            }];
            assert_eq!(
                super::validate_history_snapshot(&snapshot, "conversation", None),
                Err("history provider returned an incompatible snapshot")
            );
        }
        for kind in [
            Kind::UserMessage {
                text: "historical text".into(),
            },
            Kind::AssistantMessage {
                text: "historical tool activity".into(),
            },
        ] {
            snapshot.events[0].kind = kind;
            assert!(super::validate_history_snapshot(&snapshot, "conversation", None).is_ok());
        }
    }

    #[test]
    fn history_snapshot_metadata_must_reference_imported_nodes() {
        use bcode_session_import::{
            HistoryMessageMetadata, ImportableHistorySnapshot, ImportableSessionEvent,
            ImportableSessionEventKind,
        };
        let metadata = HistoryMessageMetadata {
            message_id: Some("message-distinct-from-node".into()),
            model: None,
            role: "user".into(),
            author: None,
            recipient: None,
            content_type: None,
        };
        let mut snapshot = ImportableHistorySnapshot {
            schema_version: 1,
            conversation_id: "conversation".into(),
            title: None,
            selected_node: "node".into(),
            revision_id: "revision".into(),
            events: vec![ImportableSessionEvent {
                external_event_id: Some("node".into()),
                timestamp_ms: None,
                kind: ImportableSessionEventKind::UserMessage {
                    text: "text".into(),
                },
            }],
            message_metadata: [("node".into(), metadata.clone())].into(),
            warnings: vec![],
        };
        assert!(super::validate_history_snapshot(&snapshot, "conversation", None).is_ok());
        for key in ["", " ", "unknown-node", "message-distinct-from-node"] {
            snapshot
                .message_metadata
                .insert(key.into(), metadata.clone());
            assert_eq!(
                super::validate_history_snapshot(&snapshot, "conversation", None),
                Err("history provider returned an incompatible snapshot")
            );
            snapshot.message_metadata.remove(key);
        }
        snapshot.events.clear();
        assert!(super::validate_history_snapshot(&snapshot, "conversation", None).is_err());
        snapshot.message_metadata.clear();
        assert!(super::validate_history_snapshot(&snapshot, "conversation", None).is_ok());
    }

    #[test]
    fn history_discovery_policy_excludes_disabled_candidates_without_losing_errors() {
        let candidates = std::collections::BTreeMap::from([
            ("openai".to_owned(), Ok(())),
            ("openai-2".to_owned(), Err("unresolved")),
            ("new-profile".to_owned(), Ok(())),
        ]);
        let mut policy = bcode_config::SessionImportConfig::default();
        let mut profiles = candidates.clone();
        super::retain_history_policy_profiles(&mut profiles, &policy);
        assert_eq!(profiles, candidates);
        policy.chatgpt.profiles.insert(
            "openai".into(),
            serde_json::from_value(serde_json::json!({"enabled": false})).unwrap(),
        );
        super::retain_history_policy_profiles(&mut profiles, &policy);
        assert!(!profiles.contains_key("openai"));
        assert_eq!(profiles.get("openai-2"), Some(&Err("unresolved")));
        assert!(profiles.contains_key("new-profile"));
        policy.chatgpt.enabled = false;
        super::retain_history_policy_profiles(&mut profiles, &policy);
        assert!(profiles.is_empty());
        policy.chatgpt.enabled = true;
        policy.enabled = false;
        profiles = candidates;
        super::retain_history_policy_profiles(&mut profiles, &policy);
        assert!(profiles.is_empty());
    }

    #[test]
    fn history_scheme_selection_requires_explicit_provider_support() {
        use bcode_model::history::{HistoryCapabilities, HistoryScopeSupport};
        let capabilities = HistoryCapabilities {
            schema_version: 1,
            auth_schemes: ["chatgpt".to_owned()].into(),
            ordinary: HistoryScopeSupport::Unverified,
            archived: HistoryScopeSupport::Unverified,
            projects: HistoryScopeSupport::Unsupported,
        };
        assert!(super::history_scheme_supported(
            &capabilities,
            Some("chatgpt")
        ));
        for scheme in [None, Some(""), Some(" "), Some("api_key"), Some("CHATGPT")] {
            assert!(!super::history_scheme_supported(&capabilities, scheme));
        }
        let unsupported = HistoryCapabilities {
            auth_schemes: std::collections::BTreeSet::default(),
            ..capabilities
        };
        assert!(!super::history_scheme_supported(
            &unsupported,
            Some("chatgpt")
        ));
    }

    use super::*;

    #[test]
    fn history_auth_admission_rejects_cancelled_or_expired_work() {
        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        let now = std::time::Instant::now();
        let future = now + std::time::Duration::from_secs(30);
        assert!(super::check_history_auth_active(&cancellation, future).is_ok());
        assert_eq!(
            super::check_history_auth_active(&cancellation, now)
                .unwrap_err()
                .message,
            "history authentication timed out; retry or unlock the vault"
        );
        cancellation.cancel();
        for deadline in [now, future] {
            assert_eq!(
                super::check_history_auth_active(&cancellation, deadline)
                    .unwrap_err()
                    .message,
                "history retrieval cancelled"
            );
        }
    }

    #[tokio::test]
    async fn timed_out_history_auth_retains_capacity_until_blocking_work_finishes() {
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let permit = gate.clone().try_acquire_owned().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        let task = tokio::task::spawn_blocking(move || {
            let _ = started_tx.send(());
            let result = release_rx.recv_timeout(std::time::Duration::from_secs(5));
            drop(permit);
            let _ = finished_tx.send(());
            result.map_err(|_| super::HistoryRetrievalError::from("fixture timed out"))
        });
        started_rx.await.unwrap();
        let error = super::await_history_auth(
            task,
            &bcode_plugin_sdk::ServiceCancellation::default(),
            std::time::Duration::ZERO,
        )
        .await
        .unwrap_err();
        assert!(error.message.contains("timed out"));
        assert!(gate.clone().try_acquire_owned().is_err());
        release_tx.send(()).unwrap();
        finished_rx.await.unwrap();
        assert!(gate.try_acquire_owned().is_ok());
    }

    #[tokio::test]
    async fn history_auth_wait_observes_cancellation_deadline_and_success() {
        use std::time::Duration;
        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        let task = tokio::spawn(async { Ok::<_, super::HistoryRetrievalError>(42) });
        assert_eq!(
            super::await_history_auth(task, &cancellation, Duration::from_secs(1))
                .await
                .unwrap(),
            42
        );
        let task = tokio::spawn(std::future::pending::<
            Result<(), super::HistoryRetrievalError>,
        >());
        let error = super::await_history_auth(task, &cancellation, Duration::ZERO)
            .await
            .unwrap_err();
        assert_eq!(
            error.message,
            "history authentication timed out; retry or unlock the vault"
        );
        let task = tokio::spawn(std::future::pending::<
            Result<(), super::HistoryRetrievalError>,
        >());
        let trigger = cancellation.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            trigger.cancel();
        });
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            super::await_history_auth(task, &cancellation, Duration::from_secs(10)),
        )
        .await
        .unwrap();
        assert_eq!(result.unwrap_err().message, "history retrieval cancelled");
    }

    #[test]
    fn history_policy_fails_closed_for_unavailable_and_disabled_configuration() {
        let config = bcode_config::BcodeConfig::default();
        assert!(super::history_enabled_config(Ok(config.clone()), "profile").is_ok());
        let mut disabled = config;
        disabled.session_import.enabled = false;
        assert_eq!(
            super::history_enabled_config(Ok(disabled), "profile")
                .unwrap_err()
                .message,
            "history synchronization is disabled for this profile"
        );
        let error = super::history_enabled_config(
            Err(bcode_config::ConfigError::Composition {
                message: "private-path".into(),
            }),
            "profile",
        )
        .unwrap_err();
        assert_eq!(error.message, "history configuration is unavailable");
        assert!(!format!("{error:?}").contains("private"));
    }

    #[test]
    fn history_pages_reject_invalid_boundaries_without_exposing_content() {
        use bcode_model::history::{HistoryPageEntry, ListHistoryPageResponse};
        let entry = HistoryPageEntry {
            conversation_id: "private-id".into(),
            title: Some("private-title".into()),
            updated_at: Some(123.0),
        };
        let valid = ListHistoryPageResponse {
            schema_version: 1,
            entries: vec![entry.clone()],
            next_offset: Some(11),
        };
        assert!(super::validate_history_page(&valid, 10, 1).is_ok());
        let mut invalid = Vec::new();
        let mut page = valid.clone();
        page.schema_version = 2;
        invalid.push(page);
        for next in [0, 10] {
            let mut page = valid.clone();
            page.next_offset = Some(next);
            invalid.push(page);
        }
        let mut page = valid.clone();
        page.entries.clear();
        invalid.push(page);
        let mut page = valid.clone();
        page.entries.push(entry);
        invalid.push(page);
        let mut page = valid.clone();
        page.entries[0].conversation_id = " \t".into();
        invalid.push(page);
        for timestamp in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            let mut page = valid.clone();
            page.entries[0].updated_at = Some(timestamp);
            invalid.push(page);
        }
        for page in invalid {
            let error = super::validate_history_page(&page, 10, 50).unwrap_err();
            assert_eq!(
                error.message,
                "history provider returned an incompatible page"
            );
            assert!(!format!("{error:?}").contains("private"));
        }
        assert!(super::validate_history_page(&valid, 10, 0).is_err());
        let empty = ListHistoryPageResponse {
            entries: Vec::new(),
            next_offset: None,
            ..valid
        };
        assert!(super::validate_history_page(&empty, u64::MAX, 50).is_ok());
        assert!(super::validate_history_page(&empty, 0, 0).is_err());
    }

    #[test]
    fn history_retry_metadata_is_versioned_and_never_successful_content() {
        for (payload, expected) in [
            (r#"{"schema_version":1,"retry_after_seconds":37}"#, Some(37)),
            (r#"{"schema_version":1,"retry_after_seconds":0}"#, Some(0)),
            (r#"{"schema_version":2,"retry_after_seconds":37}"#, None),
            (r#"{"schema_version":1}"#, None),
            ("private-malformed-payload", None),
        ] {
            let mut response = bcode_plugin_sdk::ServiceResponse::error(
                "history_rate_limited",
                "private-provider-message",
            );
            response.payload = payload.as_bytes().to_vec();
            let error = decode_history_response::<serde_json::Value>(response).unwrap_err();
            assert_eq!(error.retry_after_seconds, expected);
            assert_eq!(error.message, "history rate limited; retry later");
            assert!(!format!("{error:?}").contains("private"));
        }
        let mut response =
            bcode_plugin_sdk::ServiceResponse::error("history_auth_required", "private");
        response.payload = br#"{"schema_version":1,"retry_after_seconds":37}"#.to_vec();
        assert_eq!(
            decode_history_response::<serde_json::Value>(response)
                .unwrap_err()
                .retry_after_seconds,
            None
        );
    }

    #[test]
    fn history_errors_preserve_actionable_categories_without_provider_messages() {
        let cases = [
            (
                "history_auth_required",
                "history authentication required; reconnect this profile",
            ),
            (
                "history_refresh_required",
                "history credentials require provider-owned refresh",
            ),
            ("history_rate_limited", "history rate limited; retry later"),
            ("history_cancelled", "history retrieval cancelled"),
            ("cancelled", "history retrieval cancelled"),
            (
                "history_access_denied",
                "history access denied for this profile",
            ),
            (
                "history_access_challenge",
                "history access requires an upstream challenge",
            ),
            (
                "history_not_found",
                "remote history unavailable; retain imported history",
            ),
            (
                "history_too_large",
                "history exceeds retrieval budget; import remains incomplete",
            ),
            ("private-unknown-code", "history provider retrieval failed"),
        ];
        for (code, expected) in cases {
            let error = bcode_plugin::PluginServiceCallError::Service {
                code: code.into(),
                message: "private-token-and-transcript".into(),
            };
            assert_eq!(history_retrieval_error(&error), expected);
            assert!(!history_retrieval_error(&error).contains("private"));
        }
    }

    #[test]
    fn history_snapshot_validation_rejects_wrong_identity_branch_and_version() {
        let mut snapshot = bcode_session_import::ImportableHistorySnapshot {
            schema_version: 1,
            conversation_id: "conversation".into(),
            title: None,
            selected_node: "leaf".into(),
            revision_id: "revision".into(),
            events: Vec::new(),
            message_metadata: std::collections::BTreeMap::new(),
            warnings: Vec::new(),
        };
        assert!(validate_history_snapshot(&snapshot, "conversation", None).is_ok());
        assert!(validate_history_snapshot(&snapshot, "conversation", Some("leaf")).is_ok());
        assert!(validate_history_snapshot(&snapshot, "other", None).is_err());
        assert!(validate_history_snapshot(&snapshot, "conversation", Some("other")).is_err());
        snapshot.schema_version = 2;
        assert!(validate_history_snapshot(&snapshot, "conversation", None).is_err());
        snapshot.schema_version = 1;
        snapshot.revision_id.clear();
        assert!(validate_history_snapshot(&snapshot, "conversation", None).is_err());
    }

    #[test]
    fn import_configuration_errors_fail_closed_without_exposing_details() {
        let error = require_import_enabled(Err(bcode_config::ConfigError::Composition {
            message: "private-configuration-detail".into(),
        }))
        .unwrap_err();
        let response = import_error_response(&error);
        assert_eq!(response.code, "session_import_configuration_unavailable");
        assert!(!response.message.contains("private-configuration-detail"));
        let mut config = bcode_config::BcodeConfig::default();
        assert!(require_import_enabled(Ok(config.clone())).is_ok());
        config.session_import.enabled = false;
        assert_eq!(
            require_import_enabled(Ok(config)).unwrap_err(),
            "session import is disabled"
        );
    }

    #[test]
    fn import_errors_are_stable_and_secret_safe() {
        for (detail, code, message) in [
            (
                "session import is disabled",
                "session_import_disabled",
                "session import is disabled",
            ),
            (
                "no session import providers are loaded",
                "session_import_unavailable",
                "session import provider is unavailable",
            ),
            (
                "external session not found",
                "external_session_not_found",
                "external session was not found",
            ),
            (
                "secret-plugin-or-session-detail",
                "import_failed",
                "session import failed",
            ),
        ] {
            let error = import_error_response(detail);
            assert_eq!(error.code, code);
            assert_eq!(error.message, message);
            assert!(!error.message.contains("secret-plugin-or-session-detail"));
        }
    }

    #[test]
    fn imported_tool_result_preserves_typed_text_fallback_without_presentation() {
        let mut compacted_through_sequence = 0;
        let kind = imported_session_event_kind(
            ImportableSessionEventKind::ToolCallFinished {
                tool_call_id: "imported-call".to_owned(),
                result: "imported tool output".to_owned(),
                is_error: false,
            },
            &mut compacted_through_sequence,
        );

        let SessionEventKind::ToolInvocationResultRecorded { record } = kind else {
            panic!("tool completion must import as a canonical result")
        };
        assert_eq!(record.invocation_id, "imported-call");
        assert_eq!(record.model_output, "imported tool output");
        assert_eq!(record.presentation, None);
        assert_eq!(
            record.result,
            Some(bcode_session_models::ToolInvocationResult::Text {
                text: "imported tool output".to_owned(),
            })
        );
    }
}
