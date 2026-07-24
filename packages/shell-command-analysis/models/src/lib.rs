#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Stable, parser-independent shell command analysis contracts.
//!
//! These types intentionally describe only policy-relevant shell behavior. They are not a mirror
//! of any parser's abstract syntax tree.

use serde::{Deserialize, Serialize};

/// Current serialized shell analysis schema version.
pub const SHELL_ANALYSIS_SCHEMA_VERSION: u32 = 1;

/// Shell language selected for analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ShellDialect {
    /// POSIX shell syntax compatible with Bcode's Unix `sh` execution path.
    Posix,
}

/// Half-open UTF-8 byte range in the original shell source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellSourceSpan {
    /// Inclusive start byte offset.
    pub start: u32,
    /// Exclusive end byte offset.
    pub end: u32,
}

impl ShellSourceSpan {
    /// Construct a source span.
    #[must_use]
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    /// Return the span length in bytes.
    #[must_use]
    pub const fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    /// Return whether the span is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

/// Bounded resource limits applied to one analysis request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellAnalysisLimits {
    /// Maximum source length in bytes.
    pub max_source_bytes: u32,
    /// Maximum parser/adapter nodes visited.
    pub max_nodes: u32,
    /// Maximum shell syntax nesting depth.
    pub max_nesting_depth: u16,
    /// Maximum nested substitutions traversed.
    pub max_substitutions: u16,
    /// Maximum executable commands extracted.
    pub max_commands: u32,
    /// Maximum redirections extracted.
    pub max_redirections: u32,
}

impl Default for ShellAnalysisLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 256 * 1024,
            max_nodes: 16_384,
            max_nesting_depth: 128,
            max_substitutions: 512,
            max_commands: 4_096,
            max_redirections: 4_096,
        }
    }
}

/// Request to analyze one complete shell program.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellAnalysisRequest {
    /// Contract schema version expected by the caller.
    pub schema_version: u32,
    /// Exact source passed to the shell.
    pub source: String,
    /// Execution dialect.
    pub dialect: ShellDialect,
    /// Resource limits for this request.
    #[serde(default)]
    pub limits: ShellAnalysisLimits,
}

impl ShellAnalysisRequest {
    /// Construct a POSIX analysis request with default limits.
    #[must_use]
    pub fn posix(source: impl Into<String>) -> Self {
        Self {
            schema_version: SHELL_ANALYSIS_SCHEMA_VERSION,
            source: source.into(),
            dialect: ShellDialect::Posix,
            limits: ShellAnalysisLimits::default(),
        }
    }
}

/// Stable identifier for one extracted executable command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ShellCommandId(pub u32);

/// Parent syntax relationship for an extracted command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellCommandRelation {
    /// Top-level command.
    Root,
    /// Child of a grouped command or control-flow construct.
    Nested,
    /// Command contained in a command substitution.
    CommandSubstitution,
    /// Command contained in a process substitution.
    ProcessSubstitution,
    /// Command in a recursively analyzed literal nested shell script.
    NestedShell,
}

/// A policy-relevant shell word.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShellWord {
    /// A statically known word value.
    Static {
        /// Expanded literal value suitable for identity analysis.
        value: String,
        /// Source range for the original word.
        span: ShellSourceSpan,
    },
    /// A word whose complete value is not statically known.
    Dynamic {
        /// Expansion kinds that make the value dynamic.
        expansions: Vec<ShellExpansionKind>,
        /// Source range for the original word.
        span: ShellSourceSpan,
    },
}

impl ShellWord {
    /// Return this word's source span.
    #[must_use]
    pub const fn span(&self) -> ShellSourceSpan {
        match self {
            Self::Static { span, .. } | Self::Dynamic { span, .. } => *span,
        }
    }
}

/// Expansion that prevents a shell word from being fully static.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ShellExpansionKind {
    /// Parameter expansion such as `$name`.
    Parameter,
    /// Command substitution such as `$(command)`.
    Command,
    /// Arithmetic expansion.
    Arithmetic,
    /// Tilde expansion dependent on runtime environment.
    Tilde,
    /// Process substitution.
    Process,
    /// Glob or pattern expansion.
    Pattern,
}

/// Assignment prefix associated with a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellAssignment {
    /// Assignment variable name.
    pub name: String,
    /// Assignment value, which may be dynamic.
    pub value: ShellWord,
    /// Source range for the complete assignment.
    pub span: ShellSourceSpan,
}

/// Position of a command within its pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellPipelinePosition {
    /// Zero-based command index in the pipeline.
    pub index: u16,
    /// Number of commands in the pipeline.
    pub count: u16,
    /// Whether pipeline output is negated with `!`.
    pub negated: bool,
}

/// Control-flow and nesting context for an executable command.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellCommandContext {
    /// Pipeline membership, when present.
    pub pipeline: Option<ShellPipelinePosition>,
    /// Whether execution occurs in a conditional position or branch.
    pub conditional: bool,
    /// Whether execution is asynchronous via `&`.
    pub background: bool,
    /// Number of enclosing loops.
    pub loop_depth: u16,
    /// Number of enclosing command/process substitutions.
    pub substitution_depth: u16,
}

/// Origin of a command policy match candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellCommandMatchCandidateKind {
    /// Exact original command source slice.
    Original,
    /// Conservatively canonicalized syntax.
    Canonical,
    /// Reviewed command-domain alias.
    DomainAlias,
}

/// Candidate shell subject evaluated against wildcard command rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellCommandMatchCandidate {
    /// Candidate subject text.
    pub subject: String,
    /// Candidate origin.
    pub kind: ShellCommandMatchCandidateKind,
    /// Human-readable transformation label for diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transformation: Option<String>,
}

/// One independently executable shell command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellCommand {
    /// Stable identifier within this analysis result.
    pub id: ShellCommandId,
    /// Parent command when this command is nested inside another command's word or script.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<ShellCommandId>,
    /// Relationship to the parent syntax node.
    pub relation: ShellCommandRelation,
    /// Exact source range for this command subject.
    pub span: ShellSourceSpan,
    /// Exact source slice for diagnostics.
    pub source: String,
    /// Executable word.
    pub executable: ShellWord,
    /// Argument words, excluding assignment prefixes and redirections.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<ShellWord>,
    /// Environment/variable assignments preceding the executable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignments: Vec<ShellAssignment>,
    /// Execution context.
    pub context: ShellCommandContext,
    /// Ordered policy match candidates, always retaining the original candidate.
    pub match_candidates: Vec<ShellCommandMatchCandidate>,
}

/// Shell redirection kind relevant to filesystem policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ShellRedirectionKind {
    /// Read from a file.
    Input,
    /// Open/truncate a file for output.
    OutputTruncate,
    /// Open a file for appended output.
    OutputAppend,
    /// Read and write a file.
    InputOutput,
    /// Duplicate an existing file descriptor.
    Duplicate,
    /// Close a file descriptor.
    Close,
    /// Here-document input.
    HereDocument,
    /// Here-string input (non-POSIX extension).
    HereString,
}

/// Source or destination file descriptor in a redirection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellFileDescriptor(pub u16);

/// One syntax-derived shell redirection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellRedirection {
    /// Command that owns this redirection, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<ShellCommandId>,
    /// Redirection behavior.
    pub kind: ShellRedirectionKind,
    /// Explicit source descriptor, if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_fd: Option<ShellFileDescriptor>,
    /// Target word or descriptor spelling.
    pub target: ShellWord,
    /// Statically resolved path, when the target is a path and fully literal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_path: Option<String>,
    /// Exact source range for the complete redirection.
    pub span: ShellSourceSpan,
}

/// Reason analysis could not prove complete coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ShellIncompleteReason {
    /// Executable identity depends on runtime expansion.
    DynamicExecutable { span: ShellSourceSpan },
    /// A construct can execute dynamically supplied shell source.
    DynamicShellSource { span: ShellSourceSpan },
    /// Syntax is not supported by the selected execution dialect or adapter.
    UnsupportedConstruct {
        construct: String,
        span: Option<ShellSourceSpan>,
    },
    /// A configured resource bound was exceeded.
    LimitExceeded {
        limit: ShellAnalysisLimitKind,
        maximum: u32,
    },
}

/// Analysis resource bound that was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellAnalysisLimitKind {
    /// Input source bytes.
    SourceBytes,
    /// Visited parser/adapter nodes.
    Nodes,
    /// Shell syntax nesting depth.
    NestingDepth,
    /// Nested substitutions.
    Substitutions,
    /// Extracted commands.
    Commands,
    /// Extracted redirections.
    Redirections,
}

/// Whether analysis covered every execution-capable construct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ShellAnalysisCompleteness {
    /// Every execution-capable construct was traversed.
    Complete,
    /// Analysis produced useful facts but cannot authorize automatically.
    Incomplete { reasons: Vec<ShellIncompleteReason> },
}

impl ShellAnalysisCompleteness {
    /// Return whether complete traversal was proven.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }
}

/// Non-fatal analysis diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellAnalysisDiagnostic {
    /// Stable diagnostic code.
    pub code: String,
    /// Human-readable diagnostic message.
    pub message: String,
    /// Related source range, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<ShellSourceSpan>,
}

/// Successful shell analysis result, which may still be incomplete and therefore fail closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellAnalysis {
    /// Serialized contract version.
    pub schema_version: u32,
    /// Analyzed dialect.
    pub dialect: ShellDialect,
    /// Exact original source.
    pub source: String,
    /// Extracted executable leaves in source traversal order.
    pub commands: Vec<ShellCommand>,
    /// Extracted redirections in source traversal order.
    pub redirections: Vec<ShellRedirection>,
    /// Traversal completeness.
    pub completeness: ShellAnalysisCompleteness,
    /// Non-fatal diagnostics.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<ShellAnalysisDiagnostic>,
}

/// Analysis failure category independent of the parser implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ShellAnalysisErrorKind {
    /// Request contract version is unsupported.
    UnsupportedSchema,
    /// Requested dialect is unsupported on this implementation.
    UnsupportedDialect,
    /// Input exceeds source limits before parsing.
    SourceLimitExceeded,
    /// Shell source is syntactically invalid.
    Syntax,
    /// Parser failed without a safely usable partial result.
    Parser,
}

/// Fatal shell analysis error represented entirely by Bcode-owned fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellAnalysisError {
    /// Stable error category.
    pub kind: ShellAnalysisErrorKind,
    /// Human-readable error message.
    pub message: String,
    /// Requested dialect.
    pub dialect: ShellDialect,
    /// Related source range, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<ShellSourceSpan>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_result_round_trip_without_parser_types() {
        let request = ShellAnalysisRequest::posix("printf '%s\\n' ok");
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(
            serde_json::from_str::<ShellAnalysisRequest>(&encoded).unwrap(),
            request
        );

        let analysis = ShellAnalysis {
            schema_version: SHELL_ANALYSIS_SCHEMA_VERSION,
            dialect: ShellDialect::Posix,
            source: request.source.clone(),
            commands: vec![ShellCommand {
                id: ShellCommandId(0),
                parent_id: None,
                relation: ShellCommandRelation::Root,
                span: ShellSourceSpan::new(0, 19),
                source: request.source,
                executable: ShellWord::Static {
                    value: "printf".to_owned(),
                    span: ShellSourceSpan::new(0, 6),
                },
                arguments: Vec::new(),
                assignments: Vec::new(),
                context: ShellCommandContext::default(),
                match_candidates: vec![ShellCommandMatchCandidate {
                    subject: "printf '%s\\n' ok".to_owned(),
                    kind: ShellCommandMatchCandidateKind::Original,
                    transformation: None,
                }],
            }],
            redirections: Vec::new(),
            completeness: ShellAnalysisCompleteness::Complete,
            diagnostics: Vec::new(),
        };
        let encoded = serde_json::to_string(&analysis).unwrap();
        assert_eq!(
            serde_json::from_str::<ShellAnalysis>(&encoded).unwrap(),
            analysis
        );
    }

    #[test]
    fn default_limits_are_bounded() {
        let limits = ShellAnalysisLimits::default();
        assert!(limits.max_source_bytes > 0);
        assert!(limits.max_nodes > 0);
        assert!(limits.max_nesting_depth > 0);
        assert!(limits.max_substitutions > 0);
        assert!(limits.max_commands > 0);
        assert!(limits.max_redirections > 0);
    }
}
