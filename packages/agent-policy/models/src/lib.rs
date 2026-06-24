#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Agent permission policy models shared between the policy evaluator and configuration layer.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Agent action loaded from Pi/OpenCode-style permission config.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Allow the operation without prompting.
    Allow,
    /// Deny the operation.
    Deny,
    /// Ask via the host permission prompt.
    #[default]
    Ask,
}

/// Agent mode/profile configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Optional UI accent color, encoded as `#RRGGBB`.
    #[serde(default)]
    pub accent: Option<String>,
    /// Per-tool enablement map. Missing entries inherit the host default.
    #[serde(default)]
    pub tools: BTreeMap<String, bool>,
    /// Permission rules applied to tool invocations for this agent.
    #[serde(default)]
    pub permission: PermissionConfig,
}

/// Permission configuration for an agent.
///
/// Agent `tools` maps must use exact model-callable tool IDs such as
/// `shell.run` or plugin-defined IDs. Permission categories are separate rule
/// buckets used by plugin policy metadata:
///
/// * `bash` — patterns matched against command arguments for tools categorized as shell commands.
/// * `read` — patterns matched against path arguments for tools categorized as read-only path tools.
/// * `write` — patterns matched against path arguments for tools categorized as file writes.
/// * `edit` — patterns matched against path arguments for tools categorized as file edits.
/// * `web` — patterns matched against URL arguments for web/network tools.
/// * `external_directory` — single action governing any tool argument resolving outside
///   the session working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionConfig {
    /// Shell command rules.
    #[serde(default)]
    pub bash: BTreeMap<String, Action>,
    /// Read-only filesystem tool rules keyed by path glob.
    #[serde(default)]
    pub read: BTreeMap<String, Action>,
    /// `filesystem.write` rules keyed by path glob.
    #[serde(default)]
    pub write: BTreeMap<String, Action>,
    /// `filesystem.edit` rules keyed by path glob.
    #[serde(default)]
    pub edit: BTreeMap<String, Action>,
    /// Web/network tool rules keyed by URL glob.
    #[serde(default)]
    pub web: BTreeMap<String, Action>,
    /// Action governing any path resolving outside the session working directory.
    #[serde(default = "default_external_directory_action")]
    pub external_directory: Action,
}

impl Default for PermissionConfig {
    fn default() -> Self {
        Self {
            bash: BTreeMap::new(),
            read: BTreeMap::new(),
            write: BTreeMap::new(),
            edit: BTreeMap::new(),
            web: BTreeMap::new(),
            external_directory: default_external_directory_action(),
        }
    }
}

/// Default action applied to arguments that resolve outside the session working directory.
#[must_use]
pub const fn default_external_directory_action() -> Action {
    Action::Allow
}

/// Pi/OpenCode-style top-level permission config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPermissionConfig {
    /// Per-agent configuration keyed by agent ID.
    #[serde(default)]
    pub agent: BTreeMap<String, AgentConfig>,
}
