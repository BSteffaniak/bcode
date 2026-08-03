//! Async adapter for blocking session store operations.

use super::{
    SessionId, SessionState, SessionStore, SessionStoreError,
    catalog_updates::CatalogUpdateCoordinator, lease::SessionLeaseOwnerContext, spawn_blocking,
};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone)]
pub struct SessionStoreExecutor {
    store: SessionStore,
    catalog_updates: CatalogUpdateCoordinator,
}

impl SessionStoreExecutor {
    pub fn root_path(&self) -> PathBuf {
        self.store.root().to_path_buf()
    }

    pub fn new(store: SessionStore) -> Self {
        let catalog_updates =
            CatalogUpdateCoordinator::new(store.root().to_path_buf(), store.metrics.clone());
        Self {
            store,
            catalog_updates,
        }
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

    pub async fn backfill_catalog(
        &self,
    ) -> Result<Vec<bcode_session_models::SessionSummary>, SessionStoreError> {
        let store = self.store.clone();
        let summaries = spawn_blocking(move || store.backfill_catalog()).await??;
        for summary in summaries.iter().cloned() {
            self.catalog_updates.schedule(summary).await;
        }
        self.catalog_updates.flush().await;
        Ok(summaries)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_catalog_flush_before_commit(&self) {
        self.catalog_updates.fail_next_flush_before_commit();
    }

    #[cfg(test)]
    pub(crate) async fn pending_catalog_updates(&self) -> usize {
        self.catalog_updates.pending_len().await
    }

    pub(crate) async fn schedule_catalog_summary(
        &self,
        summary: bcode_session_models::SessionSummary,
    ) {
        self.catalog_updates.schedule(summary).await;
    }

    pub(crate) async fn delete_catalog_session(&self, session_id: SessionId, updated_at_ms: u64) {
        self.catalog_updates.delete(session_id, updated_at_ms).await;
        self.catalog_updates.flush().await;
    }

    pub(crate) async fn flush_catalog_updates(&self) {
        self.catalog_updates.flush().await;
    }

    pub(crate) async fn shutdown_catalog_updates(&self) {
        self.catalog_updates.shutdown().await;
    }

    pub(crate) async fn write_session_manifest(
        &self,
        summary: bcode_session_models::SessionSummary,
    ) -> Result<(), SessionStoreError> {
        let store = self.store.clone();
        spawn_blocking(move || store.write_session_manifest(&summary)).await?
    }
}
