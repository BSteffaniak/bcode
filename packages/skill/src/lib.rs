#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Skill discovery, parsing, and registry support for Bcode.

use bcode_skill_models::{
    SkillActivation, SkillDiagnostic, SkillDiagnosticSeverity, SkillError, SkillId, SkillList,
    SkillManifest, SkillPermissionHints, SkillSource, SkillSourceKind, SkillSummary,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use thiserror::Error;

const DEFAULT_MAX_SKILL_FILE_BYTES: u64 = 256 * 1024;
const DEFAULT_MAX_CONTEXT_BYTES: usize = 24 * 1024;
const SKILL_FILE_NAME: &str = "SKILL.md";

/// Errors returned by skill registry operations.
#[derive(Debug, Error)]
pub enum SkillRegistryError {
    /// Filesystem operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Skill operation failed.
    #[error("skill error: {0}")]
    Skill(#[from] SkillError),
}

/// Discovery source root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSourceRoot {
    /// Root directory containing skill subdirectories or a direct `SKILL.md`.
    pub path: PathBuf,
    /// Source kind for discovered skills.
    pub kind: SkillSourceKind,
    /// Human-readable source label.
    pub label: String,
    /// Source precedence; lower values win when duplicate IDs are discovered.
    pub precedence: u16,
}

impl SkillSourceRoot {
    /// Create a new source root.
    #[must_use]
    pub fn new(
        path: impl Into<PathBuf>,
        kind: SkillSourceKind,
        label: impl Into<String>,
        precedence: u16,
    ) -> Self {
        Self {
            path: path.into(),
            kind,
            label: label.into(),
            precedence,
        }
    }
}

/// Registry build options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRegistryOptions {
    /// Maximum bytes accepted for a `SKILL.md` file.
    pub max_skill_file_bytes: u64,
    /// Maximum skill context bytes returned by default.
    pub max_context_bytes: usize,
    /// Whether source scanning follows symlinks.
    pub follow_symlinks: bool,
    /// Disabled skill IDs.
    pub disabled_ids: BTreeSet<SkillId>,
}

impl Default for SkillRegistryOptions {
    fn default() -> Self {
        Self {
            max_skill_file_bytes: DEFAULT_MAX_SKILL_FILE_BYTES,
            max_context_bytes: DEFAULT_MAX_CONTEXT_BYTES,
            follow_symlinks: false,
            disabled_ids: BTreeSet::new(),
        }
    }
}

/// In-memory skill registry.
#[derive(Debug, Clone)]
pub struct SkillRegistry {
    entries: BTreeMap<SkillId, SkillEntry>,
    diagnostics: Vec<SkillDiagnostic>,
    options: SkillRegistryOptions,
}

impl SkillRegistry {
    /// Discover skills from source roots.
    ///
    /// # Errors
    ///
    /// Returns an error only for unrecoverable registry failures. Per-skill malformed files and
    /// unreadable source roots are reported as diagnostics and skipped.
    pub fn discover(
        roots: &[SkillSourceRoot],
        options: SkillRegistryOptions,
    ) -> Result<Self, SkillRegistryError> {
        let mut registry = Self {
            entries: BTreeMap::new(),
            diagnostics: Vec::new(),
            options,
        };

        for root in roots {
            registry.scan_root(root);
        }

        Ok(registry)
    }

    /// Return compact list response.
    #[must_use]
    pub fn list(&self) -> SkillList {
        SkillList {
            skills: self
                .entries
                .values()
                .map(|entry| entry.summary.clone())
                .collect(),
            diagnostics: self.diagnostics.clone(),
        }
    }

    /// Return one skill summary.
    #[must_use]
    pub fn summary(&self, skill_id: &SkillId) -> Option<&SkillSummary> {
        self.entries.get(skill_id).map(|entry| &entry.summary)
    }

    /// Load and parse a full skill manifest.
    ///
    /// # Errors
    ///
    /// Returns an error when the skill is unknown, disabled, too large, unreadable, or malformed.
    pub fn describe(&self, skill_id: &SkillId) -> Result<SkillManifest, SkillRegistryError> {
        let entry = self.entry(skill_id)?;
        if entry.path.as_os_str().is_empty() {
            return Err(SkillError::InvalidMetadata(
                "plugin-provided skill context must be loaded through its provider".to_string(),
            )
            .into());
        }
        parse_skill_file(
            &entry.path,
            &entry.summary.source,
            self.options.max_skill_file_bytes,
        )
    }

    /// Load bounded model context for an active skill.
    ///
    /// # Errors
    ///
    /// Returns an error when the skill is unknown, disabled, unreadable, or malformed.
    pub fn context(
        &self,
        skill_id: &SkillId,
        max_bytes: Option<usize>,
    ) -> Result<String, SkillRegistryError> {
        let manifest = self.describe(skill_id)?;
        let budget = max_bytes.unwrap_or(self.options.max_context_bytes);
        let mut context = format!(
            "Active Bcode skill: {}\nSource: {}\nVersion: {}\n\nInstructions:\n{}",
            manifest.summary.id,
            manifest.summary.source.label,
            manifest.summary.version.as_deref().unwrap_or("unknown"),
            manifest.instructions
        );
        if context.len() > budget {
            context.truncate(budget);
        }
        Ok(context)
    }

    fn entry(&self, skill_id: &SkillId) -> Result<&SkillEntry, SkillError> {
        if self.options.disabled_ids.contains(skill_id) {
            return Err(SkillError::DisabledSkill(skill_id.to_string()));
        }
        self.entries
            .get(skill_id)
            .ok_or_else(|| SkillError::UnknownSkill(skill_id.to_string()))
    }

    fn scan_root(&mut self, root: &SkillSourceRoot) {
        let metadata = match fs::symlink_metadata(&root.path) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.diagnostics.push(diagnostic(
                    SkillDiagnosticSeverity::Warning,
                    format!("skill source is not readable: {error}"),
                    Some(&root.path),
                ));
                return;
            }
        };

        if metadata.file_type().is_symlink() && !self.options.follow_symlinks {
            self.diagnostics.push(diagnostic(
                SkillDiagnosticSeverity::Warning,
                "skill source symlink skipped".to_string(),
                Some(&root.path),
            ));
            return;
        }

        if root.path.join(SKILL_FILE_NAME).is_file() {
            self.scan_skill_dir(root, &root.path);
            return;
        }

        let entries = match fs::read_dir(&root.path) {
            Ok(entries) => entries,
            Err(error) => {
                self.diagnostics.push(diagnostic(
                    SkillDiagnosticSeverity::Warning,
                    format!("skill source directory is not readable: {error}"),
                    Some(&root.path),
                ));
                return;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.join(SKILL_FILE_NAME).is_file() {
                self.scan_skill_dir(root, &path);
            }
        }
    }

    fn scan_skill_dir(&mut self, root: &SkillSourceRoot, skill_dir: &Path) {
        let source = SkillSource {
            kind: root.kind,
            label: root.label.clone(),
            path: Some(skill_dir.to_string_lossy().into_owned()),
            precedence: root.precedence,
        };
        let skill_file = skill_dir.join(SKILL_FILE_NAME);
        match parse_skill_file(&skill_file, &source, self.options.max_skill_file_bytes) {
            Ok(manifest) => self.insert_skill(SkillEntry {
                summary: manifest.summary,
                path: skill_file,
            }),
            Err(error) => self.diagnostics.push(diagnostic(
                SkillDiagnosticSeverity::Warning,
                error.to_string(),
                Some(&skill_file),
            )),
        }
    }

    /// Insert a plugin-provided skill summary.
    pub fn insert_plugin_skill(&mut self, summary: SkillSummary) {
        self.insert_skill(SkillEntry {
            summary,
            path: PathBuf::new(),
        });
    }

    fn insert_skill(&mut self, entry: SkillEntry) {
        if let Some(existing) = self.entries.get(&entry.summary.id)
            && existing.summary.source.precedence <= entry.summary.source.precedence
        {
            self.diagnostics.push(diagnostic(
                SkillDiagnosticSeverity::Info,
                format!(
                    "skill {} from {} shadowed by higher-precedence source {}",
                    entry.summary.id, entry.summary.source.label, existing.summary.source.label
                ),
                entry.summary.source.path.as_deref().map(Path::new),
            ));
            return;
        }
        self.entries.insert(entry.summary.id.clone(), entry);
    }
}

#[derive(Debug, Clone)]
struct SkillEntry {
    summary: SkillSummary,
    path: PathBuf,
}

/// Parse a `SKILL.md` file.
///
/// # Errors
///
/// Returns an error when the file cannot be read, exceeds size limits, or lacks required metadata.
pub fn parse_skill_file(
    path: &Path,
    source: &SkillSource,
    max_skill_file_bytes: u64,
) -> Result<SkillManifest, SkillRegistryError> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > max_skill_file_bytes {
        return Err(SkillError::InvalidMetadata(format!(
            "skill file exceeds max size: {} bytes",
            metadata.len()
        ))
        .into());
    }
    let contents = fs::read_to_string(path)?;
    parse_skill_markdown(&contents, source)
}

fn parse_skill_markdown(
    contents: &str,
    source: &SkillSource,
) -> Result<SkillManifest, SkillRegistryError> {
    let (frontmatter, instructions) = split_frontmatter(contents)?;
    let raw: RawSkillFrontmatter = toml::from_str(&yamlish_to_toml(frontmatter))
        .map_err(|error| SkillError::InvalidMetadata(error.to_string()))?;
    let id = raw
        .id
        .ok_or_else(|| SkillError::InvalidMetadata("missing skill id".to_string()))?;
    let name = raw.name.clone().unwrap_or_else(|| id.clone());
    let summary = SkillSummary {
        id: SkillId::from_str(&id)?,
        name,
        description: raw.description,
        version: raw.version,
        source: source.clone(),
        activation: raw.activation.unwrap_or_default(),
        diagnostics: Vec::new(),
    };
    Ok(SkillManifest {
        summary,
        permissions: raw.permissions.unwrap_or_default(),
        instructions: instructions.trim().to_string(),
        metadata: BTreeMap::new(),
    })
}

fn split_frontmatter(contents: &str) -> Result<(&str, &str), SkillError> {
    let Some(rest) = contents.strip_prefix("---\n") else {
        return Err(SkillError::InvalidMetadata(
            "missing frontmatter delimiter".to_string(),
        ));
    };
    let Some(end) = rest.find("\n---\n") else {
        return Err(SkillError::InvalidMetadata(
            "missing closing frontmatter delimiter".to_string(),
        ));
    };
    Ok((&rest[..end], &rest[end + "\n---\n".len()..]))
}

fn yamlish_to_toml(frontmatter: &str) -> String {
    let mut output = String::new();
    let mut current_section: Option<String> = None;
    let mut current_array_key: Option<String> = None;
    let mut current_array_values: Vec<String> = Vec::new();

    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(item) = trimmed.strip_prefix("- ") {
            if current_array_key.is_some() {
                current_array_values.push(item.trim_matches('"').to_string());
            }
            continue;
        }

        flush_array(
            &mut output,
            &mut current_array_key,
            &mut current_array_values,
        );

        let indent = line
            .chars()
            .take_while(|character| character.is_whitespace())
            .count();
        if let Some((raw_key, raw_value)) = trimmed.split_once(':') {
            let raw_key = raw_key.trim();
            let value = raw_value.trim();
            if indent == 0 && value.is_empty() {
                current_section = Some(raw_key.to_string());
                continue;
            }

            let key = if indent > 0 {
                current_section.as_ref().map_or_else(
                    || raw_key.to_string(),
                    |section| format!("{section}.{raw_key}"),
                )
            } else {
                current_section = None;
                raw_key.to_string()
            };

            if value.is_empty() {
                current_array_key = Some(key);
            } else if value.starts_with('[') || value == "true" || value == "false" {
                output.push_str(&key);
                output.push_str(" = ");
                output.push_str(value);
                output.push('\n');
            } else {
                output.push_str(&key);
                output.push_str(" = ");
                output.push_str(&toml_string(value.trim_matches('"')));
                output.push('\n');
            }
        }
    }
    flush_array(
        &mut output,
        &mut current_array_key,
        &mut current_array_values,
    );
    output
}

fn flush_array(output: &mut String, key: &mut Option<String>, values: &mut Vec<String>) {
    if let Some(key) = key.take() {
        output.push_str(&key);
        output.push_str(" = [");
        for (index, value) in values.iter().enumerate() {
            if index > 0 {
                output.push_str(", ");
            }
            output.push_str(&toml_string(value));
        }
        output.push_str("]\n");
        values.clear();
    }
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\\\""))
}

fn diagnostic(
    severity: SkillDiagnosticSeverity,
    message: String,
    path: Option<&Path>,
) -> SkillDiagnostic {
    SkillDiagnostic {
        severity,
        message,
        path: path.map(|path| path.to_string_lossy().into_owned()),
    }
}

#[derive(Debug, Default, serde::Deserialize)]
struct RawSkillFrontmatter {
    id: Option<String>,
    name: Option<String>,
    description: Option<String>,
    version: Option<String>,
    activation: Option<SkillActivation>,
    permissions: Option<SkillPermissionHints>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_yamlish_skill_frontmatter() {
        let source = SkillSource {
            kind: SkillSourceKind::Repository,
            label: "repo".to_string(),
            path: None,
            precedence: 10,
        };
        let manifest = parse_skill_markdown(
            "---\nid: rust-debugging\nname: Rust Debugging\ndescription: Debug Rust\nversion: 0.1.0\nactivation:\n  keywords:\n    - rust\n    - cargo\npermissions:\n  tools:\n    - shell.run\n---\n# Instructions\nDo things.",
            &source,
        )
        .expect("skill parses");

        assert_eq!(manifest.summary.id.as_str(), "rust-debugging");
        assert_eq!(manifest.summary.activation.keywords, vec!["rust", "cargo"]);
        assert_eq!(manifest.permissions.tools, vec!["shell.run"]);
        assert_eq!(manifest.instructions, "# Instructions\nDo things.");
    }
}
