//! TUI-facing daemon/client error classification and user messaging.

use bcode_client::ClientError;
use bcode_ipc::CodecError;

use super::TuiError;
use super::app::BmuxApp;

/// Recoverable daemon/client issue surfaced by the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiDaemonIssue {
    /// The daemon transport is unavailable or could not be started.
    Unavailable,
    /// The daemon accepted work but did not respond in time.
    Timeout,
    /// The daemon speaks an incompatible protocol or produced undecodable IPC.
    StaleOrIncompatible,
    /// The requested session is unavailable.
    SessionUnavailable,
    /// The requested session needs explicit repair before normal use.
    SessionRepairRequired,
    /// The requested projection/index is stale.
    ProjectionStale,
    /// The daemon returned a server-side rejection.
    ServerRejected { code: String, message: String },
    /// The daemon returned a response shape the client did not expect.
    UnexpectedDaemonResponse,
    /// Another recoverable client/daemon issue.
    Other(String),
}

/// User-facing daemon issue message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiDaemonIssueMessage {
    /// Short status-line message.
    pub status: String,
    /// Longer optional transcript/system-note detail.
    pub detail: Option<String>,
}

impl TuiDaemonIssue {
    /// Return a user-facing message for this issue.
    #[must_use]
    pub fn message(&self, label: &str) -> TuiDaemonIssueMessage {
        match self {
            Self::Unavailable => TuiDaemonIssueMessage {
                status: format!("{label}: daemon unavailable; UI remains active"),
                detail: Some(
                    "Bcode could not connect to the local daemon. You can keep editing drafts. Try restarting the daemon or running `bcode doctor`.".to_owned(),
                ),
            },
            Self::Timeout => TuiDaemonIssueMessage {
                status: format!("{label}: daemon did not respond; UI remains active"),
                detail: Some(
                    "The local daemon accepted the request but did not respond before the timeout. It may be busy or wedged. Try again, restart the daemon, or inspect daemon logs.".to_owned(),
                ),
            },
            Self::StaleOrIncompatible => TuiDaemonIssueMessage {
                status: format!("{label}: stale/incompatible daemon; restart recommended"),
                detail: Some(
                    "The daemon response could not be decoded by this client. This usually means an older daemon is still running after an update. Restart the daemon and retry.".to_owned(),
                ),
            },
            Self::SessionUnavailable => TuiDaemonIssueMessage {
                status: format!("{label}: session unavailable"),
                detail: Some(
                    "The requested session could not be opened. It may have been deleted, moved, or the daemon catalog may be stale. Retry after refreshing or run `bcode doctor`.".to_owned(),
                ),
            },
            Self::SessionRepairRequired => TuiDaemonIssueMessage {
                status: format!("{label}: session repair required"),
                detail: Some(
                    "This session appears inconsistent and normal TUI access will not repair it automatically. Run the explicit repair/doctor command before reopening it.".to_owned(),
                ),
            },
            Self::ProjectionStale => TuiDaemonIssueMessage {
                status: format!("{label}: session view is stale; retry"),
                detail: Some(
                    "The daemon reported a stale session projection. Retry the action; if it persists, run the repair/doctor command.".to_owned(),
                ),
            },
            Self::ServerRejected { code, message } => TuiDaemonIssueMessage {
                status: format!("{label}: daemon rejected request ({code})"),
                detail: Some(format!("Daemon rejected the request with `{code}`: {message}")),
            },
            Self::UnexpectedDaemonResponse => TuiDaemonIssueMessage {
                status: format!("{label}: unexpected daemon response"),
                detail: Some(
                    "The daemon returned a response shape this client did not expect. Restart the daemon if bcode was recently updated.".to_owned(),
                ),
            },
            Self::Other(message) => TuiDaemonIssueMessage {
                status: format!("{label}: {message}"),
                detail: Some(message.clone()),
            },
        }
    }
}

/// Classify a client error as a TUI-recoverable daemon issue.
#[must_use]
pub fn classify_client_error(error: &ClientError) -> TuiDaemonIssue {
    if matches!(error, ClientError::RequestTimeout { .. }) {
        return TuiDaemonIssue::Timeout;
    }
    if error.is_daemon_unavailable() {
        return TuiDaemonIssue::Unavailable;
    }
    match error {
        ClientError::RequestTimeout { .. } => TuiDaemonIssue::Timeout,
        ClientError::Codec(
            CodecError::Deserialize(_)
            | CodecError::EventConversion(_)
            | CodecError::UnsupportedVersion { .. },
        ) => TuiDaemonIssue::StaleOrIncompatible,
        ClientError::Server { code, message } => classify_server_error(code, message),
        ClientError::UnexpectedResponse | ClientError::UnexpectedEnvelope => {
            TuiDaemonIssue::UnexpectedDaemonResponse
        }
        ClientError::Transport(_) | ClientError::Codec(_) | ClientError::DaemonStart(_) => {
            TuiDaemonIssue::Other(error.to_string())
        }
    }
}

fn classify_server_error(code: &str, message: &str) -> TuiDaemonIssue {
    match code {
        "session_repair_required" => TuiDaemonIssue::SessionRepairRequired,
        "projection_stale" => TuiDaemonIssue::ProjectionStale,
        "session_not_found" | "session_unavailable" => TuiDaemonIssue::SessionUnavailable,
        _ => TuiDaemonIssue::ServerRejected {
            code: code.to_owned(),
            message: message.to_owned(),
        },
    }
}

/// Return whether a TUI error should degrade the UI instead of exiting it.
#[must_use]
pub const fn is_nonfatal_tui_error(error: &TuiError) -> bool {
    matches!(
        error,
        TuiError::Client(_) | TuiError::PluginService { .. } | TuiError::SessionUnavailable { .. }
    )
}

/// Report a recoverable client error to the app.
pub fn report_client_issue(app: &mut BmuxApp, label: &str, error: &ClientError) {
    let issue = classify_client_error(error);
    report_issue(app, &issue, label);
}

/// Report a recoverable TUI error to the app.
pub fn report_tui_issue(app: &mut BmuxApp, label: &str, error: &TuiError) {
    if let TuiError::Client(error) = error {
        report_client_issue(app, label, error);
        return;
    }
    let message = match error {
        TuiError::PluginService { code, message } => Some(
            TuiDaemonIssue::ServerRejected {
                code: code.clone(),
                message: message.clone(),
            }
            .message(label),
        ),
        TuiError::SessionUnavailable { reason, .. } => {
            Some(TuiDaemonIssue::SessionUnavailable.message(&format!("{label}: {reason}")))
        }
        _ => None,
    };
    if let Some(message) = message {
        app.set_status(message.status.clone());
        if let Some(detail) = message.detail {
            app.push_system_note(format!("{}\n\n{detail}", message.status));
        }
        return;
    }
    let message = format!("{label}: {error}");
    app.set_status(message.clone());
    app.push_system_note(message);
}

fn report_issue(app: &mut BmuxApp, issue: &TuiDaemonIssue, label: &str) {
    let message = issue.message(label);
    app.set_status(message.status.clone());
    if let Some(detail) = message.detail {
        app.push_system_note(format!("{}\n\n{detail}", message.status));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{TuiDaemonIssue, classify_client_error};

    #[test]
    fn request_timeout_is_recoverable_timeout() {
        let error = bcode_client::ClientError::RequestTimeout {
            timeout: Duration::from_secs(15),
        };

        assert_eq!(classify_client_error(&error), TuiDaemonIssue::Timeout);
    }

    #[test]
    fn repair_required_server_error_is_classified() {
        let error = bcode_client::ClientError::Server {
            code: "session_repair_required".to_owned(),
            message: "repair me".to_owned(),
        };

        assert_eq!(
            classify_client_error(&error),
            TuiDaemonIssue::SessionRepairRequired
        );
    }
}
