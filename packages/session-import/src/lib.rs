#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Contract types for session import provider plugins.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Plugin service interface for session import providers.
pub const SESSION_IMPORT_INTERFACE_ID: &str = "bcode.session_import/v1";

/// Operation to list import sources exposed by a plugin.
pub const OP_LIST_IMPORT_SOURCES: &str = "list_sources";

/// Operation to discover importable sessions.
pub const OP_DISCOVER_IMPORTABLE_SESSIONS: &str = "discover_sessions";

/// Operation to load one importable session for one-time import.
pub const OP_LOAD_IMPORTABLE_SESSION: &str = "load_session";

/// A source of external sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportSourceInfo {
    pub source_id: String,
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// Response returned by [`OP_LIST_IMPORT_SOURCES`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListImportSourcesResponse {
    pub sources: Vec<ImportSourceInfo>,
}

/// Request payload for discovering importable sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoverImportableSessionsRequest {
    #[serde(default)]
    pub working_directory: Option<PathBuf>,
}

/// Lightweight external session summary for picker/catalog views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportableSessionSummary {
    pub source_id: String,
    pub source_display_name: String,
    pub external_session_id: String,
    pub locator: String,
    pub title: Option<String>,
    #[serde(default)]
    pub working_directory: Option<PathBuf>,
    #[serde(default)]
    pub created_at_ms: Option<u64>,
    #[serde(default)]
    pub updated_at_ms: Option<u64>,
    #[serde(default)]
    pub message_count: Option<u64>,
    #[serde(default)]
    pub warnings: Vec<ImportWarning>,
}

/// Response returned by [`OP_DISCOVER_IMPORTABLE_SESSIONS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoverImportableSessionsResponse {
    pub sessions: Vec<ImportableSessionSummary>,
}

/// Request payload for loading a selected external session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadImportableSessionRequest {
    pub source_id: String,
    pub external_session_id: String,
    pub locator: String,
}

/// Fully loaded external session represented as provider-neutral import events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportableSession {
    pub summary: ImportableSessionSummary,
    pub events: Vec<ImportableSessionEvent>,
    #[serde(default)]
    pub warnings: Vec<ImportWarning>,
}

/// One importable event with source timestamp metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportableSessionEvent {
    #[serde(default)]
    pub external_event_id: Option<String>,
    #[serde(default)]
    pub timestamp_ms: Option<u64>,
    pub kind: ImportableSessionEventKind,
}

/// Provider-neutral event kinds understood by the Bcode importer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportableSessionEventKind {
    UserMessage {
        text: String,
    },
    AssistantMessage {
        text: String,
    },
    ToolCallRequested {
        tool_call_id: String,
        tool_name: String,
        arguments_json: String,
    },
    ToolCallFinished {
        tool_call_id: String,
        result: String,
        #[serde(default)]
        is_error: bool,
    },
    AssistantReasoningMessage {
        text: String,
    },
    ModelUsage {
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
        total_tokens: Option<u32>,
        cached_input_tokens: Option<u32>,
        cache_write_input_tokens: Option<u32>,
        reasoning_tokens: Option<u32>,
    },
    ModelChanged {
        provider: String,
        model: String,
    },
    AgentChanged {
        agent_id: String,
    },
    ContextCompacted {
        summary: String,
    },
    SystemMessage {
        text: String,
    },
}

/// Warning produced while discovering or importing an external session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportWarning {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub count: Option<u64>,
}

impl ImportWarning {
    /// Create a new import warning.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            count: None,
        }
    }

    /// Create a counted import warning.
    #[must_use]
    pub fn counted(code: impl Into<String>, message: impl Into<String>, count: u64) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            count: Some(count),
        }
    }
}
