//! Opt-in, versioned CLI failure diagnostics without raw error text.
//!
//! Applies to returned CLI errors. Argument-parser early exits are outside this
//! contract. Classification never grants retry authority or implies no side effects.
use crate::CliError;

/// Command-line selection of the returned-error presentation contract.
#[derive(Debug, clap::Args)]
pub(crate) struct CliErrorOutputOptions {
    /// Emit secret-safe versioned JSON for returned errors (not argument-parser exits).
    #[arg(long, global = true)]
    pub(crate) json_errors: bool,
}

/// Secret-safe diagnostic envelope, independently versioned from IPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct CliDiagnosticError {
    /// Exact supported diagnostic schema version.
    pub schema_version: u32,
    /// Normalized code; consumers must reject unknown versions/codes for retry.
    pub code: &'static str,
    /// Operation phase, not a claim about the underlying blocking cause.
    pub phase: &'static str,
    /// Fixed safe description; never includes provider, configuration or error payloads.
    pub message: &'static str,
}

impl CliDiagnosticError {
    /// Normalize a returned error without formatting its underlying payload.
    #[must_use]
    pub const fn from_error(error: &CliError) -> Self {
        match error {
            CliError::Client(bcode_client::ClientError::ConnectTimeout { .. }) => Self {
                schema_version: 1,
                code: "connection_timeout",
                phase: "connect",
                message: "Verified daemon connection timed out.",
            },
            _ => Self {
                schema_version: 1,
                code: "cli_failure",
                phase: "operation",
                message: "CLI operation failed.",
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_connection_timeout_has_exact_normalized_contract() {
        let error = CliError::Client(bcode_client::ClientError::ConnectTimeout {
            timeout: std::time::Duration::from_secs(5),
        });
        assert_eq!(
            serde_json::to_value(CliDiagnosticError::from_error(&error)).unwrap(),
            serde_json::json!({"schema_version": 1, "code": "connection_timeout",
                "phase": "connect", "message": "Verified daemon connection timed out."})
        );
    }

    #[test]
    fn presentation_text_cannot_impersonate_a_connection_timeout_or_leak() {
        let error = CliError::WorkflowApplication {
            code: "connection_timeout".into(),
            message: "SECRET_FIXTURE_MARKER".into(),
        };
        let diagnostic = CliDiagnosticError::from_error(&error);
        assert_eq!(diagnostic.code, "cli_failure");
        assert!(
            !serde_json::to_string(&diagnostic)
                .unwrap()
                .contains("SECRET_FIXTURE_MARKER")
        );
    }

    #[test]
    fn request_timeout_is_not_a_connection_timeout() {
        let error = CliError::Client(bcode_client::ClientError::RequestTimeout {
            timeout: std::time::Duration::from_secs(5),
        });
        assert_eq!(CliDiagnosticError::from_error(&error).code, "cli_failure");
    }

    #[test]
    fn io_failure_is_secret_safe_and_not_a_connection_timeout() {
        let error = CliError::Signal(std::io::Error::other("SECRET_FIXTURE_MARKER"));
        let diagnostic = CliDiagnosticError::from_error(&error);
        assert_eq!(diagnostic.phase, "operation");
        assert_eq!(diagnostic.code, "cli_failure");
        assert!(
            !serde_json::to_string(&diagnostic)
                .unwrap()
                .contains("SECRET_FIXTURE_MARKER")
        );
    }
}
