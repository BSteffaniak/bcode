//! Opt-in, versioned CLI failure diagnostics without raw error text.
//!
//! Applies to returned CLI errors. Argument-parser early exits are outside this
//! contract. Classification never grants retry authority or implies no side effects.
use crate::CliError;

/// Command-line selection of the returned-error presentation contract.
#[derive(Debug, clap::Args)]
pub struct CliErrorOutputOptions {
    /// Emit secret-safe versioned JSON for returned errors (not argument-parser exits).
    #[arg(long, global = true)]
    pub(crate) json_errors: bool,
}

/// Secret-safe diagnostic envelope, independently versioned from IPC.
///
/// Schema v1 permits additive fixed codes without changing the envelope. Unknown
/// codes remain unclassified and confer no retry authority. Inspection categories
/// describe typed client failures, not verified collection or execution causes.
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
    pub fn from_error(error: &CliError) -> Self {
        match error {
            CliError::WorkflowInspection { source } => Self::from_inspection_error(source),
            CliError::Client(error)
            | CliError::ChildSessionCreate { source: error, .. }
            | CliError::SessionListingInterrupted { source: error } => Self::from_client(error),
            CliError::DaemonStart(
                bcode_daemon_lifecycle::DaemonStartError::NotificationTimeout { .. },
            ) => Self::notification_timeout(),
            CliError::WorkflowApplication { code, .. } => Self::application(code),
            CliError::SessionSpawnNotAdmitted => Self::diagnostic(
                "session_spawn_not_admitted",
                "session_spawn",
                "Session spawn returned a failure or non-executable admission; inspect the complete JSON result before retrying.",
            ),
            CliError::PluginCommandNotApplied => Self::diagnostic(
                "plugin_command_not_applied",
                "plugin_command",
                "Plugin command or effect was unsuccessful; inspect the ordered effect outcomes and retained receipts before retrying.",
            ),
            CliError::InteractionResolutionNotApplied => Self::diagnostic(
                "interaction_resolution_not_applied",
                "interaction_resolution",
                "Permission or exchange resolution was not applied; inspect the exact receipt before retrying.",
            ),
            CliError::InteractionControllerNotApplied => Self::diagnostic(
                "interaction_controller_not_applied",
                "interaction_controller",
                "Semantic controller rejected input or returned cancellation; inspect the typed result and refresh before further input.",
            ),
            CliError::Signal(error) | CliError::ThemeIo(error) => Self::io(error.kind()),
            CliError::Json(_) => Self::diagnostic(
                "json_decode_failed",
                "decode",
                "JSON could not be decoded; check response compatibility and retained output.",
            ),
            CliError::Config(_) => Self::diagnostic(
                "configuration_failed",
                "configuration",
                "Configuration could not be resolved; validate the selected configuration without exposing credentials.",
            ),
            CliError::Session(_) | CliError::SessionDb(_) | CliError::SessionStore(_) => {
                Self::diagnostic(
                    "session_storage_failed",
                    "session_storage",
                    "Session storage operation failed; inspect session health through supported maintenance diagnostics before retrying.",
                )
            }
            CliError::WorkflowStore(_) => Self::diagnostic(
                "workflow_storage_failed",
                "workflow_storage",
                "Workflow storage operation failed; inspect compatibility, maintenance status and execution ownership.",
            ),
            CliError::InvalidArguments(_)
            | CliError::InvalidSessionHistoryRange
            | CliError::NewSessionWithCommand => Self::diagnostic(
                "invalid_arguments",
                "validation",
                "Invalid command arguments; consult command help.",
            ),
            _ => Self::diagnostic(
                "unclassified_cli_error",
                "operation",
                "This CLI error category has no normalized diagnostic yet; retain the operation and artifact identity. Its cause and side effects are unknown.",
            ),
        }
    }

    const fn notification_timeout() -> Self {
        Self::diagnostic(
            "daemon_readiness_notification_timeout",
            "startup",
            "Daemon readiness notification deadline elapsed; child readiness and execution lifetime are unknown. Inspect exact-artifact startup evidence before further action.",
        )
    }

    const fn diagnostic(code: &'static str, phase: &'static str, message: &'static str) -> Self {
        Self {
            schema_version: 1,
            code,
            phase,
            message,
        }
    }

    fn from_client(error: &bcode_client::ClientError) -> Self {
        use bcode_client::ClientError;
        match error {
            ClientError::ConnectTimeout { .. } => Self::diagnostic(
                "connection_timeout",
                "connect",
                "Verified daemon connection timed out.",
            ),
            ClientError::RequestTimeout { .. } => Self::diagnostic(
                "request_timeout",
                "request",
                "Daemon request deadline elapsed; the operation may have committed. Inspect its outcome before retrying mutations.",
            ),
            ClientError::DaemonStartupTimeout { .. } => Self::diagnostic(
                "daemon_startup_timeout",
                "startup",
                "Daemon startup deadline elapsed; inspect matching-artifact startup coordination and startup reports.",
            ),
            ClientError::Transport(_) => Self::diagnostic(
                "ipc_transport_failed",
                "transport",
                "IPC transport failed; inspect the matching daemon endpoint and ownership. Delivery and operation outcome may be unknown.",
            ),
            ClientError::Codec(_) => Self::diagnostic(
                "ipc_decode_failed",
                "decode",
                "IPC response decoding failed; verify exact client/daemon artifact compatibility.",
            ),
            ClientError::DaemonStart(
                bcode_daemon_lifecycle::DaemonStartError::NotificationTimeout { .. },
            ) => Self::notification_timeout(),
            ClientError::DaemonStart(_) => Self::diagnostic(
                "daemon_start_failed",
                "startup",
                "Matching daemon startup failed; inspect startup reports, storage availability and startup coordination.",
            ),
            ClientError::Server { code, .. } => Self::application(code),
            ClientError::IncompatibleDaemon { .. } => Self::diagnostic(
                "daemon_incompatible",
                "handshake",
                "Daemon compatibility verification failed; use the exact target artifact and state location.",
            ),
            ClientError::Protocol(_) => Self::diagnostic(
                "ipc_protocol_failed",
                "protocol",
                "IPC protocol validation failed; inspect client/daemon compatibility and retained operation evidence.",
            ),
            ClientError::WorktreeCreate { .. } => Self::diagnostic(
                "worktree_create_failed",
                "worktree",
                "Worktree creation failed; creation may have partially committed. Inspect registered worktrees before retrying.",
            ),
            ClientError::UnexpectedResponse => Self::diagnostic(
                "unexpected_response",
                "decode",
                "Daemon returned an unexpected response payload; inspect protocol compatibility.",
            ),
            ClientError::UnexpectedEnvelope => Self::diagnostic(
                "unexpected_envelope",
                "decode",
                "Daemon returned an unexpected IPC envelope; inspect protocol compatibility.",
            ),
        }
    }

    fn application(code: &str) -> Self {
        match code {
            "workflow_activity_snapshot_start_failed" => Self::diagnostic(
                "workflow_activity_snapshot_start_failed",
                "snapshot_start",
                "Workflow activity could not establish a snapshot; inspect storage compatibility, capacity and ownership.",
            ),
            "workflow_activity_inventory_failed" => Self::diagnostic(
                "workflow_activity_inventory_failed",
                "inventory_read",
                "Workflow activity inventory failed; inspect scan lifetime, cursor revision, ledger consistency and database availability.",
            ),
            "workflow_activity_live_observation_failed" => Self::diagnostic(
                "workflow_activity_live_observation_failed",
                "live_adaptation",
                "Workflow live observation failed; inspect session provenance, ownership, resident connection and metadata readiness.",
            ),
            "workflow_activity_unavailable" => Self::diagnostic(
                "workflow_activity_unavailable",
                "workflow_activity",
                "Workflow activity snapshot is unavailable, expired or unsupported; inspect scan identity, cursor revision, snapshot lifetime and storage health. This is not an idle or terminal observation.",
            ),
            "workflow_unavailable" => Self::diagnostic(
                "workflow_unavailable",
                "workflow",
                "Workflow state is unavailable; inspect domain health, storage compatibility and execution ownership. This does not establish a terminal outcome.",
            ),
            "session_database_locked" => Self::diagnostic(
                "session_database_locked",
                "session_create",
                "Session database is locked; inspect verified ownership and resident connection lifecycle. Do not delete the WAL or substitute storage.",
            ),
            "session_parent_not_found" => Self::diagnostic(
                "session_parent_not_found",
                "session_create",
                "Canonical parent session was not found in the selected state location; reconcile its identity without substituting storage.",
            ),
            "session_storage_migration_required" => Self::diagnostic(
                "session_storage_migration_required",
                "session_create",
                "Session storage requires explicit migration before child creation; preserve the canonical data.",
            ),
            "session_create_failed" => Self::diagnostic(
                "session_create_failed",
                "session_create",
                "Session creation failed; verify parent session health and storage availability. Retain the child identity for exact reconciliation.",
            ),
            "session_children_unavailable" => Self::diagnostic(
                "session_children_unavailable",
                "session_discovery",
                "Child discovery is unavailable; inspect query coverage and parent session health. Missing results do not prove absence of children.",
            ),
            "session_not_found" => Self::diagnostic(
                "session_not_found",
                "session_lookup",
                "Session was not found in the selected state location; verify its canonical location and identity without substituting storage.",
            ),
            _ => Self::diagnostic(
                "unclassified_application_error",
                "application",
                "The application returned a code not recognized by this diagnostic contract; its cause and side effects remain unknown. Do not infer retry safety.",
            ),
        }
    }

    const fn io(kind: std::io::ErrorKind) -> Self {
        match kind {
            std::io::ErrorKind::PermissionDenied => Self::diagnostic(
                "io_permission_denied",
                "io",
                "Filesystem access was denied; verify permissions for the selected operation.",
            ),
            std::io::ErrorKind::NotFound => Self::diagnostic(
                "io_not_found",
                "io",
                "A required filesystem resource was not found; verify the selected operation's paths.",
            ),
            std::io::ErrorKind::StorageFull => Self::diagnostic(
                "io_storage_full",
                "io",
                "Storage is full; free space in the selected storage location before retrying. Partial effects require reconciliation.",
            ),
            _ => Self::diagnostic(
                "io_failed",
                "io",
                "An I/O operation failed; inspect storage availability and operation evidence. Raw I/O payloads are withheld because they may contain secrets.",
            ),
        }
    }

    // Keep subtype diagnostics finite: never format codec errors or their payloads.
    fn inspection_codec_message(error: &bcode_ipc::CodecError) -> &'static str {
        use bcode_ipc::CodecError;
        match error {
            CodecError::PayloadTooLarge { .. } => "Workflow inspection codec: payload_too_large.",
            CodecError::InvalidChunk(_) => "Workflow inspection codec: invalid_chunk.",
            CodecError::Serialize(_) => "Workflow inspection codec: serialize.",
            CodecError::Deserialize(_) => "Workflow inspection codec: deserialize.",
            CodecError::EventConversion(_) => "Workflow inspection codec: event_conversion.",
            CodecError::UnsupportedVersion { .. } => {
                "Workflow inspection codec: unsupported_version."
            }
            CodecError::Io(error) => match error.kind() {
                std::io::ErrorKind::NotFound => "Workflow inspection codec: io_not_found.",
                std::io::ErrorKind::PermissionDenied => {
                    "Workflow inspection codec: io_permission_denied."
                }
                std::io::ErrorKind::ConnectionReset => {
                    "Workflow inspection codec: io_connection_reset."
                }
                std::io::ErrorKind::BrokenPipe => "Workflow inspection codec: io_broken_pipe.",
                std::io::ErrorKind::TimedOut => "Workflow inspection codec: io_timed_out.",
                std::io::ErrorKind::UnexpectedEof => {
                    "Workflow inspection codec: io_unexpected_eof."
                }
                std::io::ErrorKind::InvalidData => "Workflow inspection codec: io_invalid_data.",
                std::io::ErrorKind::Interrupted => "Workflow inspection codec: io_interrupted.",
                _ => "Workflow inspection codec: io_other.",
            },
        }
    }

    // The reader exposes ClientError, not typed collection/revision context. Do not
    // infer such context from server codes, messages, or the current run state.
    fn from_inspection_error(error: &bcode_client::ClientError) -> Self {
        use bcode_client::ClientError;
        let (code, phase, message) = match error {
            ClientError::ConnectTimeout { .. } => (
                "connection_timeout",
                "connect",
                "Verified daemon connection timed out.",
            ),
            ClientError::RequestTimeout { .. } => (
                "workflow_inspection_request_timeout",
                "workflow_inspection",
                "Workflow inspection request timed out.",
            ),
            ClientError::Transport(_) => (
                "workflow_inspection_transport_failure",
                "workflow_inspection",
                "Workflow inspection transport failed.",
            ),
            ClientError::Codec(error) => (
                "workflow_inspection_decode_failure",
                "workflow_inspection",
                Self::inspection_codec_message(error),
            ),
            ClientError::Protocol(_)
            | ClientError::UnexpectedResponse
            | ClientError::UnexpectedEnvelope => (
                "workflow_inspection_protocol_failure",
                "workflow_inspection",
                "Workflow inspection protocol failed.",
            ),
            ClientError::IncompatibleDaemon { .. } => (
                "workflow_inspection_incompatible_daemon",
                "workflow_inspection",
                "Workflow inspection daemon is incompatible.",
            ),
            ClientError::Server { .. } => (
                "workflow_inspection_rejected",
                "workflow_inspection",
                "Daemon rejected workflow inspection; cause is unavailable.",
            ),
            _ => ("cli_failure", "operation", "CLI operation failed."),
        };
        Self {
            schema_version: 1,
            code,
            phase,
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspection_codec_subtypes_withhold_payloads() {
        use bcode_ipc::CodecError;
        const SECRET: &str = "SECRET_FIXTURE_MARKER";

        struct SecretSerialization;
        impl serde::Serialize for SecretSerialization {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom(SECRET))
            }
        }
        struct SecretDeserialization;
        impl<'de> serde::Deserialize<'de> for SecretDeserialization {
            fn deserialize<D: serde::Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
                Err(serde::de::Error::custom(SECRET))
            }
        }
        let serialize = bcode_ipc::encode_typed_stable(&SecretSerialization).unwrap_err();
        let bytes = bcode_ipc::encode_typed_stable(&SECRET).unwrap();
        let Err(deserialize) = bcode_ipc::decode_typed_stable::<SecretDeserialization>(&bytes)
        else {
            panic!("expected decoder rejection");
        };
        assert!(matches!(serialize, CodecError::Serialize(_)));
        assert!(matches!(deserialize, CodecError::Deserialize(_)));
        let cases = [
            (
                CodecError::PayloadTooLarge {
                    actual: usize::MAX,
                    max: 1,
                },
                "payload_too_large",
            ),
            (CodecError::InvalidChunk(SECRET.into()), "invalid_chunk"),
            (serialize, "serialize"),
            (deserialize, "deserialize"),
            (
                CodecError::EventConversion(SECRET.into()),
                "event_conversion",
            ),
            (
                CodecError::UnsupportedVersion {
                    actual: u16::MAX,
                    expected: 1,
                },
                "unsupported_version",
            ),
        ];
        for (codec, subtype) in cases {
            let error = CliError::WorkflowInspection {
                source: bcode_client::ClientError::Codec(codec),
            };
            let diagnostic = CliDiagnosticError::from_error(&error);
            assert_eq!(diagnostic.schema_version, 1);
            assert_eq!(diagnostic.code, "workflow_inspection_decode_failure");
            assert_eq!(diagnostic.phase, "workflow_inspection");
            assert_eq!(
                diagnostic.message,
                format!("Workflow inspection codec: {subtype}.")
            );
            let json = serde_json::to_string(&diagnostic).unwrap();
            assert!(!json.contains(SECRET));
            assert!(json.len() < 256);
        }
    }

    #[test]
    fn inspection_codec_io_kinds_are_finite_and_secret_safe() {
        use std::io::ErrorKind;
        for (kind, subtype) in [
            (ErrorKind::NotFound, "not_found"),
            (ErrorKind::PermissionDenied, "permission_denied"),
            (ErrorKind::ConnectionReset, "connection_reset"),
            (ErrorKind::BrokenPipe, "broken_pipe"),
            (ErrorKind::TimedOut, "timed_out"),
            (ErrorKind::UnexpectedEof, "unexpected_eof"),
            (ErrorKind::InvalidData, "invalid_data"),
            (ErrorKind::Interrupted, "interrupted"),
            (ErrorKind::Other, "other"),
            (ErrorKind::WouldBlock, "other"),
        ] {
            let error = CliError::WorkflowInspection {
                source: bcode_client::ClientError::Codec(bcode_ipc::CodecError::Io(
                    std::io::Error::new(kind, "SECRET_FIXTURE_MARKER"),
                )),
            };
            let diagnostic = CliDiagnosticError::from_error(&error);
            assert_eq!(diagnostic.code, "workflow_inspection_decode_failure");
            assert_eq!(
                diagnostic.message,
                format!("Workflow inspection codec: io_{subtype}.")
            );
            let json = serde_json::to_string(&diagnostic).unwrap();
            assert!(!json.contains("SECRET_FIXTURE_MARKER"));
            assert!(json.len() < 256);
        }
    }

    #[test]
    fn notification_timeout_is_distinct_and_secret_safe_for_both_cli_paths() {
        let timeout = || bcode_daemon_lifecycle::DaemonStartError::NotificationTimeout {
            log_path: "/private/SECRET_FIXTURE_MARKER".into(),
            recent_log: "SECRET_FIXTURE_MARKER still starting retry".into(),
        };
        for error in [
            CliError::DaemonStart(timeout()),
            CliError::Client(bcode_client::ClientError::DaemonStart(timeout())),
        ] {
            let diagnostic = CliDiagnosticError::from_error(&error);
            assert_eq!(diagnostic.code, "daemon_readiness_notification_timeout");
            assert_eq!(diagnostic.phase, "startup");
            assert!(
                diagnostic
                    .message
                    .contains("execution lifetime are unknown")
            );
            assert!(!diagnostic.message.contains("still starting"));
            assert!(
                !serde_json::to_string(&diagnostic)
                    .unwrap()
                    .contains("SECRET_FIXTURE_MARKER")
            );
        }
        let probe_timeout = CliError::Client(bcode_client::ClientError::DaemonStart(
            bcode_daemon_lifecycle::DaemonStartError::StartTimeout {
                log_path: "SECRET_FIXTURE_MARKER".into(),
                recent_log: "SECRET_FIXTURE_MARKER".into(),
            },
        ));
        assert_eq!(
            CliDiagnosticError::from_error(&probe_timeout).code,
            "daemon_start_failed"
        );
    }

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
    fn inspection_classifications_use_only_typed_categories_and_fixed_text() {
        use bcode_client::ClientError;
        let secret = "SECRET_FIXTURE_MARKER";
        let cases = [
            (
                ClientError::RequestTimeout {
                    timeout: std::time::Duration::from_secs(5),
                },
                "workflow_inspection_request_timeout",
            ),
            (
                ClientError::Protocol(secret.into()),
                "workflow_inspection_protocol_failure",
            ),
            (
                ClientError::UnexpectedResponse,
                "workflow_inspection_protocol_failure",
            ),
            (
                ClientError::UnexpectedEnvelope,
                "workflow_inspection_protocol_failure",
            ),
            (
                ClientError::IncompatibleDaemon {
                    message: secret.into(),
                },
                "workflow_inspection_incompatible_daemon",
            ),
            (
                ClientError::Server {
                    code: "connection_timeout".into(),
                    message: secret.into(),
                },
                "workflow_inspection_rejected",
            ),
            (
                ClientError::Server {
                    code: format!("future_category_{secret}"),
                    message: secret.into(),
                },
                "workflow_inspection_rejected",
            ),
            (
                ClientError::DaemonStartupTimeout {
                    timeout: std::time::Duration::from_secs(5),
                },
                "cli_failure",
            ),
            (
                ClientError::WorktreeCreate {
                    code: secret.into(),
                    message: secret.into(),
                    created_path: Some(secret.into()),
                },
                "cli_failure",
            ),
        ];
        for (source, expected) in cases {
            let diagnostic =
                CliDiagnosticError::from_error(&CliError::WorkflowInspection { source });
            let json = serde_json::to_value(diagnostic).unwrap();
            assert_eq!(diagnostic.code, expected);
            assert_eq!(diagnostic.schema_version, 1);
            assert_eq!(json.as_object().unwrap().len(), 4);
            assert!(!json.to_string().contains(secret));
            assert_eq!(
                diagnostic.phase,
                if expected == "cli_failure" {
                    "operation"
                } else {
                    "workflow_inspection"
                }
            );
        }
    }

    #[test]
    fn inspection_connect_timeout_preserves_existing_retry_classification() {
        let source = bcode_client::ClientError::ConnectTimeout {
            timeout: std::time::Duration::from_secs(5),
        };
        let scoped = CliDiagnosticError::from_error(&CliError::WorkflowInspection { source });
        let generic = CliDiagnosticError::from_error(&CliError::Client(
            bcode_client::ClientError::ConnectTimeout {
                timeout: std::time::Duration::from_secs(5),
            },
        ));
        assert_eq!(scoped, generic);
    }

    #[test]
    fn presentation_text_cannot_impersonate_a_connection_timeout_or_leak() {
        let error = CliError::WorkflowApplication {
            code: "connection_timeout".into(),
            message: "SECRET_FIXTURE_MARKER".into(),
        };
        let diagnostic = CliDiagnosticError::from_error(&error);
        assert_eq!(diagnostic.code, "unclassified_application_error");
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
        assert_eq!(
            CliDiagnosticError::from_error(&error).code,
            "request_timeout"
        );
    }

    #[test]
    fn known_application_codes_are_actionable_without_echoing_payloads() {
        for code in [
            "workflow_unavailable",
            "session_create_failed",
            "session_database_locked",
            "session_parent_not_found",
            "session_storage_migration_required",
            "session_children_unavailable",
            "session_not_found",
        ] {
            let error = CliError::Client(bcode_client::ClientError::Server {
                code: code.into(),
                message: "SECRET_FIXTURE_MARKER".into(),
            });
            let diagnostic = CliDiagnosticError::from_error(&error);
            assert_eq!(diagnostic.code, code);
            assert!(
                !serde_json::to_string(&diagnostic)
                    .unwrap()
                    .contains("SECRET_FIXTURE_MARKER")
            );
        }
    }

    #[test]
    fn wrapped_child_errors_preserve_typed_connection_classification() {
        let error = CliError::ChildSessionCreate {
            child_id: bcode_session_models::SessionId::new(),
            parent_id: bcode_session_models::SessionId::new(),
            source: bcode_client::ClientError::RequestTimeout {
                timeout: std::time::Duration::from_secs(15),
            },
        };
        assert_eq!(
            CliDiagnosticError::from_error(&error).code,
            "request_timeout"
        );
    }

    #[test]
    fn storage_full_is_actionable_without_raw_path_disclosure() {
        let error = CliError::Signal(std::io::Error::from(std::io::ErrorKind::StorageFull));
        assert_eq!(
            CliDiagnosticError::from_error(&error).code,
            "io_storage_full"
        );
    }

    #[test]
    fn io_failure_is_secret_safe_and_not_a_connection_timeout() {
        let error = CliError::Signal(std::io::Error::other("SECRET_FIXTURE_MARKER"));
        let diagnostic = CliDiagnosticError::from_error(&error);
        assert_eq!(diagnostic.phase, "io");
        assert_eq!(diagnostic.code, "io_failed");
        assert!(
            !serde_json::to_string(&diagnostic)
                .unwrap()
                .contains("SECRET_FIXTURE_MARKER")
        );
    }
}
