//! Async adapter for blocking session store operations.

use super::{
    SessionId, SessionState, SessionStore, SessionStoreError, lease::SessionLeaseOwnerContext,
    spawn_blocking,
};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone)]
pub struct SessionStoreExecutor {
    store: SessionStore,
}

impl SessionStoreExecutor {
    pub fn root_path(&self) -> PathBuf {
        self.store.root().to_path_buf()
    }

    pub const fn new(store: SessionStore) -> Self {
        Self { store }
    }

    pub(crate) fn metrics(&self) -> bcode_metrics::MetricsRegistry {
        self.store.metrics.clone()
    }

    pub(crate) const fn lease_owner(&self) -> &SessionLeaseOwnerContext {
        self.store.lease_owner()
    }

    pub async fn load_catalog(
        &self,
    ) -> Result<BTreeMap<SessionId, SessionState>, SessionStoreError> {
        let store = self.store.clone();
        spawn_blocking(move || store.load_catalog()).await?
    }

    pub(crate) async fn write_session_manifest(
        &self,
        summary: bcode_session_models::SessionSummary,
    ) -> Result<(), SessionStoreError> {
        let store = self.store.clone();
        spawn_blocking(move || store.write_session_manifest(&summary)).await?
    }
}
