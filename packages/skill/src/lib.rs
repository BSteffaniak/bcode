#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Skill discovery, parsing, and registry support for Bcode.

use bcode_skill_models::{
    GenericThinkingEffort, ResolvedSkillPermissionPolicy, SkillActivation, SkillDiagnostic,
    SkillDiagnosticSeverity, SkillError, SkillId, SkillList, SkillManifest, SkillModelPolicy,
    SkillModelRequest, SkillPermissionHints, SkillPermissionMode, SkillPermissionPolicy,
    SkillSource, SkillSourceKind, SkillSummary, SkillThinkingEffort, SkillToolPolicyOutcome,
    SkillToolPolicyRequest, SkillToolPolicyTarget,
};
use bcode_tool::{ResolvedToolSelector, ToolDefinition, ToolReferenceResolution};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use thiserror::Error;

const DEFAULT_MAX_SKILL_FILE_BYTES: u64 = 256 * 1024;
const DEFAULT_MAX_CONTEXT_BYTES: usize = 24 * 1024;
const SKILL_FILE_NAMES: &[&str] = &["SKILL.md", "skill.md", "README.md"];
const DEFAULT_PROMPT_CATALOG_BYTES: usize = 8 * 1024;
const DEFAULT_PROMPT_DESCRIPTION_CHARS: usize = 240;

/// Skill prompt catalog rendering mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillPromptCatalogMode {
    /// Do not render a skill catalog.
    Off,
    /// Render skill IDs/names and locations only.
    NamesOnly,
    /// Render IDs/names, descriptions, locations, sources, and optionally keywords.
    Summary,
}

/// Options for rendering model-visible skill catalog metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPromptCatalogOptions {
    /// Catalog rendering mode.
    pub mode: SkillPromptCatalogMode,
    /// Maximum bytes returned.
    pub max_bytes: usize,
    /// Maximum description characters per skill.
    pub max_description_chars: usize,
    /// Include source labels.
    pub include_sources: bool,
    /// Include activation keywords.
    pub include_keywords: bool,
}

impl Default for SkillPromptCatalogOptions {
    fn default() -> Self {
        Self {
            mode: SkillPromptCatalogMode::Summary,
            max_bytes: DEFAULT_PROMPT_CATALOG_BYTES,
            max_description_chars: DEFAULT_PROMPT_DESCRIPTION_CHARS,
            include_sources: true,
            include_keywords: false,
        }
    }
}

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

/// Build configured skill source roots from the Bcode configuration.
#[must_use]
pub fn skill_source_roots_from_config(config: &bcode_config::BcodeConfig) -> Vec<SkillSourceRoot> {
    let mut roots = Vec::new();
    if !config.skills.enabled {
        return roots;
    }
    if config.skills.include_repo_skills {
        roots.push(SkillSourceRoot::new(
            PathBuf::from(".bcode/skills"),
            SkillSourceKind::Repository,
            "repo:.bcode/skills",
            10,
        ));
    }
    if config.skills.include_generic_repo_skills {
        roots.push(SkillSourceRoot::new(
            PathBuf::from("skills"),
            SkillSourceKind::Repository,
            "repo:skills",
            15,
        ));
    }
    if config.skills.include_compat_claude_skills {
        roots.push(SkillSourceRoot::new(
            PathBuf::from(".claude/skills"),
            SkillSourceKind::Compatibility,
            "repo:.claude/skills",
            20,
        ));
    }
    if config.skills.include_user_skills {
        roots.push(SkillSourceRoot::new(
            bcode_config::default_config_dir().join("skills"),
            SkillSourceKind::User,
            "user-config:skills",
            30,
        ));
        roots.push(SkillSourceRoot::new(
            bcode_config::default_state_dir().join("skills"),
            SkillSourceKind::User,
            "user-state:skills",
            35,
        ));
    }
    for (index, path) in config.skills.sources.paths.iter().enumerate() {
        roots.push(SkillSourceRoot::new(
            path.clone(),
            SkillSourceKind::Configured,
            format!("configured:{index}"),
            40 + u16::try_from(index).unwrap_or(u16::MAX - 40),
        ));
    }
    roots
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
            follow_symlinks: true,
            disabled_ids: BTreeSet::new(),
        }
    }
}

/// Format a compact XML-style skill catalog for model prompt injection.
#[must_use]
pub fn format_skill_catalog_for_prompt(
    list: &SkillList,
    options: &SkillPromptCatalogOptions,
) -> String {
    if options.mode == SkillPromptCatalogMode::Off || options.max_bytes == 0 {
        return String::new();
    }
    let visible_skills = list
        .skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect::<Vec<_>>();
    if visible_skills.is_empty() {
        return String::new();
    }

    let mut output = String::from(
        "\n\nThe following Bcode skills provide specialized instructions for specific tasks.\n\
Use available Bcode filesystem/document tools to load a skill file when the task matches its description.\n\
User skills are discovered from configured roots including ~/.config/bcode/skills when user skills are enabled.\n\
When a skill file references a relative path, resolve it against the skill directory.\n\n\
<available_skills>",
    );
    let mut truncated = false;

    for skill in visible_skills {
        let mut block = String::from("\n  <skill>");
        block.push_str("\n    <id>");
        block.push_str(&escape_xml(skill.id.as_str()));
        block.push_str("</id>");
        block.push_str("\n    <name>");
        block.push_str(&escape_xml(&skill.name));
        block.push_str("</name>");
        if options.mode == SkillPromptCatalogMode::Summary
            && let Some(description) = &skill.description
        {
            block.push_str("\n    <description>");
            block.push_str(&escape_xml(&truncate_chars(
                description,
                options.max_description_chars,
            )));
            block.push_str("</description>");
        }
        if let Some(location) = skill.source.path.as_deref() {
            block.push_str("\n    <location>");
            block.push_str(&escape_xml(location));
            block.push_str("</location>");
        }
        if options.mode == SkillPromptCatalogMode::Summary && options.include_sources {
            block.push_str("\n    <source>");
            block.push_str(&escape_xml(&skill.source.label));
            block.push_str("</source>");
        }
        if options.mode == SkillPromptCatalogMode::Summary
            && options.include_keywords
            && !skill.activation.keywords.is_empty()
        {
            block.push_str("\n    <keywords>");
            block.push_str(&escape_xml(&skill.activation.keywords.join(", ")));
            block.push_str("</keywords>");
        }
        block.push_str("\n  </skill>");

        if output.len() + block.len() + "\n</available_skills>".len() > options.max_bytes {
            truncated = true;
            break;
        }
        output.push_str(&block);
    }
    if truncated {
        output.push_str("\n  <truncated>true</truncated>");
    }
    output.push_str("\n</available_skills>");
    if output.len() > options.max_bytes {
        output.truncate(options.max_bytes);
    }
    output
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut truncated = value.chars().take(max_chars).collect::<String>();
    truncated.push('…');
    truncated
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
        let entry = self.entry(skill_id)?;
        let manifest = self.describe(skill_id)?;
        let budget = max_bytes.unwrap_or(self.options.max_context_bytes);
        let skill_file = entry.path.to_string_lossy();
        let skill_directory = entry
            .path
            .parent()
            .map_or_else(String::new, |path| path.to_string_lossy().into_owned());
        let resource_root = entry
            .path
            .parent()
            .and_then(Path::parent)
            .map_or_else(String::new, |path| path.to_string_lossy().into_owned());
        let mut context = format!(
            "<skill id=\"{}\" name=\"{}\" location=\"{}\">\nReferences are relative to {}.\nSource label: {}\nSkill resource root: {}\nVersion: {}\n\n{}\n</skill>",
            manifest.summary.id,
            manifest.summary.name,
            skill_file,
            skill_directory,
            manifest.summary.source.label,
            resource_root,
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

        if let Some(skill_file) = find_skill_file(&root.path) {
            self.scan_skill_file(root, &root.path, &skill_file);
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
            if let Some(skill_file) = find_skill_file(&path) {
                self.scan_skill_file(root, &path, &skill_file);
            } else if path.is_file() && path.extension().is_some_and(|extension| extension == "md")
            {
                self.scan_skill_file(root, &path, &path);
            }
        }
    }

    fn scan_skill_file(&mut self, root: &SkillSourceRoot, skill_path: &Path, skill_file: &Path) {
        let source = SkillSource {
            kind: root.kind,
            label: root.label.clone(),
            path: Some(skill_path.to_string_lossy().into_owned()),
            precedence: root.precedence,
        };
        let skill_file = skill_file.to_path_buf();
        let fallback_id = skill_fallback_id(skill_path, &skill_file);
        match parse_skill_file_with_fallback(
            &skill_file,
            &source,
            self.options.max_skill_file_bytes,
            fallback_id.as_deref(),
        ) {
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

fn find_skill_file(path: &Path) -> Option<PathBuf> {
    SKILL_FILE_NAMES
        .iter()
        .map(|file_name| path.join(file_name))
        .find(|candidate| candidate.is_file())
}

fn skill_fallback_id(skill_path: &Path, skill_file: &Path) -> Option<String> {
    if skill_path == skill_file {
        skill_file
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(ToString::to_string)
    } else {
        skill_path
            .file_name()
            .and_then(|name| name.to_str())
            .map(ToString::to_string)
    }
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
    let fallback_id = path.file_stem().and_then(|stem| stem.to_str());
    parse_skill_file_with_fallback(path, source, max_skill_file_bytes, fallback_id)
}

fn parse_skill_file_with_fallback(
    path: &Path,
    source: &SkillSource,
    max_skill_file_bytes: u64,
    fallback_id: Option<&str>,
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
    parse_skill_markdown(&contents, source, fallback_id)
}

fn parse_skill_markdown(
    contents: &str,
    source: &SkillSource,
    fallback_id: Option<&str>,
) -> Result<SkillManifest, SkillRegistryError> {
    let (frontmatter, instructions) = split_frontmatter(contents)?;
    let toml_frontmatter = yamlish_to_toml(frontmatter);
    let raw: RawSkillFrontmatter = toml::from_str(&toml_frontmatter)
        .map_err(|error| SkillError::InvalidMetadata(format!("{error}: {toml_frontmatter}")))?;
    let id = raw
        .id
        .clone()
        .or_else(|| fallback_id.map(ToString::to_string))
        .ok_or_else(|| SkillError::InvalidMetadata("missing skill id".to_string()))?;
    let name = raw.name.clone().unwrap_or_else(|| id.clone());
    let summary = SkillSummary {
        id: SkillId::from_str(&id)?,
        name,
        description: raw.description.clone(),
        version: raw.version.clone(),
        source: source.clone(),
        activation: raw.activation.clone().unwrap_or_default(),
        diagnostics: Vec::new(),
        disable_model_invocation: raw.disable_model_invocation
            || raw.disable_model_invocation_compat,
    };
    let permissions = normalize_permission_hints(&raw, source);
    let permission_policy = normalize_permission_policy(&raw, source);
    let model_policy = normalize_model_policy(&raw);
    Ok(SkillManifest {
        summary,
        permissions,
        permission_policy,
        model_policy,
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
            let raw_key = raw_key.trim().replace('-', "_");
            let value = raw_value.trim();
            if indent == 0 && value.is_empty() {
                current_section = Some(raw_key.clone());
                continue;
            }

            let key = if indent > 0 {
                current_section
                    .as_ref()
                    .map_or_else(|| raw_key.clone(), |section| format!("{section}.{raw_key}"))
            } else {
                current_section = None;
                raw_key.clone()
            };

            if value.is_empty() {
                current_array_key = Some(key);
            } else if value.starts_with('[') {
                output.push_str(&key);
                output.push_str(" = ");
                output.push_str(&toml_array(value));
                output.push('\n');
            } else if value == "true" || value == "false" {
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

fn toml_array(value: &str) -> String {
    let trimmed = value.trim();
    let Some(inner) = trimmed
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    else {
        return trimmed.to_string();
    };
    let items = split_compat_list_items(inner);
    let mut output = String::from("[");
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            output.push_str(", ");
        }
        let item = item.trim();
        if (item.starts_with('"') && item.ends_with('"'))
            || (item.starts_with('\'') && item.ends_with('\''))
        {
            output.push_str(&toml_string(item.trim_matches('"').trim_matches('\'')));
        } else {
            output.push_str(&toml_string(item));
        }
    }
    output.push(']');
    output
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

/// Resolve a canonical skill permission policy against the available tool catalog.
#[must_use]
pub fn resolve_skill_permission_policy(
    policy: &SkillPermissionPolicy,
    available_tools: &[ToolDefinition],
) -> ResolvedSkillPermissionPolicy {
    let mut selectors = Vec::new();
    let mut unknown_references = Vec::new();
    let mut ambiguous_references = Vec::new();

    for reference in &policy.tools {
        match bcode_tool::resolve_tool_reference(reference, available_tools) {
            ToolReferenceResolution::Resolved { selector } => selectors.push(selector),
            ToolReferenceResolution::Unknown { reference } => unknown_references.push(reference),
            ambiguous @ ToolReferenceResolution::Ambiguous { .. } => {
                ambiguous_references.push(ambiguous);
            }
        }
    }

    selectors.extend(
        policy
            .categories
            .iter()
            .cloned()
            .map(|category| ResolvedToolSelector::PermissionCategory { category }),
    );

    ResolvedSkillPermissionPolicy {
        mode: policy.mode,
        selectors,
        unknown_references,
        ambiguous_references,
    }
}

/// Evaluate one tool call against active resolved skill permission policies.
#[must_use]
pub fn evaluate_skill_tool_call(request: &SkillToolPolicyRequest) -> SkillToolPolicyOutcome {
    if request.active_policies.is_empty() {
        return SkillToolPolicyOutcome::NoOpinion;
    }

    let mut saw_allow = false;
    let mut warn_reasons = Vec::new();
    let mut ask_reasons = Vec::new();

    for policy in &request.active_policies {
        match evaluate_single_skill_tool_policy(policy, &request.tool) {
            SkillToolPolicyOutcome::NoOpinion => {}
            SkillToolPolicyOutcome::Allow { .. } => saw_allow = true,
            SkillToolPolicyOutcome::Warn { reason } => warn_reasons.push(reason),
            SkillToolPolicyOutcome::Ask { reason } => ask_reasons.push(reason),
            SkillToolPolicyOutcome::Deny { reason } => {
                return SkillToolPolicyOutcome::Deny { reason };
            }
        }
    }

    if !ask_reasons.is_empty() {
        return SkillToolPolicyOutcome::Ask {
            reason: ask_reasons.join("; "),
        };
    }
    if !warn_reasons.is_empty() {
        return SkillToolPolicyOutcome::Warn {
            reason: warn_reasons.join("; "),
        };
    }
    if saw_allow {
        return SkillToolPolicyOutcome::Allow {
            reason: "tool call matches active skill policy".to_string(),
        };
    }
    SkillToolPolicyOutcome::NoOpinion
}

fn evaluate_single_skill_tool_policy(
    policy: &ResolvedSkillPermissionPolicy,
    tool: &SkillToolPolicyTarget,
) -> SkillToolPolicyOutcome {
    if policy.mode == SkillPermissionMode::Disabled {
        return SkillToolPolicyOutcome::NoOpinion;
    }
    if policy.mode == SkillPermissionMode::Inherit
        && policy.selectors.is_empty()
        && policy.unknown_references.is_empty()
        && policy.ambiguous_references.is_empty()
    {
        return SkillToolPolicyOutcome::NoOpinion;
    }
    if !policy.unknown_references.is_empty() || !policy.ambiguous_references.is_empty() {
        return SkillToolPolicyOutcome::Ask {
            reason: "skill permission policy contains unresolved tool references".to_string(),
        };
    }
    if policy
        .selectors
        .iter()
        .any(|selector| selector_matches_tool(selector, tool))
    {
        return SkillToolPolicyOutcome::Allow {
            reason: "tool call matches skill permission policy".to_string(),
        };
    }

    match policy.mode {
        SkillPermissionMode::Disabled => SkillToolPolicyOutcome::NoOpinion,
        SkillPermissionMode::Strict => SkillToolPolicyOutcome::Deny {
            reason: "tool call is not declared by strict skill permission policy".to_string(),
        },
        SkillPermissionMode::Warn => SkillToolPolicyOutcome::Warn {
            reason: "tool call is not declared by skill permission policy".to_string(),
        },
        SkillPermissionMode::Inherit | SkillPermissionMode::Enforce | SkillPermissionMode::Ask => {
            SkillToolPolicyOutcome::Ask {
                reason: "tool call is not declared by skill permission policy".to_string(),
            }
        }
    }
}

fn selector_matches_tool(selector: &ResolvedToolSelector, tool: &SkillToolPolicyTarget) -> bool {
    match selector {
        ResolvedToolSelector::ToolName { name } => tool.name == *name,
        ResolvedToolSelector::Alias { alias } => tool.aliases.iter().any(|value| value == alias),
        ResolvedToolSelector::CompatibilityAlias { ecosystem, name } => tool
            .compatibility_aliases
            .iter()
            .any(|alias| alias.ecosystem.eq_ignore_ascii_case(ecosystem) && alias.name == *name),
        ResolvedToolSelector::PermissionCategory { category } => tool
            .permission_category
            .as_ref()
            .is_some_and(|tool_category| tool_category == category),
        ResolvedToolSelector::Capability { capability } => {
            tool.capabilities.iter().any(|value| value == capability)
        }
    }
}

#[derive(Debug, Default, serde::Deserialize)]
struct RawSkillFrontmatter {
    id: Option<String>,
    name: Option<String>,
    description: Option<String>,
    version: Option<String>,
    activation: Option<SkillActivation>,
    permissions: Option<RawSkillPermissions>,
    #[serde(default, alias = "allowed-tools", rename = "allowed_tools")]
    allowed_tools: RawStringList,
    #[serde(default)]
    tools: RawStringList,
    model: Option<RawSkillModelField>,
    #[serde(default)]
    models: RawSkillModels,
    preferred_model: Option<String>,
    required_model: Option<String>,
    thinking_effort: Option<String>,
    reasoning_effort: Option<String>,
    #[serde(default)]
    reasoning: RawSkillReasoning,
    #[serde(default)]
    disable_model_invocation: bool,
    #[serde(default, rename = "disable-model-invocation")]
    disable_model_invocation_compat: bool,
}

#[derive(Debug, Default, Clone, serde::Deserialize)]
struct RawSkillPermissions {
    #[serde(default)]
    tools: RawStringList,
    #[serde(default)]
    categories: RawStringList,
    mode: Option<SkillPermissionMode>,
}

#[derive(Debug, Default, Clone)]
struct RawStringList(Vec<String>);

impl RawStringList {
    fn values(&self) -> Vec<String> {
        self.0.clone()
    }
}

impl<'de> serde::Deserialize<'de> for RawStringList {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum StringListValue {
            One(String),
            Many(Vec<String>),
        }

        let value = StringListValue::deserialize(deserializer)?;
        let values = match value {
            StringListValue::One(value) => split_compat_string_list(&value),
            StringListValue::Many(values) => values
                .into_iter()
                .flat_map(|value| split_compat_string_list(&value))
                .collect(),
        };
        Ok(Self(values))
    }
}

fn split_compat_string_list(value: &str) -> Vec<String> {
    split_compat_list_items(value)
        .into_iter()
        .map(|item| item.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

fn split_compat_list_items(value: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut paren_depth = 0_u32;
    let mut bracket_depth = 0_u32;
    let mut quote: Option<char> = None;
    for character in value.chars() {
        match (character, quote) {
            ('"' | '\'', None) => {
                quote = Some(character);
                current.push(character);
            }
            (value, Some(quote_character)) if value == quote_character => {
                quote = None;
                current.push(character);
            }
            ('(', None) => {
                paren_depth = paren_depth.saturating_add(1);
                current.push(character);
            }
            (')', None) => {
                paren_depth = paren_depth.saturating_sub(1);
                current.push(character);
            }
            ('[', None) => {
                bracket_depth = bracket_depth.saturating_add(1);
                current.push(character);
            }
            (']', None) => {
                bracket_depth = bracket_depth.saturating_sub(1);
                current.push(character);
            }
            (',', None) if paren_depth == 0 && bracket_depth == 0 => {
                items.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(character),
        }
    }
    if !current.trim().is_empty() {
        items.push(current.trim().to_string());
    }
    items
}

#[derive(Debug, Default, Clone, serde::Deserialize)]
struct RawSkillModels {
    preferred: Option<RawSkillModelField>,
    required: Option<RawSkillModelField>,
}

#[derive(Debug, Default, Clone, serde::Deserialize)]
struct RawSkillReasoning {
    effort: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
enum RawSkillModelField {
    Name(String),
    Request(RawSkillModelRequest),
}

#[derive(Debug, Clone, serde::Deserialize)]
struct RawSkillModelRequest {
    model: String,
    provider: Option<String>,
    thinking_effort: Option<String>,
    reasoning_effort: Option<String>,
}

impl RawSkillPermissions {
    fn to_hints(&self) -> SkillPermissionHints {
        SkillPermissionHints {
            tools: self.tools.values(),
            categories: self.categories.values(),
            unresolved_tools: Vec::new(),
        }
    }
}

fn normalize_permission_hints(
    raw: &RawSkillFrontmatter,
    source: &SkillSource,
) -> SkillPermissionHints {
    let mut hints = raw
        .permissions
        .as_ref()
        .map_or_else(SkillPermissionHints::default, RawSkillPermissions::to_hints);
    hints.tools.extend(raw.allowed_tools.values());
    hints.tools.extend(raw.tools.values());
    hints.unresolved_tools = hints
        .tools
        .iter()
        .map(|tool| unresolved_tool_reference(tool, source_default_ecosystem(source)))
        .collect();
    hints
}

fn normalize_permission_policy(
    raw: &RawSkillFrontmatter,
    source: &SkillSource,
) -> SkillPermissionPolicy {
    let hints = normalize_permission_hints(raw, source);
    SkillPermissionPolicy {
        mode: raw
            .permissions
            .as_ref()
            .and_then(|permissions| permissions.mode)
            .unwrap_or_default(),
        tools: hints.unresolved_tools,
        categories: hints.categories,
    }
}

fn unresolved_tool_reference(
    tool: &str,
    default_ecosystem: Option<&'static str>,
) -> bcode_tool::UnresolvedToolReference {
    if let Some((prefix, value)) = tool.split_once(':') {
        match prefix {
            "category" | "capability" => bcode_tool::UnresolvedToolReference::raw(tool),
            _ => bcode_tool::UnresolvedToolReference::compatibility_alias(prefix, value),
        }
    } else if let Some(ecosystem) = default_ecosystem {
        bcode_tool::UnresolvedToolReference::compatibility_alias(ecosystem, tool)
    } else {
        bcode_tool::UnresolvedToolReference::raw(tool)
    }
}

fn normalize_model_policy(raw: &RawSkillFrontmatter) -> SkillModelPolicy {
    let top_level_effort = raw
        .thinking_effort
        .as_deref()
        .or(raw.reasoning_effort.as_deref())
        .or(raw.reasoning.effort.as_deref());
    let preferred = raw
        .models
        .preferred
        .clone()
        .or_else(|| raw.preferred_model.clone().map(RawSkillModelField::Name))
        .or_else(|| {
            raw.model
                .clone()
                .filter(|_| raw.required_model.is_none() && raw.models.required.is_none())
        });
    let required = raw
        .models
        .required
        .clone()
        .or_else(|| raw.required_model.clone().map(RawSkillModelField::Name));
    SkillModelPolicy {
        preferred: preferred
            .as_ref()
            .map(|request| normalize_model_request(request, top_level_effort)),
        required: required
            .as_ref()
            .map(|request| normalize_model_request(request, top_level_effort)),
    }
}

fn normalize_model_request(
    request: &RawSkillModelField,
    top_level_effort: Option<&str>,
) -> SkillModelRequest {
    match request {
        RawSkillModelField::Name(model) => SkillModelRequest {
            model: model.clone(),
            provider: None,
            thinking_effort: top_level_effort.map(normalize_thinking_effort),
        },
        RawSkillModelField::Request(request) => {
            let effort = request
                .thinking_effort
                .as_deref()
                .or(request.reasoning_effort.as_deref())
                .or(top_level_effort);
            SkillModelRequest {
                model: request.model.clone(),
                provider: request.provider.clone(),
                thinking_effort: effort.map(normalize_thinking_effort),
            }
        }
    }
}

fn normalize_thinking_effort(value: &str) -> SkillThinkingEffort {
    let normalized_level = match value {
        "minimal" => Some(GenericThinkingEffort::Minimal),
        "low" => Some(GenericThinkingEffort::Low),
        "medium" => Some(GenericThinkingEffort::Medium),
        "high" => Some(GenericThinkingEffort::High),
        _ => None,
    };
    SkillThinkingEffort {
        source_label: value.to_string(),
        normalized_level,
        provider_value: normalized_level.is_none().then(|| value.to_string()),
    }
}

fn source_default_ecosystem(source: &SkillSource) -> Option<&'static str> {
    let path = source.path.as_deref()?;
    if path.contains(".claude") {
        Some("claude")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn tool_with_policy(name: &str, policy: bcode_tool::ToolPolicyMetadata) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: String::new(),
            input_schema: serde_json::Value::default(),
            side_effect: bcode_tool::ToolSideEffect::ReadOnly,
            requires_permission: false,
            policy,
            ui: bcode_tool::ToolUiMetadata::default(),
        }
    }

    fn basic_tool(name: &str) -> ToolDefinition {
        tool_with_policy(name, bcode_tool::ToolPolicyMetadata::default())
    }

    fn policy_with_selector(
        mode: SkillPermissionMode,
        selector: ResolvedToolSelector,
    ) -> ResolvedSkillPermissionPolicy {
        ResolvedSkillPermissionPolicy {
            mode,
            selectors: vec![selector],
            unknown_references: Vec::new(),
            ambiguous_references: Vec::new(),
        }
    }

    fn evaluate(
        tool: ToolDefinition,
        active_policies: Vec<ResolvedSkillPermissionPolicy>,
    ) -> SkillToolPolicyOutcome {
        evaluate_skill_tool_call(&SkillToolPolicyRequest {
            tool: tool.into(),
            active_policies,
        })
    }

    fn simple_identifier() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9_]{0,12}".prop_map(String::from)
    }

    fn is_allow(outcome: &SkillToolPolicyOutcome) -> bool {
        matches!(outcome, SkillToolPolicyOutcome::Allow { .. })
    }

    fn is_ask(outcome: &SkillToolPolicyOutcome) -> bool {
        matches!(outcome, SkillToolPolicyOutcome::Ask { .. })
    }

    proptest! {
        #[test]
        fn tool_name_selector_allows_exact_tool_name_and_asks_otherwise(
            tool_name in simple_identifier(),
            requested_name in simple_identifier(),
        ) {
            let outcome = evaluate(
                basic_tool(&tool_name),
                vec![policy_with_selector(
                    SkillPermissionMode::Ask,
                    ResolvedToolSelector::ToolName { name: requested_name.clone() },
                )],
            );

            if tool_name == requested_name {
                prop_assert!(is_allow(&outcome));
            } else {
                prop_assert!(is_ask(&outcome));
            }
        }

        #[test]
        fn alias_selector_matches_only_declared_aliases(
            alias in simple_identifier(),
            requested_alias in simple_identifier(),
        ) {
            let tool = tool_with_policy(
                "tool.run",
                bcode_tool::ToolPolicyMetadata {
                    aliases: vec![alias.clone()],
                    compatibility_aliases: Vec::new(),
                    capabilities: Vec::new(),
                    permission_category: None,
                    argument_extractors: Vec::new(),
                },
            );
            let outcome = evaluate(
                tool,
                vec![policy_with_selector(
                    SkillPermissionMode::Ask,
                    ResolvedToolSelector::Alias { alias: requested_alias.clone() },
                )],
            );

            if alias == requested_alias {
                prop_assert!(is_allow(&outcome));
            } else {
                prop_assert!(is_ask(&outcome));
            }
        }

        #[test]
        fn category_selector_matches_only_tool_permission_category(
            category in simple_identifier(),
            requested_category in simple_identifier(),
        ) {
            let tool = tool_with_policy(
                "tool.run",
                bcode_tool::ToolPolicyMetadata {
                    aliases: Vec::new(),
                    compatibility_aliases: Vec::new(),
                    capabilities: Vec::new(),
                    permission_category: Some(category.clone()),
                    argument_extractors: Vec::new(),
                },
            );
            let outcome = evaluate(
                tool,
                vec![policy_with_selector(
                    SkillPermissionMode::Ask,
                    ResolvedToolSelector::PermissionCategory { category: requested_category.clone() },
                )],
            );

            if category == requested_category {
                prop_assert!(is_allow(&outcome));
            } else {
                prop_assert!(is_ask(&outcome));
            }
        }

        #[test]
        fn capability_selector_matches_only_declared_capabilities(
            capability in simple_identifier(),
            requested_capability in simple_identifier(),
        ) {
            let tool = tool_with_policy(
                "tool.run",
                bcode_tool::ToolPolicyMetadata {
                    aliases: Vec::new(),
                    compatibility_aliases: Vec::new(),
                    capabilities: vec![capability.clone()],
                    permission_category: None,
                    argument_extractors: Vec::new(),
                },
            );
            let outcome = evaluate(
                tool,
                vec![policy_with_selector(
                    SkillPermissionMode::Ask,
                    ResolvedToolSelector::Capability { capability: requested_capability.clone() },
                )],
            );

            if capability == requested_capability {
                prop_assert!(is_allow(&outcome));
            } else {
                prop_assert!(is_ask(&outcome));
            }
        }

        #[test]
        fn compatibility_alias_selector_matches_declared_alias_pair_case_insensitive_ecosystem(
            ecosystem in simple_identifier(),
            alias_name in simple_identifier(),
            requested_name in simple_identifier(),
        ) {
            let requested_ecosystem = ecosystem.to_uppercase();
            let tool = tool_with_policy(
                "tool.run",
                bcode_tool::ToolPolicyMetadata {
                    aliases: Vec::new(),
                    compatibility_aliases: vec![bcode_tool::ToolCompatibilityAlias::new(&ecosystem, &alias_name)],
                    capabilities: Vec::new(),
                    permission_category: None,
                    argument_extractors: Vec::new(),
                },
            );
            let outcome = evaluate(
                tool,
                vec![policy_with_selector(
                    SkillPermissionMode::Ask,
                    ResolvedToolSelector::CompatibilityAlias {
                        ecosystem: requested_ecosystem,
                        name: requested_name.clone(),
                    },
                )],
            );

            if alias_name == requested_name {
                prop_assert!(is_allow(&outcome));
            } else {
                prop_assert!(is_ask(&outcome));
            }
        }

        #[test]
        fn explicit_category_references_resolve_without_catalog(category in simple_identifier()) {
            let policy = SkillPermissionPolicy {
                mode: SkillPermissionMode::Enforce,
                tools: vec![bcode_tool::UnresolvedToolReference::raw(format!("category:{category}"))],
                categories: Vec::new(),
            };
            let resolved = resolve_skill_permission_policy(&policy, &[]);

            prop_assert_eq!(resolved.unknown_references, Vec::new());
            prop_assert_eq!(resolved.ambiguous_references, Vec::new());
            prop_assert_eq!(resolved.selectors, vec![ResolvedToolSelector::PermissionCategory { category }]);
        }
    }

    #[test]
    fn no_active_policies_have_no_opinion() {
        let outcome = evaluate(basic_tool("tool.run"), Vec::new());
        assert!(matches!(outcome, SkillToolPolicyOutcome::NoOpinion));
    }

    #[test]
    fn disabled_policy_has_no_opinion_even_with_unresolved_references() {
        let outcome = evaluate(
            basic_tool("tool.run"),
            vec![ResolvedSkillPermissionPolicy {
                mode: SkillPermissionMode::Disabled,
                selectors: Vec::new(),
                unknown_references: vec![bcode_tool::UnresolvedToolReference::raw("missing")],
                ambiguous_references: Vec::new(),
            }],
        );
        assert!(matches!(outcome, SkillToolPolicyOutcome::NoOpinion));
    }

    #[test]
    fn unknown_references_ask_before_matching() {
        let outcome = evaluate(
            basic_tool("tool.run"),
            vec![ResolvedSkillPermissionPolicy {
                mode: SkillPermissionMode::Ask,
                selectors: vec![ResolvedToolSelector::ToolName {
                    name: "tool.run".to_string(),
                }],
                unknown_references: vec![bcode_tool::UnresolvedToolReference::raw("missing")],
                ambiguous_references: Vec::new(),
            }],
        );
        assert!(matches!(outcome, SkillToolPolicyOutcome::Ask { .. }));
    }

    #[test]
    fn strict_undeclared_tool_denies() {
        let outcome = evaluate(
            basic_tool("tool.run"),
            vec![ResolvedSkillPermissionPolicy {
                mode: SkillPermissionMode::Strict,
                selectors: Vec::new(),
                unknown_references: Vec::new(),
                ambiguous_references: Vec::new(),
            }],
        );
        assert!(matches!(outcome, SkillToolPolicyOutcome::Deny { .. }));
    }

    #[test]
    fn warn_undeclared_tool_warns() {
        let outcome = evaluate(
            basic_tool("tool.run"),
            vec![ResolvedSkillPermissionPolicy {
                mode: SkillPermissionMode::Warn,
                selectors: Vec::new(),
                unknown_references: Vec::new(),
                ambiguous_references: Vec::new(),
            }],
        );
        assert!(matches!(outcome, SkillToolPolicyOutcome::Warn { .. }));
    }

    #[test]
    fn deny_dominates_multiple_policies() {
        let outcome = evaluate(
            basic_tool("tool.run"),
            vec![
                policy_with_selector(
                    SkillPermissionMode::Ask,
                    ResolvedToolSelector::ToolName {
                        name: "tool.run".to_string(),
                    },
                ),
                ResolvedSkillPermissionPolicy {
                    mode: SkillPermissionMode::Strict,
                    selectors: Vec::new(),
                    unknown_references: Vec::new(),
                    ambiguous_references: Vec::new(),
                },
            ],
        );
        assert!(matches!(outcome, SkillToolPolicyOutcome::Deny { .. }));
    }

    #[test]
    fn ambiguous_alias_resolution_is_not_silent_allow() {
        let tool_a = tool_with_policy(
            "tool.a",
            bcode_tool::ToolPolicyMetadata {
                aliases: vec!["same".to_string()],
                compatibility_aliases: Vec::new(),
                capabilities: Vec::new(),
                permission_category: None,
                argument_extractors: Vec::new(),
            },
        );
        let tool_b = tool_with_policy(
            "tool.b",
            bcode_tool::ToolPolicyMetadata {
                aliases: vec!["same".to_string()],
                compatibility_aliases: Vec::new(),
                capabilities: Vec::new(),
                permission_category: None,
                argument_extractors: Vec::new(),
            },
        );
        let policy = SkillPermissionPolicy {
            mode: SkillPermissionMode::Enforce,
            tools: vec![bcode_tool::UnresolvedToolReference::raw("same")],
            categories: Vec::new(),
        };
        let resolved = resolve_skill_permission_policy(&policy, &[tool_a.clone(), tool_b]);
        let outcome = evaluate(tool_a, vec![resolved]);

        assert!(matches!(outcome, SkillToolPolicyOutcome::Ask { .. }));
    }

    #[test]
    fn parses_inline_compat_allowed_tools() {
        let source = SkillSource {
            kind: SkillSourceKind::User,
            label: "user-config:skills".to_string(),
            path: None,
            precedence: 30,
        };
        let frontmatter = "name: commit-message-staged-write
description: Generate a commit message from staged changes only
allowed-tools: Bash(git:*), Read(*), Edit(*)";
        let manifest = parse_skill_markdown(
            &format!("---\n{frontmatter}\n---\n# Instructions\nDo things."),
            &source,
            Some("commit-message-staged-write"),
        )
        .expect("compat skill parses");

        assert_eq!(
            yamlish_to_toml(frontmatter),
            "name = \"commit-message-staged-write\"\ndescription = \"Generate a commit message from staged changes only\"\nallowed_tools = \"Bash(git:*), Read(*), Edit(*)\"\n"
        );
        assert_eq!(manifest.summary.id.as_str(), "commit-message-staged-write");
        assert_eq!(
            manifest.permissions.tools,
            vec!["Bash(git:*)", "Read(*)", "Edit(*)"]
        );
    }

    #[test]
    fn parses_json_style_compat_allowed_tools() {
        let source = SkillSource {
            kind: SkillSourceKind::User,
            label: "user-config:skills".to_string(),
            path: None,
            precedence: 30,
        };
        let manifest = parse_skill_markdown(
            "---
name: compat-list
allowed-tools: [Bash(git:*), Read(*), \"Edit(*)\"]
---
# Instructions
Do things.",
            &source,
            Some("compat-list"),
        )
        .expect("compat skill parses");

        assert_eq!(
            manifest.permissions.tools,
            vec!["Bash(git:*)", "Read(*)", "Edit(*)"]
        );
    }

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
            None,
        )
        .expect("skill parses");

        assert_eq!(manifest.summary.id.as_str(), "rust-debugging");
        assert_eq!(manifest.summary.activation.keywords, vec!["rust", "cargo"]);
        assert_eq!(manifest.permissions.tools, vec!["shell.run"]);
        assert_eq!(manifest.instructions, "# Instructions\nDo things.");
    }
    #[test]
    fn infers_directory_skill_id_from_parent_directory() {
        let skill_path = Path::new("skills/commit-message");
        let skill_file = Path::new("skills/commit-message/SKILL.md");

        assert_eq!(
            skill_fallback_id(skill_path, skill_file).as_deref(),
            Some("commit-message")
        );
    }

    #[test]
    fn infers_flat_markdown_skill_id_from_file_name() {
        let source = SkillSource {
            kind: SkillSourceKind::Repository,
            label: "repo:skills".to_string(),
            path: None,
            precedence: 15,
        };
        let manifest = parse_skill_markdown(
            "---
name: Code Review
activation:
  keywords:
    - review
---
# Review
Check the code.",
            &source,
            Some("code-review"),
        )
        .expect("skill parses");

        assert_eq!(manifest.summary.id.as_str(), "code-review");
        assert_eq!(manifest.summary.name, "Code Review");
    }
}
