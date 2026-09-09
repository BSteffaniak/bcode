#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Portable plugin discovery metadata, independent of native loading and renderers.
//!
//! These types retain the manifest/discovery field representation. Optional fields
//! and serde defaults preserve existing decoding; schema versions in config metadata
//! describe plugin-owned schemas, not host implementation versions.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
