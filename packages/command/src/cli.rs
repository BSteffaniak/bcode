//! Bcode-owned CLI extension policy and results, independent of clap and ABI layout.

use std::{collections::BTreeMap, path::PathBuf};

use bcode_session_models::SessionId;
use serde::{Deserialize, Serialize};

pub mod descriptor;
pub use descriptor::{CliArgvRequest, CliClassification, CliDescriptor};

/// Inert descriptor query: empty bytes; response is JSON `Option<CliDescriptor>`.
pub const CLI_DESCRIBE_CONTRACT: &str = "bcode.cli.describe/v1";
/// Side-effect-free JSON `CliArgvRequest` -> `CliClassification`.
pub const CLI_CLASSIFY_CONTRACT: &str = "bcode.cli.classify/v1";
/// Local-authorized JSON `CliArgvRequest` -> `CliOutcome`, exactly once.
pub const CLI_INVOKE_CONTRACT: &str = "bcode.cli.invoke/v1";

/// Versioned application extension contract; never exposed as a model tool.
pub const CLI_EXTENSION_CONTRACT: &str = "bcode.cli/v1";

/// Inert per-command policy supplied alongside the shared command declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliCommandPolicy {
    /// Shared declaration's stable command name, not its displayed alias.
    pub command_name: String,
    /// Whether the host must establish its selected daemon before invocation.
    pub requires_daemon: bool,
    /// Whether this command may receive stdin. The default is no input.
    #[serde(default)]
    pub accepts_stdin: bool,
    /// Whether this command may return terminal-local host actions.
    #[serde(default)]
    pub allows_host_action: bool,
}

/// Parsed caller-local request. Invocation authority is provided separately by
/// the host's activation/invocation scope, never by fields in this payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliInvocation {
    /// Stable name from the selected shared declaration.
    pub command_name: String,
    /// Canonical path, even when the caller used an alias.
    pub command_path: Vec<String>,
    /// Original argument tokens, preserving ordering and the `--` boundary.
    pub arguments: Vec<String>,
    /// Explicitly supplied input; the CLI never reads stdin speculatively.
    pub stdin: Option<Vec<u8>>,
}

/// Local application actions are not generic BMUX domain variants, executable
/// callbacks, tool calls, or a request to replace terminal ownership remotely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CliHostAction {
    /// Open a plugin surface through the local Bcode frontend adapter.
    OpenTuiSurface {
        /// Plugin-owned surface kind.
        surface_kind: String,
        /// Optional repository context, resolved by the local host.
        repo_path: Option<PathBuf>,
        /// Plugin-owned options.
        options: BTreeMap<String, String>,
    },
    /// Attach locally through the normal session application boundary.
    AttachSession {
        /// Session chosen by the command.
        session_id: SessionId,
    },
}

/// Explicit, bounded output transported by both packaging paths. Command
/// failures are values, not SDK thread-local state or plugin process stderr.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliOutcome {
    /// Process-compatible status; zero indicates success.
    pub exit_code: u8,
    /// Bytes written only by the CLI adapter, never by attach adapters.
    #[serde(default)]
    pub stdout: Vec<u8>,
    /// Diagnostic bytes written only by the CLI adapter.
    #[serde(default)]
    pub stderr: Vec<u8>,
    /// Optional local action; forbidden alongside output or a failed status.
    pub host_action: Option<CliHostAction>,
}
