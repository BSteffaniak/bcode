//! One scoped service socket with bounded same-client cancellation control.

#[cfg(test)]
include!("contextual_tests.rs");

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use super::{BcodeClient, ClientError};
use bcode_ipc::{
    EnvelopeKind, PluginServiceContext, PluginServiceResponse, Request, Response, ResponsePayload,
    decode_response, recv_envelope, request_envelope, send_envelope,
};

type CancelReply = oneshot::Sender<bool>;
#[derive(Debug, Clone, Default)]
pub struct Controls(Arc<Mutex<BTreeMap<String, Option<mpsc::Sender<CancelReply>>>>>);

struct Registration {
    controls: Controls,
    id: String,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(mut entries) = self.controls.0.lock() {
            entries.remove(&self.id);
        }
    }
}

pub async fn cancel(client: &BcodeClient, id: &str) -> Result<bool, ClientError> {
    let sender = client
        .contextual_invocations
        .0
        .lock()
        .ok()
        .and_then(|entries| entries.get(id).cloned().flatten());
    let Some(sender) = sender else {
        return Ok(false);
    };
    let (reply, result) = oneshot::channel();
    if sender.try_send(reply).is_err() {
        return Ok(false);
    }
    Ok(result.await.unwrap_or(false))
}

fn project(response: Response) -> Result<PluginServiceResponse, ClientError> {
    match response {
        Response::Ok(ResponsePayload::PluginServiceResult { response }) => Ok(response),
        Response::Err(error) => Err(ClientError::Server {
            code: error.code,
            message: error.message,
        }),
        Response::Ok(_) => Err(ClientError::UnexpectedResponse),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn invoke(
    client: &BcodeClient,
    context: PluginServiceContext,
    plugin_id: String,
    interface_id: String,
    operation: String,
    payload: Vec<u8>,
) -> Result<PluginServiceResponse, ClientError> {
    let (sender, mut controls) = mpsc::channel(1);
    {
        let mut entries = client
            .contextual_invocations
            .0
            .lock()
            .map_err(|_| ClientError::UnexpectedResponse)?;
        if entries.len() >= 4096
            || entries.values().filter(|entry| entry.is_some()).count() >= 64
            || entries.contains_key(&context.invocation_id)
        {
            return Err(ClientError::UnexpectedResponse);
        }
        entries.insert(context.invocation_id.clone(), Some(sender));
    }
    let registration = Registration {
        controls: client.contextual_invocations.clone(),
        id: context.invocation_id.clone(),
    };
    let timeout = Duration::from_millis(context.deadline_ms).saturating_add(Duration::from_secs(2));
    let work = async {
        let mut connection = client.connect("bcode-plugin-context").await?;
        if let Some(session) = context.workspace.session_id.as_deref() {
            let session = session
                .parse()
                .map_err(|_| ClientError::UnexpectedResponse)?;
            // Attach bounded projection metadata on the same socket that owns the
            // invocation, never full canonical history or another client identity.
            connection.attach_session_recent(session, 1).await?;
        }
        let invoke_id = connection.next_request_id;
        let cancel_id = invoke_id
            .checked_add(1)
            .ok_or(ClientError::UnexpectedResponse)?;
        let request = Request::InvokePluginServiceContextual {
            context,
            plugin_id,
            interface_id,
            operation,
            payload,
        };
        let (mut reader, mut writer) = tokio::io::split(connection.stream);
        send_envelope(&mut writer, &request_envelope(invoke_id, &request)?).await?;
        let mut cancel_reply = None;
        let mut cancel_sent = false;
        let mut terminal = None;
        loop {
            // Keep this future alive when control arrives: frame reads are not
            // cancellation-safe after partially receiving a chunked envelope.
            let receive = recv_envelope(&mut reader);
            tokio::pin!(receive);
            let envelope = loop {
                tokio::select! {
                    result = &mut receive => break result?,
                    Some(reply) = controls.recv(), if !cancel_sent => {
                        cancel_sent = true;
                        cancel_reply = Some(reply);
                        let request = Request::CancelPluginServiceInvocation { invocation_id: registration.id.clone() };
                        send_envelope(&mut writer, &request_envelope(cancel_id, &request)?).await?;
                    }
                }
            };
            if envelope.kind == EnvelopeKind::Event {
                continue;
            }
            if envelope.kind != EnvelopeKind::Response {
                return Err(ClientError::UnexpectedResponse);
            }
            let response = decode_response(&envelope.payload)?;
            if envelope.request_id == cancel_id && cancel_sent {
                let accepted = matches!(
                    response,
                    Response::Ok(ResponsePayload::PluginServiceInvocationCancelled {
                        accepted: true
                    })
                );
                if let Some(reply) = cancel_reply.take() {
                    let _sent = reply.send(accepted);
                }
                if let Some(response) = terminal.take() {
                    return project(response);
                }
                continue;
            }
            if envelope.request_id != invoke_id {
                return Err(ClientError::UnexpectedResponse);
            }
            if cancel_reply.is_some() {
                terminal = Some(response);
                continue;
            }
            return project(response);
        }
    };
    tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| ClientError::RequestTimeout { timeout })?
}
