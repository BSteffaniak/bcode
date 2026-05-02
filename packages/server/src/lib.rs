#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Local Bcode daemon runtime.

use bcode_ipc::{
    CodecError, EnvelopeKind, ErrorResponse, Event, IpcEndpoint, LocalIpcListener, LocalIpcStream,
    Request, Response, ResponsePayload, decode, event_envelope, recv_envelope, response_envelope,
    send_envelope,
};
use bcode_session::SessionManager;
use bcode_session_models::{ClientId, SessionId};
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{WriteHalf, split};
use tokio::sync::Mutex;

/// Shared client writer.
type SharedWriter = Arc<Mutex<WriteHalf<LocalIpcStream>>>;

/// Errors returned by the local server.
#[derive(Debug, Error)]
pub enum ServerError {
    #[error("IPC transport error: {0}")]
    Transport(#[from] bcode_ipc::IpcTransportError),
    #[error("IPC codec error: {0}")]
    Codec(#[from] CodecError),
}

/// Run the local Bcode server until interrupted.
///
/// # Errors
///
/// Returns an error when the server cannot bind or accept local IPC connections.
pub async fn run(endpoint: IpcEndpoint) -> Result<(), ServerError> {
    let listener = LocalIpcListener::bind(&endpoint).await?;
    let sessions = Arc::new(SessionManager::default());
    loop {
        let stream = listener.accept().await?;
        let sessions = Arc::clone(&sessions);
        tokio::spawn(async move {
            if let Err(error) = handle_client(stream, sessions).await {
                eprintln!("client connection failed: {error}");
            }
        });
    }
}

async fn handle_client(
    stream: LocalIpcStream,
    sessions: Arc<SessionManager>,
) -> Result<(), ServerError> {
    let client_id = ClientId::new();
    let (mut reader, writer) = split(stream);
    let writer = Arc::new(Mutex::new(writer));
    let mut attached_session = None;

    loop {
        let envelope = match recv_envelope(&mut reader).await {
            Ok(envelope) => envelope,
            Err(CodecError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(error) => return Err(error.into()),
        };

        if envelope.kind != EnvelopeKind::Request {
            continue;
        }

        let request = decode(&envelope.payload)?;
        handle_request(
            request,
            envelope.request_id,
            client_id,
            &sessions,
            &writer,
            &mut attached_session,
        )
        .await?;
    }

    if let Some(session_id) = attached_session {
        sessions.detach_session(session_id, client_id).await;
    }

    Ok(())
}

async fn handle_request(
    request: Request,
    request_id: u64,
    client_id: ClientId,
    sessions: &Arc<SessionManager>,
    writer: &SharedWriter,
    attached_session: &mut Option<SessionId>,
) -> Result<(), ServerError> {
    match request {
        Request::Hello { .. } => handle_hello(request_id, client_id, writer).await,
        Request::Ping => {
            send_response(writer, request_id, Response::Ok(ResponsePayload::Pong)).await
        }
        Request::CreateSession { name } => {
            handle_create_session(request_id, sessions, writer, name).await
        }
        Request::ListSessions => handle_list_sessions(request_id, sessions, writer).await,
        Request::AttachSession { session_id } => {
            handle_attach_session(
                request_id,
                client_id,
                sessions,
                writer,
                attached_session,
                session_id,
            )
            .await
        }
        Request::SendUserMessage { session_id, text } => {
            handle_user_message(request_id, client_id, sessions, writer, session_id, text).await
        }
    }
}

async fn handle_hello(
    request_id: u64,
    client_id: ClientId,
    writer: &SharedWriter,
) -> Result<(), ServerError> {
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::Hello {
            protocol_version: bcode_ipc::ProtocolVersion::current(),
            client_id,
        }),
    )
    .await
}

async fn handle_create_session(
    request_id: u64,
    sessions: &SessionManager,
    writer: &SharedWriter,
    name: Option<String>,
) -> Result<(), ServerError> {
    let session = sessions.create_session(name).await;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::SessionCreated { session }),
    )
    .await
}

async fn handle_list_sessions(
    request_id: u64,
    sessions: &SessionManager,
    writer: &SharedWriter,
) -> Result<(), ServerError> {
    let session_list = sessions.list_sessions().await;
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::SessionList {
            sessions: session_list,
        }),
    )
    .await
}

async fn handle_attach_session(
    request_id: u64,
    client_id: ClientId,
    sessions: &Arc<SessionManager>,
    writer: &SharedWriter,
    attached_session: &mut Option<SessionId>,
    session_id: SessionId,
) -> Result<(), ServerError> {
    match sessions.attach_session(session_id, client_id).await {
        Ok(attachment) => {
            *attached_session = Some(session_id);
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::Attached {
                    session_id,
                    history: attachment.history,
                }),
            )
            .await?;
            forward_session_events(writer.clone(), attachment.events);
            Ok(())
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("session_not_found", error.to_string())),
            )
            .await
        }
    }
}

async fn handle_user_message(
    request_id: u64,
    client_id: ClientId,
    sessions: &SessionManager,
    writer: &SharedWriter,
    session_id: SessionId,
    text: String,
) -> Result<(), ServerError> {
    match sessions
        .append_user_message(session_id, client_id, text)
        .await
    {
        Ok(_) => {
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::MessageSent),
            )
            .await
        }
        Err(error) => {
            send_response(
                writer,
                request_id,
                Response::Err(ErrorResponse::new("session_not_found", error.to_string())),
            )
            .await
        }
    }
}

fn forward_session_events(
    writer: SharedWriter,
    mut events: tokio::sync::broadcast::Receiver<bcode_session_models::SessionEvent>,
) {
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            let envelope = match event_envelope(&Event::Session(event)) {
                Ok(envelope) => envelope,
                Err(error) => {
                    eprintln!("failed to encode session event: {error}");
                    break;
                }
            };
            let mut writer = writer.lock().await;
            if let Err(error) = send_envelope(&mut *writer, &envelope).await {
                eprintln!("failed to send session event: {error}");
                break;
            }
        }
    });
}

async fn send_response(
    writer: &SharedWriter,
    request_id: u64,
    response: Response,
) -> Result<(), ServerError> {
    let envelope = response_envelope(request_id, &response)?;
    let mut writer = writer.lock().await;
    send_envelope(&mut *writer, &envelope).await?;
    drop(writer);
    Ok(())
}
