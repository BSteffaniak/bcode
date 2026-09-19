//! Pinned request-only context for isolated, tool-free generation.
use super::{
    Arc, ErrorResponse, Response, ResponsePayload, ServerError, ServerState, SessionEventKind,
    SessionId, SharedWriter, ambiguous_session_location_response, send_response,
};

#[derive(Debug)]
pub struct CapturedContext {
    pub session_id: SessionId,
    pub events: Arc<Vec<bcode_session_models::SessionEvent>>,
    pub created: std::time::Instant,
}
#[cfg(test)]
#[path = "context_generation_tests.rs"]
mod tests;

const MAX_CAPTURES: usize = 32;
const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;
const LIFETIME: std::time::Duration = std::time::Duration::from_mins(10);

pub async fn handle(
    request_id: u64,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    request: bcode_session_models::PrepareContextGeneration,
) -> Result<(), ServerError> {
    if let Some(response) =
        ambiguous_session_location_response(state, request.source_session_id).await
    {
        return send_response(writer, request_id, response).await;
    }
    let result = prepare(state, request).await;
    let response = match result {
        Ok(prepared) => Response::Ok(ResponsePayload::PreparedContextGeneration { prepared }),
        Err(message) => Response::Err(ErrorResponse::new(
            "generation_context_unavailable",
            message,
        )),
    };
    send_response(writer, request_id, response).await
}

/// Capture a stable bounded source prefix without copying it into destination history.
pub async fn prepare(
    state: &ServerState,
    request: bcode_session_models::PrepareContextGeneration,
) -> Result<bcode_session_models::PreparedContextGeneration, &'static str> {
    if request.version != 1 || request.name.is_empty() || request.name.len() > 256 {
        return Err("Invalid context generation request");
    }
    let _ownership = state
        .sessions
        .acquire_session_ownership(
            request.source_session_id,
            bcode_session::SessionOwnershipKind::RuntimeWork,
        )
        .await
        .map_err(|_| "Source session unavailable")?;
    let before = state
        .sessions
        .session_derivation_snapshot(request.source_session_id)
        .await
        .map_err(|_| "Source snapshot unavailable")?;
    let events = state
        .sessions
        .model_context_events(request.source_session_id)
        .await
        .map_err(|_| "Source model context unavailable")?;
    let after = state
        .sessions
        .session_derivation_snapshot(request.source_session_id)
        .await
        .map_err(|_| "Source snapshot unavailable")?;
    if before.generation != after.generation {
        return Err("Source changed while capturing context; retry generation");
    }
    if events.iter().any(|event| {
        matches!(
            event.kind,
            SessionEventKind::ProviderContextCompacted { .. }
        )
    }) {
        return Err(
            "Source uses opaque provider context that cannot be safely transferred; compact to portable context first",
        );
    }
    if serde_json::to_vec(&events)
        .map_err(|_| "Context encoding failed")?
        .len()
        > MAX_CAPTURE_BYTES
    {
        return Err("Source context exceeds capture allowance; compact the source session first");
    }
    let mut captures = state.generation_contexts.lock().await;
    captures.retain(|_, capture| capture.created.elapsed() < LIFETIME);
    if captures.len() >= MAX_CAPTURES {
        return Err("Too many pending context generations; retry later");
    }
    let session = state
        .sessions
        .create_session(Some(request.name), before.working_directory.clone())
        .await
        .map_err(|_| "Could not create generation session")?;
    let context_id = uuid::Uuid::new_v4().to_string();
    captures.insert(
        context_id.clone(),
        CapturedContext {
            session_id: session.id,
            events: Arc::new(events),
            created: std::time::Instant::now(),
        },
    );
    drop(captures);
    Ok(bcode_session_models::PreparedContextGeneration {
        session_id: session.id,
        source: before,
        context_id,
    })
}

pub async fn events(
    state: &ServerState,
    session_id: SessionId,
    execution: &bcode_session_models::TurnExecutionOptions,
) -> Result<Option<Arc<Vec<bcode_session_models::SessionEvent>>>, bcode_session::SessionError> {
    let Some(id) = &execution.request_context_id else {
        return Ok(None);
    };
    let invalid = || {
        bcode_session::SessionError::EventSerialization(
            "Request-only source context unavailable; regenerate from the source session".into(),
        )
    };
    if execution.tools != bcode_session_models::TurnToolPolicy::Disabled
        || execution.structured_output.is_none()
    {
        return Err(invalid());
    }
    let mut captures = state.generation_contexts.lock().await;
    captures.retain(|_, capture| capture.created.elapsed() < LIFETIME);
    let capture = captures
        .get(id)
        .filter(|capture| capture.session_id == session_id && capture.created.elapsed() < LIFETIME)
        .ok_or_else(invalid)?;
    let events = Arc::clone(&capture.events);
    drop(captures);
    Ok(Some(events))
}
