#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Historical session persistence decoding and migration planning.
//!
//! The current session runtime deliberately does not know historical durable
//! event shapes. This crate owns frozen readers for formats emitted by older
//! Bcode writers and converts them into the current session domain model.

mod audit;
mod historical;
mod operation;
mod registry;

pub use audit::SessionMigrationReceipt;
pub use operation::{SessionMigrationOperation, SessionMigrationOperations};

pub use historical::{
    HistoricalDecode, HistoricalEventMetadata, HistoricalSessionEventError, decode_for_migration,
    historical_conversion_counts, ordered_payload_digest,
};
pub use registry::{
    CURRENT_WRITER_EPOCH, MigrationPlan, MigrationPlanError, MigrationPlanService,
    MigrationStepDescriptor, plan_writer_epoch_migration,
};
