#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Portable shell delivery contracts. Authentication belongs to canonical workflow outputs.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Complete delivered target, not a selection from or certification of a live workspace.
///
/// Version 1 supports only UTF-8 regular non-executable files and implicit directories.
/// External dependencies, tools, environment, and command hermeticity are not certified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveredSnapshot {
    /// Representation version; unknown versions fail closed.
    pub version: u32,
    /// Every delivered file, including its exact content. No exclusions or omitted scopes.
    pub files: BTreeMap<String, String>,
}
impl DeliveredSnapshot {
    /// Validate bounded, portable content and paths without filesystem access.
    /// # Errors
    /// Rejects unsupported versions, empty/oversized targets, unsafe paths and file/directory collisions.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 || self.files.is_empty() || self.files.len() > 64 {
            return Err("unsupported snapshot version or file count (1..=64)".into());
        }
        let mut bytes = 0usize;
        for (path, content) in &self.files {
            bytes = bytes
                .saturating_add(path.len())
                .saturating_add(content.len());
            if path.len() > 1024
                || path.contains(['\\', ':', '\0'])
                || path
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == "..")
                || path
                    .split('/')
                    .scan(String::new(), |prefix, part| {
                        if !prefix.is_empty() {
                            prefix.push('/');
                        }
                        prefix.push_str(part);
                        Some(prefix.clone())
                    })
                    .any(|prefix| prefix != *path && self.files.contains_key(&prefix))
            {
                return Err("unsupported snapshot path or file/directory collision".into());
            }
        }
        if bytes > 262_144 {
            return Err("snapshot exceeds 256 KiB".into());
        }
        Ok(())
    }
}

/// Shell-observed outcome over an immutable inline delivered target.
/// Only meaningful when loaded from an authenticated canonical shell execution output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotVerification {
    /// Evidence boundary version.
    pub version: u32,
    /// Complete immutable delivered bytes; never a mutable workspace reference.
    pub target: DeliveredSnapshot,
    /// All commands exited with accepted codes, without cancellation or timeout.
    pub commands_passed: bool,
    /// Materialized target still exactly matched after each command.
    pub target_unchanged: bool,
}
impl SnapshotVerification {
    /// Acceptance-time revalidation against the exact target being delivered.
    /// Caller MUST authenticate the canonical producer/output and command plan separately.
    /// This deliberately makes no freshness claim about any live filesystem copy.
    /// # Errors
    /// Rejects unsupported/incomplete, failed, mutated, or mismatched evidence.
    pub fn accept(&self, delivered: &DeliveredSnapshot) -> Result<(), String> {
        delivered.validate()?;
        self.target.validate()?;
        if self.version != 1
            || !self.commands_passed
            || !self.target_unchanged
            || self.target != *delivered
        {
            return Err("snapshot verification unsupported, failed, incomplete, or stale".into());
        }
        Ok(())
    }
}
