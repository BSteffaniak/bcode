//! Dedicated, handle-bound activity observation connections; never request lanes.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use bcode_session_models::{
    SessionActivityWatchDelivery as Delivery, SessionActivityWatchHandle as Handle,
    SessionActivityWatchScope as Scope, SessionActivityWatchUpdate as Update, SessionId,
    application::SessionApplicationFailure as Failure,
};
use tokio::sync::watch;

use crate::{BcodeClient, ClientConnection};

const MAX_WATCHES: usize = 64;

#[derive(Debug, Default)]
pub struct ActivityWatches {
    entries: Mutex<BTreeMap<SessionId, Entry>>,
}

#[derive(Debug)]
struct Entry {
    handle: Option<Handle>,
    connection: Option<ClientConnection>,
    closed: watch::Sender<bool>,
    sequence: u64,
}

fn failure(code: &str, message: &str) -> Failure {
    Failure {
        code: code.into(),
        message: message.into(),
    }
}

fn closed() -> Failure {
    failure(
        "activity_watch_closed",
        "Activity watch is closed or belongs to another client context.",
    )
}

/// Owns the socket outside the registry across every await. Dropping an uncertain
/// operation removes its handle and disconnects, including partially written frames.
struct Lease {
    registry: Arc<ActivityWatches>,
    key: SessionId,
    connection: Option<ClientConnection>,
    cancelled: watch::Receiver<bool>,
    complete: bool,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self.complete {
            let mut entries = self
                .registry
                .entries
                .lock()
                .expect("activity watch lock poisoned");
            if let Some(entry) = entries.remove(&self.key) {
                entry.closed.send_replace(true);
            }
        }
    }
}

impl ActivityWatches {
    fn reserve(self: &Arc<Self>) -> Result<Lease, Failure> {
        let mut entries = self.entries.lock().expect("activity watch lock poisoned");
        if entries.len() >= MAX_WATCHES {
            return Err(failure(
                "activity_watch_capacity",
                "Client activity watch admission is full.",
            ));
        }
        let key = SessionId::new();
        let (closed, cancelled) = watch::channel(false);
        entries.insert(
            key,
            Entry {
                handle: None,
                connection: None,
                closed,
                sequence: 0,
            },
        );
        drop(entries);
        Ok(Lease {
            registry: Arc::clone(self),
            key,
            connection: None,
            cancelled,
            complete: false,
        })
    }

    fn checkout(self: &Arc<Self>, handle: Handle) -> Result<(Lease, u64), Failure> {
        let mut entries = self.entries.lock().expect("activity watch lock poisoned");
        let (key, entry) = entries
            .iter_mut()
            .find(|(_, entry)| entry.handle == Some(handle))
            .ok_or_else(closed)?;
        let connection = entry.connection.take().ok_or_else(|| {
            failure(
                "activity_watch_busy",
                "Only one activity watch pull may be outstanding.",
            )
        })?;
        let key = *key;
        let cancelled = entry.closed.subscribe();
        let sequence = entry.sequence;
        drop(entries);
        Ok((
            Lease {
                registry: Arc::clone(self),
                key,
                connection: Some(connection),
                cancelled,
                complete: false,
            },
            sequence,
        ))
    }

    fn remove(&self, handle: Handle) -> Option<Entry> {
        let mut entries = self.entries.lock().expect("activity watch lock poisoned");
        let key = entries
            .iter()
            .find(|(_, entry)| entry.handle == Some(handle))
            .map(|(key, _)| *key)?;
        let entry = entries.remove(&key)?;
        drop(entries);
        entry.closed.send_replace(true);
        Some(entry)
    }
}

impl Lease {
    fn finish(mut self, handle: Handle, sequence: u64) -> Result<(), Failure> {
        {
            let mut entries = self
                .registry
                .entries
                .lock()
                .expect("activity watch lock poisoned");
            if sequence == 0
                && entries.iter().any(|(key, entry)| {
                    *key != self.key
                        && entry
                            .handle
                            .is_some_and(|existing| existing.watch_id == handle.watch_id)
                })
            {
                return Err(failure(
                    "activity_watch_invalid_handle",
                    "Daemon reused an active activity watch identity.",
                ));
            }
            let entry = entries.get_mut(&self.key).ok_or_else(closed)?;
            entry.handle = Some(handle);
            entry.sequence = sequence;
            entry.connection = self.connection.take();
            drop(entries);
        }
        self.complete = true;
        Ok(())
    }
}

impl BcodeClient {
    pub(super) async fn open_activity_watch_connection(
        &self,
        scope: Scope,
    ) -> Result<Handle, Failure> {
        let mut lease = self.activity_watches.reserve()?;
        let mut connection = self
            .connect("bcode-activity-watch")
            .await
            .map_err(crate::session_application_failure)?;
        // Watch lifetimes are non-resumable. Do not retain a client/registry cycle.
        connection.reconnect_client = None;
        lease.connection = Some(connection);
        let result = tokio::time::timeout(
            self.request_timeout,
            exchange(
                lease.connection.as_mut().ok_or_else(closed)?,
                crate::Request::OpenSessionActivityWatch { scope },
            ),
        )
        .await
        .map_err(|_| {
            crate::session_application_failure(crate::ClientError::RequestTimeout {
                timeout: self.request_timeout,
            })
        })?
        .map_err(crate::session_application_failure)?;
        let crate::ResponsePayload::SessionActivityWatchOpened { handle } = result else {
            return Err(crate::session_application_failure(
                crate::ClientError::UnexpectedResponse,
            ));
        };
        lease.finish(handle, 0)?;
        Ok(handle)
    }

    pub(super) async fn pull_activity_watch_connection(
        &self,
        handle: Handle,
    ) -> Result<Delivery, Failure> {
        let (mut lease, sequence) = self.activity_watches.checkout(handle)?;
        let connection = lease.connection.as_mut().ok_or_else(closed)?;
        // No response deadline: unchanged state may legitimately wait indefinitely.
        // Writes remain bounded. A cancelled pull drops this lease and its socket.
        let result = tokio::select! {
            biased;
            _ = lease.cancelled.changed() => return Err(closed()),
            result = exchange(connection, crate::Request::PullSessionActivityWatch { handle }) => result,
        }.map_err(crate::session_application_failure)?;
        let crate::ResponsePayload::SessionActivityWatchUpdated { delivery } = result else {
            return Err(crate::session_application_failure(
                crate::ClientError::UnexpectedResponse,
            ));
        };
        if delivery.handle != handle
            || delivery.sequence != sequence
            || (sequence == 0 && delivery.update != Update::Initial)
            || (sequence != 0 && delivery.update == Update::Initial)
        {
            return Err(failure(
                "activity_watch_invalid_delivery",
                "Activity watch identity or delivery order changed; reopen the watch.",
            ));
        }
        let next = sequence.checked_add(1).ok_or_else(closed)?;
        lease.finish(handle, next)?;
        Ok(*delivery)
    }

    pub(super) async fn close_activity_watch_connection(
        &self,
        handle: Handle,
    ) -> Result<bool, Failure> {
        let Some(mut entry) = self.activity_watches.remove(handle) else {
            return Ok(false);
        };
        let Some(mut connection) = entry.connection.take() else {
            // A live pull owns the socket. The close signal wakes and drops it;
            // disconnect removes the server handle without acquiring execution authority.
            return Ok(true);
        };
        let result = tokio::time::timeout(
            self.request_timeout,
            exchange(
                &mut connection,
                crate::Request::CloseSessionActivityWatch { handle },
            ),
        )
        .await
        .map_err(|_| {
            crate::session_application_failure(crate::ClientError::RequestTimeout {
                timeout: self.request_timeout,
            })
        })?
        .map_err(crate::session_application_failure)?;
        match result {
            crate::ResponsePayload::SessionActivityWatchClosed { closed } => Ok(closed),
            _ => Err(crate::session_application_failure(
                crate::ClientError::UnexpectedResponse,
            )),
        }
    }
}

/// Strict framing; neither event buffering nor reconnect-and-restore is valid here.
async fn exchange(
    connection: &mut ClientConnection,
    request: crate::Request,
) -> Result<crate::ResponsePayload, crate::ClientError> {
    let id = connection.next_request_id;
    connection.next_request_id = id.checked_add(1).ok_or_else(|| {
        crate::ClientError::Protocol("activity watch request ID exhausted".into())
    })?;
    let envelope = crate::request_envelope(id, &request)?;
    tokio::time::timeout(
        connection.request_timeout,
        crate::send_envelope(&mut connection.stream, &envelope),
    )
    .await
    .map_err(|_| crate::ClientError::RequestTimeout {
        timeout: connection.request_timeout,
    })??;
    let envelope = crate::recv_envelope(&mut connection.stream).await?;
    if envelope.kind != crate::EnvelopeKind::Response || envelope.request_id != id {
        return Err(crate::ClientError::UnexpectedResponse);
    }
    match crate::decode_response(&envelope.payload)? {
        crate::Response::Ok(payload) => Ok(payload),
        crate::Response::Err(error) => Err(error.into()),
    }
}

#[cfg(all(test, unix))]
mod tests;
