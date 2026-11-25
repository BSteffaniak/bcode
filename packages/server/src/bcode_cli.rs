//! Exact Bcode CLI identity offered to commands started by tools.
//!
//! A command that runs `bcode` must reach the daemon artifact and state location serving
//! the session that started it. A search-path lookup can resolve a different build, which
//! then talks to a different daemon or state location. The daemon therefore publishes a
//! per-artifact directory whose only `bcode` entry links to its own pinned executable and
//! describes it, the state root and the session in a typed host context entry.

use std::path::{Path, PathBuf};

use bcode_session_models::SessionId;

/// Name of the CLI entry published in [`ServerBcodeCli::cli_directory`].
const CLI_ENTRY_NAME: &str = if cfg!(windows) { "bcode.exe" } else { "bcode" };

/// Verified Bcode executable serving this daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerBcodeCli {
    executable: PathBuf,
    cli_directory: Option<PathBuf>,
}

impl ServerBcodeCli {
    /// Describe `executable` and publish its CLI directory beneath `state_root`.
    ///
    /// Publication is best-effort: when the directory cannot be prepared, commands still
    /// receive the exact executable path, but no search-path entry is offered.
    #[must_use]
    pub fn publish(executable: &Path, digest: &str, state_root: &Path) -> Self {
        let cli_directory = publish_cli_directory(executable, digest, state_root)
            .map_err(|error| {
                tracing::warn!(
                    %error,
                    executable = %executable.display(),
                    "could not publish the Bcode CLI directory for shell commands"
                );
            })
            .ok();
        Self {
            executable: executable.to_path_buf(),
            cli_directory,
        }
    }

    /// Directory whose `bcode` entry resolves to the pinned executable, when published.
    #[cfg(test)]
    #[must_use]
    pub fn cli_directory(&self) -> Option<&Path> {
        self.cli_directory.as_deref()
    }

    /// Typed host context entry for one invocation of `session_id`.
    #[must_use]
    pub fn host_context_entry(
        &self,
        state_root: &Path,
        config_file: Option<&Path>,
        session_id: SessionId,
    ) -> bcode_tool::ToolHostContextEntry {
        let context = bcode_tool::ToolBcodeRuntimeContext {
            executable: self.executable.clone(),
            cli_directory: self.cli_directory.clone(),
            state_root: state_root.to_path_buf(),
            config_file: config_file.map(Path::to_path_buf),
            session_id: session_id.to_string(),
        };
        bcode_tool::ToolHostContextEntry {
            schema: bcode_tool::TOOL_BCODE_RUNTIME_CONTEXT_SCHEMA.to_owned(),
            schema_version: bcode_tool::TOOL_BCODE_RUNTIME_CONTEXT_SCHEMA_VERSION,
            payload: serde_json::to_value(context).unwrap_or(serde_json::Value::Null),
        }
    }
}

/// The configuration file a command can pass to reproduce `launch` exactly, if any.
///
/// Only a single explicit file without inline overrides reproduces a session's
/// configuration; anything else is left to the command to resolve explicitly.
#[must_use]
pub fn reproducible_config_file(launch: &bcode_config::ConfigLaunchContext) -> Option<&Path> {
    match launch.config_files.as_slice() {
        [file] if !launch.inline_overrides && file.is_absolute() => Some(file.as_path()),
        _ => None,
    }
}

fn cli_directory_path(state_root: &Path, digest: &str) -> PathBuf {
    let label: String = digest
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(16)
        .collect();
    state_root.join("runtime").join("cli").join(label)
}

#[cfg(unix)]
fn publish_cli_directory(
    executable: &Path,
    digest: &str,
    state_root: &Path,
) -> std::io::Result<PathBuf> {
    if !executable.is_absolute() || digest.is_empty() {
        return Err(std::io::Error::other(
            "the pinned executable path must be absolute and have a digest",
        ));
    }
    let directory = cli_directory_path(state_root, digest);
    std::fs::create_dir_all(&directory)?;
    let entry = directory.join(CLI_ENTRY_NAME);
    if std::fs::read_link(&entry).is_ok_and(|target| target == executable) {
        return Ok(directory);
    }
    // Publish atomically so concurrent daemons of the same artifact never observe a
    // missing or partially written entry.
    let staged = directory.join(format!(".{CLI_ENTRY_NAME}.{}", std::process::id()));
    let _ = std::fs::remove_file(&staged);
    std::os::unix::fs::symlink(executable, &staged)?;
    std::fs::rename(&staged, &entry).inspect_err(|_| {
        let _ = std::fs::remove_file(&staged);
    })?;
    Ok(directory)
}

#[cfg(not(unix))]
fn publish_cli_directory(
    _executable: &Path,
    _digest: &str,
    _state_root: &Path,
) -> std::io::Result<PathBuf> {
    // Symbolic links require elevated privileges on Windows; commands receive the exact
    // executable path instead.
    Err(std::io::Error::other(
        "CLI directory publication is unavailable on this platform",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn published_entry_resolves_to_the_pinned_executable() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("images").join("bcode-abc");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, b"#!/bin/sh\n").unwrap();

        let cli = ServerBcodeCli::publish(&executable, "abcdef0123", root.path());
        let directory = cli.cli_directory().expect("published directory");
        assert_eq!(
            std::fs::canonicalize(directory.join("bcode")).unwrap(),
            std::fs::canonicalize(&executable).unwrap()
        );

        // Republishing is idempotent and a different artifact gets its own directory.
        let again = ServerBcodeCli::publish(&executable, "abcdef0123", root.path());
        assert_eq!(again.cli_directory(), Some(directory));
        let other = ServerBcodeCli::publish(&executable, "999999", root.path());
        assert_ne!(other.cli_directory(), Some(directory));
    }

    #[test]
    fn relative_executable_is_not_published() {
        let root = tempfile::tempdir().unwrap();
        let cli = ServerBcodeCli::publish(Path::new("bcode"), "abc", root.path());
        assert!(cli.cli_directory().is_none());
    }

    #[test]
    fn host_context_round_trips_through_the_typed_contract() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("bcode-image");
        std::fs::write(&executable, b"").unwrap();
        let cli = ServerBcodeCli::publish(&executable, "abc123", root.path());
        let session_id = SessionId::new();
        let config = root.path().join("bcode.toml");
        let entry = cli.host_context_entry(root.path(), Some(&config), session_id);
        assert_eq!(entry.schema, bcode_tool::TOOL_BCODE_RUNTIME_CONTEXT_SCHEMA);
        let decoded: bcode_tool::ToolBcodeRuntimeContext =
            serde_json::from_value(entry.payload).unwrap();
        assert_eq!(decoded.executable, executable);
        assert_eq!(decoded.config_file, Some(config));
        assert_eq!(decoded.session_id, session_id.to_string());
    }

    #[test]
    fn only_a_single_override_free_file_reproduces_configuration() {
        let launch = |files: &[&str], inline_overrides| bcode_config::ConfigLaunchContext {
            config_files: files.iter().map(PathBuf::from).collect(),
            inline_overrides,
            provider_config_directory: PathBuf::from("/providers"),
            recovery_sources: None,
        };
        assert_eq!(
            reproducible_config_file(&launch(&["/a.toml"], false)),
            Some(Path::new("/a.toml"))
        );
        assert_eq!(reproducible_config_file(&launch(&["/a.toml"], true)), None);
        assert_eq!(
            reproducible_config_file(&launch(&["/a.toml", "/b.toml"], false)),
            None
        );
        assert_eq!(reproducible_config_file(&launch(&["a.toml"], false)), None);
    }
}
