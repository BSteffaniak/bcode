#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Portable plugin discovery metadata, independent of native loading and renderers.
//!
//! These types retain the manifest/discovery field representation. Optional fields
//! and serde defaults preserve existing decoding; schema versions in config metadata
//! describe plugin-owned schemas, not host implementation versions.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Plugin service execution concurrency policy.
/// The existing externally tagged serde representation is retained.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PluginConcurrency {
    /// Allow unconstrained concurrent plugin execution.
    #[default]
    Concurrent,
    /// Serialize invocations for this plugin on a dedicated worker.
    Exclusive,
    /// Reserve support for bounded concurrent plugin execution.
    Limited(usize),
}

/// Portable plugin executor status snapshot; field representation is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginExecutorStatus {
    pub plugin_id: String,
    pub concurrency: PluginConcurrency,
    pub running: usize,
    pub queued: usize,
    pub queued_control: usize,
    pub queued_query: usize,
    pub queued_tool_execution: usize,
    pub queued_model_provider: usize,
    pub queued_event_delivery: usize,
    pub queued_service: usize,
    pub completed: u64,
    pub failed: u64,
}

/// Serialized plugin-owned workflow template metadata, without loading or validation behavior.
///
/// Schema and definition types are supplied by the consuming workflow contract. Field
/// names and optional-field semantics match contribution version 1; consumers must
/// validate the contribution version before execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    deny_unknown_fields,
    bound(deserialize = "Schema: Deserialize<'de>, Definition: Deserialize<'de>")
)]
pub struct WorkflowTemplateDescriptor<Schema, Definition> {
    pub contribution_version: u32,
    pub template_id: String,
    pub template_version: u32,
    pub title: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration_schema: Option<Schema>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<Definition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_source: Option<WorkflowTemplateDocumentSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_plugins: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub presentation: std::collections::BTreeMap<String, String>,
}

impl<Schema, Definition> WorkflowTemplateDescriptor<Schema, Definition> {
    /// Return the inline schema of a validated inline contribution.
    ///
    /// # Panics
    /// Panics if no inline schema exists. External templates use the separately supplied document.
    #[must_use]
    pub const fn configuration_schema(&self) -> &Schema {
        self.configuration_schema
            .as_ref()
            .expect("validated inline template schema")
    }
}

/// Plugin-package-relative authoring document identity. Loading must validate confinement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowTemplateDocumentSource {
    pub path: PathBuf,
    pub sha256: String,
}

/// Command palette/action contribution declared by a plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCommandContribution {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub surface: Option<String>,
}

/// Command contribution with plugin ownership attached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginOwnedCommandContribution {
    pub plugin_id: String,
    pub command: PluginCommandContribution,
}

/// Plugin-owned config alias declaration from a plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginConfigAlias {
    /// User-facing top-level config section or dotted path.
    pub section: String,
    /// Optional reason, normally `legacy`, `compatibility`, or `short_name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Resolved plugin config extension metadata with plugin ownership attached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginConfigExtension {
    pub plugin_id: String,
    pub section: Option<String>,
    pub aliases: Vec<PluginConfigAlias>,
    pub categories: Vec<String>,
    pub schema_version: Option<u16>,
    pub schema_file: Option<PathBuf>,
}

impl PluginConfigExtension {
    /// Return the primary config section plus manifest-declared aliases.
    #[must_use]
    pub fn sections(&self) -> Vec<&str> {
        self.section
            .iter()
            .map(String::as_str)
            .chain(self.aliases.iter().map(|alias| alias.section.as_str()))
            .collect()
    }
}

/// Plugin default selection mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PluginSelectionMode {
    /// Enable all candidates unless disabled.
    All,
    /// Enable only explicitly selected plugin IDs.
    #[default]
    Explicit,
}

/// Plugin enable/disable selection policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSelection {
    pub mode: PluginSelectionMode,
    pub enabled: BTreeSet<String>,
    pub disabled: BTreeSet<String>,
}

impl Default for PluginSelection {
    fn default() -> Self {
        Self {
            mode: PluginSelectionMode::Explicit,
            enabled: BTreeSet::new(),
            disabled: BTreeSet::new(),
        }
    }
}

impl PluginSelection {
    /// Return a policy where all discovered plugins are enabled unless disabled.
    #[must_use]
    pub fn all_enabled() -> Self {
        Self {
            mode: PluginSelectionMode::All,
            ..Self::default()
        }
    }

    /// Return true when the plugin ID is enabled by this selection policy.
    #[must_use]
    pub fn is_enabled(&self, plugin_id: &str) -> bool {
        if self.disabled.contains(plugin_id) {
            return false;
        }
        match self.mode {
            PluginSelectionMode::All => true,
            PluginSelectionMode::Explicit => self.enabled.contains(plugin_id),
        }
    }
}

/// Loaded route for a manifest-declared visual adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginVisualAdapterRoute {
    pub plugin_id: String,
    pub adapter_id: String,
    pub schema: String,
    pub service_interface_id: String,
    pub surfaces: Vec<String>,
    pub priority: i32,
    pub producer_default: bool,
    pub render_mode: PluginVisualAdapterRenderMode,
}

impl PluginVisualAdapterRoute {
    /// Return this route's stable user-facing adapter reference.
    #[must_use]
    pub fn adapter_reference(&self) -> String {
        format!("{}/{}", self.plugin_id, self.adapter_id)
    }
}

/// How a visual adapter's rows should be composed into host transcript chrome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginVisualAdapterRenderMode {
    #[default]
    Inline,
    TranscriptBlock,
    FullBlock,
}
