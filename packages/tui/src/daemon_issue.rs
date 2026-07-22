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
    /// The daemon speaks an incompatible protocol version.
    StaleOrIncompatible,
    /// The daemon response could not be decoded or converted.
    InvalidDaemonResponse(String),
    /// Another daemon owns the session, so migration or access must wait.
    SessionActiveElsewhere { message: String },
    /// The session storage writer generation is unsupported by this build.
    SessionWriterIncompatible { message: String },
    /// The requested session is unavailable.
    SessionUnavailable { message: String },
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
                status: format!("{label}: incompatible daemon protocol; restart recommended"),
                detail: Some(
                    "The daemon uses a protocol version this client does not support. Restart the daemon and retry."
                        .to_owned(),
                ),
            },
            Self::InvalidDaemonResponse(error) => TuiDaemonIssueMessage {
                status: format!("{label}: daemon response decode failed"),
                detail: Some(format!(
                    "The daemon response could not be decoded or converted. This does not necessarily mean the daemon is stale.\n\nDecode error: {error}"
                )),
            },
            Self::SessionActiveElsewhere { message } => TuiDaemonIssueMessage {
                status: format!("{label}: session is active in another daemon"),
                detail: Some(format!(
                    "Another daemon currently owns this session. Close clients using that daemon or stop it, then retry. Bcode will migrate known legacy storage automatically once exclusive ownership is available.\n\nDaemon error: {message}"
                )),
            },
            Self::SessionWriterIncompatible { message } => TuiDaemonIssueMessage {
                status: format!("{label}: incompatible session storage writer"),
                detail: Some(format!(
                    "This session was written by an unsupported storage generation. Use a compatible Bcode build or run `bcode session diagnose <session-id>`. Automatic migration was not attempted.\n\nDaemon error: {message}"
                )),
            },
            Self::SessionUnavailable { message } => TuiDaemonIssueMessage {
                status: format!("{label}: session unavailable"),
                detail: Some(format!(
                    "The daemon rejected this session request.\n\nDaemon error: {message}\n\nRefresh the session catalog and retry. If the error reports writer or projection incompatibility, run `bcode session diagnose <session-id>` for exact recovery guidance."
                )),
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
        ClientError::Codec(CodecError::UnsupportedVersion { .. })
        | ClientError::IncompatibleDaemon { .. } => TuiDaemonIssue::StaleOrIncompatible,
        ClientError::Codec(
            error @ (CodecError::Deserialize(_) | CodecError::EventConversion(_)),
        ) => TuiDaemonIssue::InvalidDaemonResponse(error.to_string()),
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
        "session_active_elsewhere" | "session_incompatible_active_client" => {
            TuiDaemonIssue::SessionActiveElsewhere {
                message: message.to_owned(),
            }
        }
        "session_writer_incompatible" => TuiDaemonIssue::SessionWriterIncompatible {
            message: message.to_owned(),
        },
        "session_not_found" | "session_unavailable" => TuiDaemonIssue::SessionUnavailable {
            message: message.to_owned(),
        },
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

/// Return the status text for a recoverable client error, including the underlying error.
#[must_use]
pub fn client_issue_status(label: &str, error: &ClientError) -> String {
    let status = classify_client_error(error).message(label).status;
    format!("{status}: {}", error_diagnostic(error).replace('\n', "; "))
}

/// Format an error and its complete source chain for diagnostics.
#[must_use]
pub fn error_diagnostic(error: &(dyn std::error::Error + 'static)) -> String {
    let mut diagnostic = format!("Underlying error: {error}");
    let mut source = error.source();
    while let Some(cause) = source {
        diagnostic.push_str("\nCaused by: ");
        diagnostic.push_str(&cause.to_string());
        source = cause.source();
    }
    diagnostic
}

/// Report a recoverable client error to the app.
pub fn report_client_issue(app: &mut BmuxApp, label: &str, error: &ClientError) {
    let issue = classify_client_error(error);
    report_issue(app, &issue, label, error);
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
        TuiError::SessionUnavailable { reason, .. } => Some(
            TuiDaemonIssue::SessionUnavailable {
                message: reason.clone(),
            }
            .message(&format!("{label}: {reason}")),
        ),
        _ => None,
    };
    if let Some(message) = message {
        app.set_status(format!(
            "{}: {}",
            message.status,
            error_diagnostic(error).replace('\n', "; ")
        ));
        let diagnostic = error_diagnostic(error);
        let note = message.detail.map_or_else(
            || format!("{}\n\n{diagnostic}", message.status),
            |detail| format!("{}\n\n{detail}\n\n{diagnostic}", message.status),
        );
        app.push_system_note(note);
        return;
    }
    let diagnostic = error_diagnostic(error);
    app.set_status(format!("{label}: {}", diagnostic.replace('\n', "; ")));
    app.push_system_note(format!("{label}\n\n{diagnostic}"));
}

fn report_issue(
    app: &mut BmuxApp,
    issue: &TuiDaemonIssue,
    label: &str,
    error: &(dyn std::error::Error + 'static),
) {
    let message = issue.message(label);
    app.set_status(format!(
        "{}: {}",
        message.status,
        error_diagnostic(error).replace('\n', "; ")
    ));
    let diagnostic = error_diagnostic(error);
    let note = message.detail.map_or_else(
        || format!("{}\n\n{diagnostic}", message.status),
        |detail| format!("{}\n\n{detail}\n\n{diagnostic}", message.status),
    );
    app.push_system_note(note);
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::Duration;

    use bcode_ipc::CodecError;

    use super::{TuiDaemonIssue, classify_client_error, client_issue_status, error_diagnostic};

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

    #[test]
    fn active_session_owner_has_specific_retry_guidance() {
        let error = bcode_client::ClientError::Server {
            code: "session_active_elsewhere".to_owned(),
            message: "owned by daemon 42".to_owned(),
        };
        let issue = classify_client_error(&error);
        assert_eq!(
            issue,
            TuiDaemonIssue::SessionActiveElsewhere {
                message: "owned by daemon 42".to_owned(),
            }
        );
        let detail = issue.message("send failed").detail.expect("detail");
        assert!(detail.contains("migrate known legacy storage automatically"));
        assert!(!detail.contains("Refresh the session catalog"));
    }

    #[test]
    fn writer_incompatibility_is_not_projection_staleness() {
        let error = bcode_client::ClientError::Server {
            code: "session_writer_incompatible".to_owned(),
            message: "future writer".to_owned(),
        };
        let issue = classify_client_error(&error);
        assert_eq!(
            issue,
            TuiDaemonIssue::SessionWriterIncompatible {
                message: "future writer".to_owned(),
            }
        );
        let detail = issue.message("attach failed").detail.expect("detail");
        assert!(detail.contains("Automatic migration was not attempted"));
        assert!(!detail.contains("Refresh the session catalog"));
    }

    #[test]
    fn only_unsupported_protocol_is_classified_as_incompatible() {
        let unsupported = bcode_client::ClientError::Codec(CodecError::UnsupportedVersion {
            actual: 8,
            expected: 9,
        });
        let conversion = bcode_client::ClientError::Codec(CodecError::EventConversion(
            "invalid model metadata".to_owned(),
        ));

        assert_eq!(
            classify_client_error(&unsupported),
            TuiDaemonIssue::StaleOrIncompatible
        );
        assert_eq!(
            classify_client_error(&conversion),
            TuiDaemonIssue::InvalidDaemonResponse(
                "event conversion failed: invalid model metadata".to_owned()
            )
        );
    }

    #[test]
    fn diagnostic_preserves_complete_error_source_chain() {
        let error =
            bcode_client::ClientError::Codec(CodecError::Io(io::Error::other("socket exploded")));

        assert_eq!(
            error_diagnostic(&error),
            "Underlying error: IPC codec error: I/O error: socket exploded\nCaused by: I/O error: socket exploded\nCaused by: socket exploded"
        );
    }

    #[test]
    fn status_only_diagnostics_retain_the_underlying_error() {
        let error = bcode_client::ClientError::Codec(CodecError::EventConversion(
            "invalid model metadata".to_owned(),
        ));
        let status = client_issue_status("model list unavailable", &error);

        assert!(status.contains("daemon response decode failed"));
        assert!(status.contains("event conversion failed: invalid model metadata"));
    }
}
