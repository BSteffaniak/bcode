//! Shell-owned semantic presentation snapshot. No terminal recording or host capability.

/// Versioned producer output snapshot consumed by both BMUX presentation hosts.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellSemanticOutput {
    /// Only version one is accepted by the producer adapter.
    pub version: u32,
    /// Exact command source, never rewritten execution text.
    pub command: String,
    /// Effective invocation arguments and recorded presentation policy.
    pub arguments: serde_json::Value,
    /// Producer-normalized plain text. Consumers must not parse terminal bytes.
    pub output: String,
    /// Original normalized byte count before bounded-tail retention.
    pub output_bytes: u64,
    /// Whether this snapshot omits earlier normalized bytes.
    pub output_truncated: bool,
    /// Whether execution settled; false snapshots remain replaceable progress.
    pub complete: bool,
}
