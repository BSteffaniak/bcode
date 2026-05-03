#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Command contract types and `bcode.command/v1` service interface for Bcode.
//!
//! Plugins declare this interface in their manifest to contribute commands
//! discoverable via the control panel and slash commands.

use bcode_model::ReasoningEffort;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Plugin service interface for command providers / core command registry.
pub const COMMAND_INTERFACE_ID: &str = "bcode.command/v1";

/// Operation to list available commands (returns `CommandList`).
pub const OP_LIST_COMMANDS: &str = "list";

/// Operation to invoke a command (request `InvokeCommandRequest`, response `InvokeCommandResponse`).
pub const OP_INVOKE_COMMAND: &str = "invoke";

/// Command metadata for palette / slash discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandInfo {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub requires_args: bool,
    #[serde(default)]
    pub category: Option<String>,
}

/// Response to `OP_LIST_COMMANDS`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandList {
    pub commands: Vec<CommandInfo>,
}

/// Request payload for `OP_INVOKE_COMMAND`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvokeCommandRequest {
    pub command_id: String,
    #[serde(default)]
    pub args: BTreeMap<String, String>,
}

/// Response from command invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvokeCommandResponse {
    pub success: bool,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub updated_model: Option<String>,
    #[serde(default)]
    pub updated_provider: Option<String>,
    #[serde(default)]
    pub updated_thinking: Option<ReasoningEffort>,
}

/// Errors returned by command operations.
///
/// * Invalid command ID or arguments.
/// * Underlying provider / session error.
/// * Permission denied for the action.
#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
pub enum CommandError {
    #[error("unknown command: {0}")]
    UnknownCommand(String),
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("execution failed: {0}")]
    ExecutionFailed(String),
}
