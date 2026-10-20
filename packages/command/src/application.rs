//! Caller-bound typed command discovery, execution and cancellation.
//!
//! Plugin payload encoding is an implementation detail. Responses retain command-owned
//! effects; consumers manifest them natively without changing execution semantics.

use crate::{CommandList, InvokeCommandRequest, InvokeCommandResponse};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::PathBuf;

/// Secret-safe application routing or execution failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandApplicationFailure {
    /// Extensible machine-readable code; unknown codes are not success.
    pub code: String,
    /// Normalized public diagnostic.
    pub message: String,
}

/// Typed invocation with caller-scoped identity and checked workspace hints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandInvocationRequest {
    /// Enabled plugin owning the command service.
    pub plugin_id: String,
    /// Unique identity within this caller connection; reuse is rejected by the owner.
    pub invocation_id: String,
    /// Optional enabled-agent hint; not permission authority.
    pub agent_id: Option<String>,
    /// Optional workspace hint; canonical session context takes precedence.
    pub launch_root: Option<PathBuf>,
    /// Total execution deadline in milliseconds, between 1 and 30,000.
    pub deadline_ms: u64,
    /// Existing command-owned input, including optional canonical session context.
    pub command: InvokeCommandRequest,
}

/// Command invocation result, including explicit uncertainty when no settled owner response arrived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandInvocationResult {
    /// Exact caller-scoped invocation identity.
    pub invocation_id: String,
    /// Observed result; `Unknown` is not a terminal execution outcome.
    pub outcome: CommandInvocationOutcome,
}

/// Observed command result, separating owner responses from transport uncertainty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "result", rename_all = "snake_case")]
pub enum CommandInvocationOutcome {
    /// Plugin returned a valid command response; `success` remains authoritative.
    Completed(InvokeCommandResponse),
    /// Host routing, plugin execution, cancellation or decoding failed.
    Failed(CommandApplicationFailure),
    /// No trustworthy settled owner response arrived. Effects may have committed.
    /// This does not establish cancellation, rollback, or safe replay.
    Unknown(CommandApplicationFailure),
}

/// Cancellation acknowledgment; acceptance does not prove terminal cancellation or rollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandCancellationAcknowledgment {
    /// Exact caller-scoped invocation identity.
    pub invocation_id: String,
    /// Whether the owner accepted the cancellation request for active work.
    pub accepted: bool,
}

/// Command operations supplied for a verified caller; no renderer or plugin-runtime access.
pub trait CommandApplication: Sync {
    /// List typed commands from one enabled command-service owner.
    /// # Errors
    /// Rejects unavailable services, invalid responses or plugin-owned failures.
    fn list_commands(
        &self,
        plugin_id: String,
    ) -> impl Future<Output = Result<CommandList, CommandApplicationFailure>> + Send;
    /// Execute through the existing invocation owner and report its result or explicit uncertainty.
    /// Dropping this future does not establish cancellation, rollback or safe replay.
    fn invoke_command(
        &self,
        request: CommandInvocationRequest,
    ) -> impl Future<Output = CommandInvocationResult> + Send;
    /// Request cancellation through the same caller's invocation owner.
    /// # Errors
    /// Rejects unavailable application access; false means no active matching invocation.
    fn cancel_command(
        &self,
        invocation_id: String,
    ) -> impl Future<Output = Result<CommandCancellationAcknowledgment, CommandApplicationFailure>> + Send;
}
