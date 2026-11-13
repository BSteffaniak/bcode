//! Request-only turn context held in daemon memory.
//!
//! An application may mark context blocks request-only. Admission moves them out of the
//! admission metadata into this store and records only an opaque handle, so session history never
//! holds them. The turn reads them back while it assembles model requests; when the daemon no
//! longer holds them (it restarted, or they expired), the turn fails closed instead of running
//! without them. Entries are bounded in number and lifetime.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use bcode_session_models::{
    SessionId, TurnAdmissionMetadata, TurnAdmissionMetadataError, TurnContextBlock,
    TurnContextError,
};

/// Request-only contexts held at once; the oldest is dropped beyond this.
const MAX_RETAINED_CONTEXTS: usize = 256;
/// How long a request-only context waits for its turn, including time queued.
const RETAINED_CONTEXT_LIFETIME: Duration = Duration::from_hours(1);

#[derive(Debug)]
struct Retained {
    session_id: SessionId,
    blocks: Vec<TurnContextBlock>,
    created: Instant,
}

#[derive(Debug, Default)]
struct Store {
    entries: BTreeMap<String, Retained>,
    order: VecDeque<String>,
}

impl Store {
    fn expire(&mut self) {
        while let Some(oldest) = self.order.front() {
            let expired = self
                .entries
                .get(oldest)
                .is_none_or(|entry| entry.created.elapsed() >= RETAINED_CONTEXT_LIFETIME);
            if !expired && self.order.len() <= MAX_RETAINED_CONTEXTS {
                break;
            }
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }
}

/// Daemon-memory store of request-only turn context.
#[derive(Debug, Default)]
pub struct RequestOnlyContexts {
    inner: StdMutex<Store>,
}

impl RequestOnlyContexts {
    fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        self.inner
            .lock()
            .expect("request-only context store lock poisoned")
    }

    /// Move the admission's request-only blocks into memory and record their handle.
    ///
    /// Applications never set the handle themselves; one supplied with a submission is rejected.
    pub fn retain(
        &self,
        session_id: SessionId,
        admission: &mut TurnAdmissionMetadata,
    ) -> Result<(), TurnAdmissionMetadataError> {
        if admission.context.request_only_handle.is_some() {
            return Err(TurnAdmissionMetadataError::InvalidContext(
                TurnContextError::InvalidHandle,
            ));
        }
        admission
            .context
            .validate()
            .map_err(TurnAdmissionMetadataError::InvalidContext)?;
        if admission.context.request_only.is_empty() {
            return Ok(());
        }
        let handle = uuid::Uuid::new_v4().to_string();
        let blocks = std::mem::take(&mut admission.context.request_only);
        let mut store = self.lock();
        store.entries.insert(
            handle.clone(),
            Retained {
                session_id,
                blocks,
                created: Instant::now(),
            },
        );
        store.order.push_back(handle.clone());
        store.expire();
        drop(store);
        admission.context.request_only_handle = Some(handle);
        Ok(())
    }

    /// The blocks held under `handle` for `session_id`, when the daemon still holds them.
    pub fn blocks(&self, session_id: SessionId, handle: &str) -> Option<Vec<TurnContextBlock>> {
        let mut store = self.lock();
        store.expire();
        store
            .entries
            .get(handle)
            .filter(|entry| entry.session_id == session_id)
            .map(|entry| entry.blocks.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::TurnContext;

    #[test]
    fn retained_blocks_leave_the_admission_and_stay_with_their_session() {
        let contexts = RequestOnlyContexts::default();
        let session = SessionId::new();
        let mut admission = TurnAdmissionMetadata {
            context: TurnContext {
                blocks: vec![TurnContextBlock::new("durable", "kept in history")],
                ..TurnContext::request_only(vec![TurnContextBlock::new("screen", "rows")])
            },
            ..TurnAdmissionMetadata::default()
        };
        contexts.retain(session, &mut admission).expect("retain");
        assert_eq!(admission.context.request_only.len(), 0);
        assert_eq!(admission.context.blocks.len(), 1);
        assert_eq!(admission.validate_durable(), Ok(()));
        let handle = admission
            .context
            .request_only_handle
            .clone()
            .expect("handle");
        assert_eq!(
            contexts.blocks(session, &handle),
            Some(vec![TurnContextBlock::new("screen", "rows")])
        );
        assert_eq!(contexts.blocks(SessionId::new(), &handle), None);
    }

    #[test]
    fn application_supplied_handles_are_rejected() {
        let contexts = RequestOnlyContexts::default();
        let mut admission = TurnAdmissionMetadata::default();
        admission.context.request_only_handle = Some("forged".into());
        assert_eq!(
            contexts.retain(SessionId::new(), &mut admission),
            Err(TurnAdmissionMetadataError::InvalidContext(
                TurnContextError::InvalidHandle
            ))
        );
    }

    #[test]
    fn the_store_stays_bounded() {
        let contexts = RequestOnlyContexts::default();
        let session = SessionId::new();
        let mut first = None;
        for index in 0..=MAX_RETAINED_CONTEXTS {
            let mut admission = TurnAdmissionMetadata {
                context: TurnContext::request_only(vec![TurnContextBlock::new(
                    "n",
                    index.to_string(),
                )]),
                ..TurnAdmissionMetadata::default()
            };
            contexts.retain(session, &mut admission).expect("retain");
            first.get_or_insert_with(|| admission.context.request_only_handle.expect("handle"));
        }
        assert_eq!(contexts.blocks(session, &first.expect("first")), None);
        assert_eq!(contexts.lock().entries.len(), MAX_RETAINED_CONTEXTS);
    }
}
