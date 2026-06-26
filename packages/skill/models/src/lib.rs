#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Shared skill models for Bcode.
//!
//! This crate intentionally contains only leaf data types for the skill domain.

use bcode_tool::UnresolvedToolReference;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::str::FromStr;

/// Plugin service interface for skill providers.
pub const SKILL_INTERFACE_ID: &str = "bcode.skill/v1";

/// Operation to list compact skill summaries.
pub const OP_LIST_SKILLS: &str = "list";

/// Operation to describe one skill.
pub const OP_DESCRIBE_SKILL: &str = "describe";

/// Operation to load bounded skill model context.
pub const OP_SKILL_CONTEXT: &str = "context";

/// Operation to invoke optional plugin-backed skill behavior.
pub const OP_INVOKE_SKILL: &str = "invoke";

/// Stable skill identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SkillId(pub String);

impl SkillId {
    /// Create a skill ID without validation.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Return the string representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for SkillId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for SkillId {
    type Err = SkillError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.trim().is_empty() {
            Err(SkillError::InvalidSkillId(value.to_string()))
        } else {
            Ok(Self(value.to_string()))
        }
    }
}

/// Source kind for a discovered skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSourceKind {
    /// Repository-local `.bcode/skills` source.
    Repository,
    /// Compatibility source such as `.claude/skills`.
    Compatibility,
    /// User-level configuration directory source.
    User,
    /// Explicitly configured path source.
    Configured,
    /// Bundled Bcode or plugin distribution source.
    Bundled,
    /// Plugin-provided virtual source.
    Plugin,
}

/// Provenance for a discovered skill.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SkillSource {
    pub kind: SkillSourceKind,
    pub label: String,
    #[serde(default)]
    pub path: Option<String>,
    pub precedence: u16,
}

/// Activation metadata used for suggestions and auto-activation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillActivation {
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub file_patterns: Vec<String>,
}

/// Advisory permission metadata declared by a skill.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillPermissionHints {
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub unresolved_tools: Vec<UnresolvedToolReference>,
}

/// Canonical permission policy requested by a skill.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillPermissionPolicy {
    #[serde(default)]
    pub mode: SkillPermissionMode,
    #[serde(default)]
    pub tools: Vec<UnresolvedToolReference>,
    #[serde(default)]
    pub categories: Vec<String>,
}

/// Skill-requested permission enforcement mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillPermissionMode {
    /// Use configured global/per-skill defaults.
    #[default]
    Inherit,
    /// Enforce declared skill permissions.
    Enforce,
    /// Ask before undeclared tool use.
    Ask,
    /// Warn but continue when undeclared tools are requested.
    Warn,
    /// Block undeclared tool use and do not allow user override unless config explicitly permits it.
    Strict,
    /// Disable skill permission enforcement for this skill.
    Disabled,
}

/// Resolved skill permission policy ready for tool-call evaluation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSkillPermissionPolicy {
    pub mode: SkillPermissionMode,
    #[serde(default)]
    pub selectors: Vec<bcode_tool::ResolvedToolSelector>,
    #[serde(default)]
    pub unknown_references: Vec<UnresolvedToolReference>,
    #[serde(default)]
    pub ambiguous_references: Vec<bcode_tool::ToolReferenceResolution>,
}

/// Request to evaluate one tool call against active skill policies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillToolPolicyRequest {
    pub tool: bcode_tool::ToolDefinition,
    #[serde(default)]
    pub active_policies: Vec<ResolvedSkillPermissionPolicy>,
}

/// Skill-owned tool policy outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum SkillToolPolicyOutcome {
    /// Skills have no opinion about this call.
    NoOpinion,
    /// The tool call is allowed by skill policy.
    Allow { reason: String },
    /// The tool call should continue with a warning.
    Warn { reason: String },
    /// The host should ask the user before continuing.
    Ask { reason: String },
    /// The tool call is denied by skill policy.
    Deny { reason: String },
}

/// Canonical model policy requested by a skill.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillModelPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred: Option<SkillModelRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<SkillModelRequest>,
}

/// Skill-requested model selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillModelRequest {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<SkillThinkingEffort>,
}

/// Skill-requested thinking/reasoning effort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillThinkingEffort {
    pub source_label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized_level: Option<GenericThinkingEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_value: Option<String>,
}

/// Provider-independent thinking effort labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenericThinkingEffort {
    Minimal,
    Low,
    Medium,
    High,
}

/// Compact skill metadata used for listing and matching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSummary {
    pub id: SkillId,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    pub source: SkillSource,
    #[serde(default)]
    pub activation: SkillActivation,
    #[serde(default)]
    pub diagnostics: Vec<SkillDiagnostic>,
    #[serde(default)]
    pub disable_model_invocation: bool,
}

/// Full validated skill manifest plus markdown instructions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillManifest {
    pub summary: SkillSummary,
    #[serde(default)]
    pub permissions: SkillPermissionHints,
    #[serde(default)]
    pub permission_policy: SkillPermissionPolicy,
    #[serde(default)]
    pub model_policy: SkillModelPolicy,
    pub instructions: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Diagnostic emitted while discovering or loading skills.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDiagnostic {
    pub severity: SkillDiagnosticSeverity,
    pub message: String,
    #[serde(default)]
    pub path: Option<String>,
}

/// Diagnostic severity for skill loading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillDiagnosticSeverity {
    Info,
    Warning,
    Error,
}

/// Response to `OP_LIST_SKILLS`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillList {
    pub skills: Vec<SkillSummary>,
    #[serde(default)]
    pub diagnostics: Vec<SkillDiagnostic>,
}

/// Request to describe a skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DescribeSkillRequest {
    pub skill_id: SkillId,
}

/// Request to load active skill context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillContextRequest {
    pub skill_id: SkillId,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

/// Bounded skill context for model prompt injection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillContextResponse {
    pub skill_id: SkillId,
    pub context: String,
    pub source: SkillSource,
    pub bytes_loaded: usize,
    pub truncated: bool,
}

/// Skill activation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillActivationMode {
    Explicit,
    Suggested,
    Automatic,
}

/// Request to invoke optional plugin-backed skill behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvokeSkillRequest {
    pub skill_id: SkillId,
    #[serde(default)]
    pub args: BTreeMap<String, String>,
    pub mode: SkillActivationMode,
}

/// Response from skill invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvokeSkillResponse {
    pub success: bool,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub activated: bool,
}

/// Errors returned by skill operations.
///
/// * Invalid skill IDs or malformed metadata.
/// * Unknown or disabled skills.
/// * Context budget or provider failures.
#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
pub enum SkillError {
    #[error("invalid skill id: {0}")]
    InvalidSkillId(String),
    #[error("unknown skill: {0}")]
    UnknownSkill(String),
    #[error("skill is disabled: {0}")]
    DisabledSkill(String),
    #[error("invalid skill metadata: {0}")]
    InvalidMetadata(String),
    #[error("skill context budget exceeded: {0}")]
    ContextBudgetExceeded(String),
    #[error("skill execution failed: {0}")]
    ExecutionFailed(String),
}

/// Remembered decision for a skill-governed tool permission check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillToolDecision {
    /// Allow the matching skill/tool combination without another skill-policy prompt.
    Allow,
    /// Deny the matching skill/tool combination without another skill-policy prompt.
    Deny,
}

/// Scope where a remembered skill tool decision applies.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillToolDecisionScope {
    /// Applies across Bcode state.
    Global,
    /// Applies only for the identified workspace.
    Workspace { workspace_id: String },
}

/// Stable lookup key for remembered skill tool decisions.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SkillToolDecisionKey {
    /// Sorted set of active skills that produced the permission policy.
    pub skill_ids: BTreeSet<SkillId>,
    /// Canonical service tool name.
    pub tool_name: String,
    /// Decision applicability scope.
    pub scope: SkillToolDecisionScope,
}

/// One remembered skill tool policy decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillToolDecisionEntry {
    /// Lookup key.
    pub key: SkillToolDecisionKey,
    /// Remembered decision.
    pub decision: SkillToolDecision,
    /// Unix epoch milliseconds when the decision was remembered.
    pub remembered_at_ms: u64,
    /// Optional human-readable note explaining why this was remembered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Persisted skill tool decision state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillToolDecisionState {
    /// Remembered decisions.
    #[serde(default)]
    pub decisions: Vec<SkillToolDecisionEntry>,
}

impl SkillToolDecisionState {
    /// Return the remembered decision matching `key`, if any.
    #[must_use]
    pub fn decision_for(&self, key: &SkillToolDecisionKey) -> Option<&SkillToolDecisionEntry> {
        self.decisions.iter().find(|entry| &entry.key == key)
    }

    /// Upsert one remembered decision, replacing any existing entry with the same key.
    pub fn upsert(&mut self, entry: SkillToolDecisionEntry) {
        self.decisions.retain(|existing| existing.key != entry.key);
        self.decisions.push(entry);
        self.decisions
            .sort_by(|left, right| left.key.cmp(&right.key));
    }
}
