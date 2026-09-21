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
    /// Include diagnostic source/status entries in discovery summaries.
    #[serde(default)]
    pub include_diagnostics: bool,
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
    pub status: ImportableSessionStatus,
    #[serde(default)]
    pub warnings: Vec<ImportWarning>,
}

/// Discovery/load status for an importable session summary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportableSessionStatus {
    /// Session is ready to import.
    #[default]
    Available,
    /// Diagnostic row describing a source/path condition, not an importable session.
    Diagnostic,
    /// Source exists but could not be scanned completely.
    Unavailable,
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

/// A complete normalized remote revision returned by a history adapter.
///
/// Version 1 is independent of the one-shot session-import interface. Consumers
/// must reject unsupported versions before publication. This payload does not
/// attest remote account scope: the host must establish that separately before
/// associating the revision with a canonical session. Source identifiers are
/// opaque and must not be converted into website URLs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportableHistorySnapshot {
    /// Payload compatibility version (currently 1).
    pub schema_version: u32,
    /// API conversation identity within a separately verified remote scope.
    pub conversation_id: String,
    /// Source title, not trusted instructions.
    pub title: Option<String>,
    /// Selected source graph leaf; events follow this ancestry only.
    pub selected_node: String,
    /// Adapter-defined versioned identity of the normalized revision.
    pub revision_id: String,
    /// Complete selected-branch historical events, never executable authority.
    /// V1 remote history accepts only user/assistant text events. Tools and privileged
    /// source roles must be represented as labelled historical text with source metadata,
    /// not operational tool, agent, model, compaction, reasoning or usage events.
    pub events: Vec<ImportableSessionEvent>,
    /// Source message metadata keyed by the event's external node identity.
    /// Values describe untrusted history, never local model/tool routing.
    #[serde(default)]
    pub message_metadata: std::collections::BTreeMap<String, HistoryMessageMetadata>,
    /// Fidelity limitations that must remain visible with the imported content.
    pub warnings: Vec<ImportWarning>,
}

/// Portable source labels retained without granting executable authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryMessageMetadata {
    /// Original message identity, which may differ from its graph node identity.
    pub message_id: Option<String>,
    /// Source model label, not a locally resolved model selection.
    pub model: Option<String>,
    /// Original source role, not a trusted instruction role.
    pub role: String,
    /// Historical author or tool label.
    pub author: Option<String>,
    /// Historical destination, never a local routing instruction.
    pub recipient: Option<String>,
    /// Source content discriminator, not a claim of attachment preservation.
    pub content_type: Option<String>,
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
