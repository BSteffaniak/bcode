//! Session import orchestration for the local server.

use crate::{ErrorResponse, ServerError, ServerState, send_response};
use bcode_ipc::LocalIpcStream;
use bcode_ipc::{Response, ResponsePayload, SessionImportWarning};
use bcode_session_import::{
    DiscoverImportableSessionsRequest, DiscoverImportableSessionsResponse, ImportableSessionEvent,
    ImportableSessionEventKind, LoadImportableSessionRequest, OP_DISCOVER_IMPORTABLE_SESSIONS,
    OP_LOAD_IMPORTABLE_SESSION, SESSION_IMPORT_INTERFACE_ID,
};
use bcode_session_models::{
    ClientId, SessionEventKind, SessionEventProvenance, SessionId, SessionImportSummary,
};
use sha2::{Digest as _, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::WriteHalf;
use tokio::sync::Mutex;
use uuid::Uuid;

/// Shared client writer.
type SharedWriter = Arc<Mutex<WriteHalf<LocalIpcStream>>>;

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
) -> Result<(SessionId, Vec<bcode_session_import::ImportWarning>), String> {
    if !bcode_config::load_config().map_or(true, |config| config.session_import.enabled) {
        return Err("session import is disabled".to_string());
    }
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
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let events = importable
            .events
            .into_iter()
            .scan(0_u64, |compacted_through_sequence, event| {
                let provenance = import_event_provenance(&event, &importable.summary.locator);
                let kind = match event.kind {
                    ImportableSessionEventKind::UserMessage { text } => {
                        SessionEventKind::UserMessage {
                            client_id: ClientId::new(),
                            text,
                        }
                    }
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
                        tool_name,
                        arguments_json,
                    },
                    ImportableSessionEventKind::ToolCallFinished {
                        tool_call_id,
                        result,
                        is_error,
                    } => SessionEventKind::ToolCallFinished {
                        tool_call_id,
                        result,
                        is_error,
                        output: None,
                        semantic_result: None,
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
                        },
                    },
                    ImportableSessionEventKind::ModelChanged { provider, model } => {
                        SessionEventKind::ModelChanged { provider, model }
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
                };
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
) -> Result<(), ServerError> {
    match import_external_session(state, source_id, external_session_id).await {
        Ok((session_id, warnings)) => {
            let session = state.sessions.session_summary(session_id).await?;
            state
                .session_catalog
                .upsert_native_session(session.clone())
                .await;
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::ExternalSessionImported {
                    session,
                    warnings: warnings.into_iter().map(import_warning_to_ipc).collect(),
                }),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("import_failed", error)),
            )
            .await
        }
    }
}

fn import_warning_to_ipc(warning: bcode_session_import::ImportWarning) -> SessionImportWarning {
    SessionImportWarning {
        code: warning.code,
        message: warning.message,
        count: warning.count,
    }
}

/// Resolve a synthetic external session ID into an imported native Bcode session ID.
pub async fn resolve_attach_session_id(
    state: &Arc<ServerState>,
    session_id: SessionId,
) -> SessionId {
    if state.sessions.session_summary(session_id).await.is_ok() {
        return session_id;
    }
    let Some((source_id, external_session_id)) =
        external_parts_from_session_id(state, session_id).await
    else {
        return session_id;
    };
    match import_external_session(state, &source_id, &external_session_id).await {
        Ok((imported_session_id, warnings)) => {
            if let Ok(session) = state.sessions.session_summary(imported_session_id).await {
                state.session_catalog.upsert_native_session(session).await;
            }
            if !warnings.is_empty() {
                eprintln!(
                    "imported [{source_id}] session with {} warnings",
                    warnings.len()
                );
                for warning in warnings {
                    eprintln!("import warning: {}: {}", warning.code, warning.message);
                }
            }
            imported_session_id
        }
        Err(error) => {
            eprintln!(
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
