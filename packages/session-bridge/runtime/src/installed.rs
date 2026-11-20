//! An installed Bcode's session bridge, run as a child process.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bcode_session_bridge::BridgeClientError;
use tokio::io::{AsyncBufReadExt as _, BufReader};

use crate::{AttachError, Attached, RuntimeOptions};

/// How long a bridge that closed while starting gets to finish writing its diagnostics.
const STARTUP_STDERR_WAIT: Duration = Duration::from_secs(2);
/// Most of a failed bridge's last diagnostic lines kept to explain it.
const STARTUP_STDERR_LINES: usize = 20;

/// Start `executable session-bridge --stdio` without the private environment and negotiate.
///
/// The bridge's diagnostics go to the log, not the application's terminal, and its last lines
/// explain a bridge that closes while starting.
pub async fn start(options: &RuntimeOptions, executable: &Path) -> Result<Attached, AttachError> {
    let mut command = tokio::process::Command::new(executable);
    command
        .args(["session-bridge", "--stdio"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for name in &options.private_env {
        command.env_remove(name);
    }
    command.envs(&options.env);
    if let Some(directory) = &options.directory {
        command.current_dir(directory);
    }
    let mut child = command.spawn().map_err(|error| {
        AttachError::Failed(format!(
            "Could not start Bcode ({}): {error}",
            executable.display()
        ))
    })?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(AttachError::Failed(
            "Bcode's session bridge has no pipes".to_owned(),
        ));
    };
    let recent = Arc::new(Mutex::new(VecDeque::new()));
    let reader = tokio::spawn({
        let recent = Arc::clone(&recent);
        async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(target: "bcode_session_bridge", "{line}");
                let mut recent = lock(&recent);
                if recent.len() == STARTUP_STDERR_LINES {
                    recent.pop_front();
                }
                recent.push_back(line);
            }
        }
    });
    match Attached::connect(stdout, stdin, &options.app, options.versions).await {
        Ok(attached) => {
            attached.client.adopt_process(child).await;
            Ok(attached)
        }
        Err(error) => {
            let _ = tokio::time::timeout(STARTUP_STDERR_WAIT, reader).await;
            let diagnostics: Vec<String> = lock(&recent).iter().cloned().collect();
            Err(explain_startup_failure(
                error,
                executable,
                &diagnostics,
                options,
            ))
        }
    }
}

/// A bridge client failure as an attach error naming `app`.
pub fn bridge_error(app: &str, error: BridgeClientError) -> AttachError {
    match error {
        BridgeClientError::Bridge(error) if error.code == "version_mismatch" => {
            AttachError::VersionMismatch(format!(
                "{app} cannot use this Bcode ({}). Update whichever side the message names.",
                error.message.replace("the application", app)
            ))
        }
        other => AttachError::Failed(format!("Could not start Bcode's session bridge: {other}")),
    }
}

/// Why a bridge `executable` started closed before it answered, from its last `diagnostics`:
/// a Bcode too old to have the bridge says so, and anything else carries its last words.
fn explain_startup_failure(
    error: AttachError,
    executable: &Path,
    diagnostics: &[String],
    options: &RuntimeOptions,
) -> AttachError {
    if !matches!(error, AttachError::Failed(_)) {
        return error;
    }
    if diagnostics
        .iter()
        .any(|line| line.contains("unrecognized subcommand"))
    {
        return AttachError::VersionMismatch(format!(
            "The Bcode at {} is too old for {}: it has no session bridge. Update it, or name a \
             newer Bcode with {}.",
            executable.display(),
            options.app,
            options.naming_hint
        ));
    }
    match diagnostics
        .iter()
        .rev()
        .find(|line| !line.trim().is_empty())
    {
        Some(last) => AttachError::Failed(format!(
            "{error} ({}: {})",
            executable.display(),
            last.trim()
        )),
        None => error,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bcode_without_the_bridge_is_reported_as_too_old() {
        let options = RuntimeOptions {
            naming_hint: "`--bcode`".into(),
            ..RuntimeOptions::new("bmux")
        };
        let error = explain_startup_failure(
            AttachError::Failed("closed".into()),
            Path::new("/old/bcode"),
            &["error: unrecognized subcommand 'session-bridge'".into()],
            &options,
        );
        assert!(
            matches!(&error, AttachError::VersionMismatch(message)
                if message.contains("too old for bmux") && message.contains("`--bcode`")),
            "{error}"
        );
    }

    #[test]
    fn other_startup_failures_carry_the_bridge_s_last_words() {
        let error = explain_startup_failure(
            AttachError::Failed("closed".into()),
            Path::new("/usr/bin/bcode"),
            &[
                "starting".into(),
                "config error: bad key".into(),
                "  ".into(),
            ],
            &RuntimeOptions::new("bmux"),
        );
        assert!(
            error
                .to_string()
                .ends_with("(/usr/bin/bcode: config error: bad key)")
        );
    }
}
