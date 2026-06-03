//! Async adapter for blocking session store operations.

use super::{SessionEventStore, SessionState, SessionStoreError, spawn_blocking};
use bcode_session_models::SessionId;
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone)]
pub struct SessionStoreExecutor {
    store: SessionEventStore,
}

impl SessionStoreExecutor {
    pub fn root_path(&self) -> PathBuf {
        self.store.root().to_path_buf()
    }

    pub const fn new(store: SessionEventStore) -> Self {
        Self { store }
    }

    pub(crate) fn metrics(&self) -> bcode_metrics::MetricsRegistry {
        self.store.metrics.clone()
    }

    pub async fn load_catalog(
        &self,
    ) -> Result<BTreeMap<SessionId, SessionState>, SessionStoreError> {
        let store = self.store.clone();
        spawn_blocking(move || store.load_catalog()).await?
    }

    pub async fn delete(&self, session_id: SessionId) -> Result<(), SessionStoreError> {
        let store = self.store.clone();
        spawn_blocking(move || store.delete(session_id)).await?
    }
}
