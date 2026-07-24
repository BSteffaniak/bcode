use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Durable audit receipt for one completed session migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMigrationReceipt {
    /// Stable migration operation identity.
    pub operation_id: String,
    /// Writer epoch observed before migration.
    pub source_writer_epoch: u32,
    /// Writer epoch installed after validation.
    pub target_writer_epoch: u32,
    /// Ordered migration steps applied by the operation.
    pub migration_step_ids: Vec<String>,
    /// Canonical source event count.
    pub source_event_count: u64,
    /// Canonical source tail, if the session contains events.
    pub source_event_tail: Option<u64>,
    /// Digest over the ordered source canonical payloads.
    pub source_event_digest_sha256: String,
    /// Canonical target event count.
    pub target_event_count: u64,
    /// Canonical target tail, if the session contains events.
    pub target_event_tail: Option<u64>,
    /// Digest over the ordered target canonical payloads.
    pub target_event_digest_sha256: String,
    /// Converted event counts keyed by `schema:kind`.
    pub converted_events: BTreeMap<String, u64>,
    /// Retired-known event counts keyed by `schema:kind`.
    pub retired_known_events: BTreeMap<String, u64>,
    /// Completion time in Unix milliseconds.
    pub completed_at_ms: u64,
}
