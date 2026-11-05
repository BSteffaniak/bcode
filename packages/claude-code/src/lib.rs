//! Rust-native Claude Code CLI protocol. No Bcode session or scheduling dependencies.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

mod attribution;
mod auth;
mod launch;
mod protocol;
mod transport;

pub use attribution::{AttributionStage, SubmissionAttribution, SubmissionUuid};
pub use auth::{
    AuthMethod, AuthStatus, NativeAuthSupport, NativeLoginHandoff, auth_status, login_handoff,
};
pub use launch::{
    BillingRoute, LaunchOptions, Limits, NativeEnvironment, SettingsInheritance,
    validate_session_uuid,
};
pub use protocol::{
    CallbackRequest, CallbackResponse, ControlRequest, Event, HookMatcher, NativeMessage,
    PermissionMode, SessionState, Settlement, SettlementSupport,
};
pub use transport::{
    ClaudeClient, ClaudeSession, ControlTicket, DeliveryReceipt, IdentityState, InitializedSession,
    PhysicalReap,
};

/// Transport errors deliberately omit native output and credential-bearing payloads.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Configuration does not establish a safe supported launch.
    #[error("invalid Claude Code launch: {0}")]
    InvalidConfiguration(&'static str),
    /// A native I/O operation failed. Delivery may already have happened.
    #[error("Claude Code I/O failed")]
    Io(#[from] std::io::Error),
    /// Native protocol is malformed or outside negotiated support.
    #[error("unsupported or malformed Claude Code protocol")]
    Protocol,
    /// A configured memory/concurrency bound was exceeded.
    #[error("Claude Code transport bound exceeded")]
    Capacity,
    /// Native process or observation stream ended without a delivery guarantee.
    #[error("Claude Code connection closed; reconcile before retrying")]
    Closed,
    /// No answer arrived; this is not evidence of non-execution.
    #[error("Claude Code request outcome uncertain; do not replay automatically")]
    Uncertain,
    /// Callback identity is no longer active.
    #[error("Claude Code callback was cancelled, answered, or is unknown")]
    StaleCallback,
    /// No native-confirmed identity arrived, or it contradicted the requested association.
    #[error("Claude Code session identity is missing or mismatched")]
    SessionIdentity,
    /// Native CLI version differs from the explicit compatibility declaration.
    #[error("Claude Code executable version mismatch")]
    VersionMismatch,
}
