//! Versioned control service. No audio or final text crosses this service.

use bcode_command::{InvokeCommandRequest, InvokeCommandResponse};
use bcode_plugin_sdk::{ServiceRequest, ServiceResponse};
use bcode_vox::contracts::{Action, Error, Status};
use serde::{Deserialize, Serialize};

/// Control interface identity. This is not a tool service.
pub const INTERFACE_ID: &str = "bcode.vox.control/v1";
/// Supported payload contract version.
pub const VERSION: u32 = 1;
/// Upper bound on command/control input bytes.
pub const MAX_REQUEST_BYTES: usize = 4096;

/// A request to control the client-local runtime.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionRequest {
    /// Must equal [`VERSION`].
    pub version: u32,
    /// Explicit user-selected control action.
    pub action: Action,
}

/// A status query, with no user-controlled host or device selection.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusRequest {
    /// Must equal [`VERSION`].
    pub version: u32,
}

/// Action acceptance is not a claim of transcription or delivery completion.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionResponse {
    /// Response contract version.
    pub version: u32,
    /// The local runtime accepted the action. Never automatically retry.
    pub accepted: bool,
}

/// Availability and state, deliberately excluding transcripts and device details.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusResponse {
    /// Response contract version.
    pub version: u32,
    /// Whether this request reached a client-local runtime.
    pub local_frontend: bool,
    /// None when no local runtime is bound (not a synthetic Idle state).
    pub status: Option<Status>,
}

/// Minimal domain service consumed by the local frontend.
///
/// Implementations enqueue actions on the existing Vox worker; they must not
/// perform capture or recognition inline on the frontend's event loop.
pub trait LocalControl {
    /// Accept an action without automatically retrying delivery.
    ///
    /// # Errors
    /// Returns the runtime's busy, cancellation, invalid-configuration, or
    /// unavailable-capability failure. IPC implementations may also fail local
    /// connection, ownership, framing, or compatibility checks. Failure does not
    /// prove that a transported action was unexecuted; never retry automatically.
    fn action(&mut self, action: Action) -> bcode_vox::Result<()>;
    /// Read the current runtime snapshot without recording or transcribing.
    ///
    /// # Errors
    /// Returns a runtime availability failure or an IPC connection, ownership,
    /// framing, or compatibility error. Reading status never starts capture.
    fn status(&self) -> bcode_vox::Result<Status>;
}

fn unavailable() -> ServiceResponse {
    ServiceResponse::error(
        "local_frontend_required",
        "Vox requires an enabled client-local frontend; daemon microphone capture is disabled",
    )
}

fn runtime_failure(error: &Error) -> ServiceResponse {
    let (code, message) = match error {
        Error::Busy => ("busy", "Vox is busy"),
        Error::Cancelled => ("cancelled", "Vox operation cancelled"),
        Error::Invalid(_) => (
            "invalid_configuration",
            "Vox local configuration is invalid",
        ),
        Error::Unsupported(_) => ("unsupported", "Vox local capability is unavailable"),
        Error::Io(_) | Error::Backend(_) => (
            "local_runtime_failed",
            "Vox local runtime failed; delivery may be partial, do not retry automatically",
        ),
    };
    ServiceResponse::error(code, message)
}

fn encode(value: &impl Serialize) -> ServiceResponse {
    ServiceResponse::json(value)
        .unwrap_or_else(|_| ServiceResponse::error("encode_failed", "Vox response unavailable"))
}

fn status(control: Option<&dyn LocalControl>) -> ServiceResponse {
    let Some(control) = control else {
        return encode(&StatusResponse {
            version: VERSION,
            local_frontend: false,
            status: None,
        });
    };
    match control.status() {
        Ok(value) => {
            // Even a nominally safe runtime diagnostic is normalized at this boundary.
            let value = match value {
                Status::Failed { .. } => Status::Failed {
                    message: "Vox local operation failed".into(),
                },
                value => value,
            };
            encode(&StatusResponse {
                version: VERSION,
                local_frontend: true,
                status: Some(value),
            })
        }
        Err(error) => runtime_failure(&error),
    }
}

fn action(action: Action, control: Option<&mut dyn LocalControl>) -> ServiceResponse {
    let Some(control) = control else {
        return unavailable();
    };
    match control.action(action) {
        Ok(()) => encode(&ActionResponse {
            version: VERSION,
            accepted: true,
        }),
        Err(error) => runtime_failure(&error),
    }
}

/// Dispatch on a host-selected local binding, never on a request-provided locality flag.
///
/// Passing None is mandatory for daemon-side invocations. Unknown fields,
/// oversized input, unknown versions and unknown operations fail closed.
#[must_use]
pub fn dispatch(
    request: &ServiceRequest,
    control: Option<&mut dyn LocalControl>,
) -> ServiceResponse {
    if request.payload.len() > MAX_REQUEST_BYTES {
        return ServiceResponse::error("request_too_large", "Vox request exceeds input limit");
    }
    if request.interface_id == bcode_command::COMMAND_INTERFACE_ID {
        return command(request, control);
    }
    if request.interface_id != INTERFACE_ID {
        return ServiceResponse::error("unsupported_interface", "Unsupported Vox interface");
    }
    match request.operation.as_str() {
        "action" => {
            let Ok(value) = request.payload_json::<ActionRequest>() else {
                return ServiceResponse::error("invalid_request", "Invalid Vox action request");
            };
            if value.version != VERSION {
                return unsupported_version();
            }
            action(value.action, control)
        }
        "status" => {
            let Ok(value) = request.payload_json::<StatusRequest>() else {
                return ServiceResponse::error("invalid_request", "Invalid Vox status request");
            };
            if value.version != VERSION {
                return unsupported_version();
            }
            status(control.as_deref())
        }
        _ => ServiceResponse::error("unsupported_operation", "Unsupported Vox operation"),
    }
}

fn unsupported_version() -> ServiceResponse {
    ServiceResponse::error("unsupported_version", "Unsupported Vox contract version")
}

fn command(request: &ServiceRequest, control: Option<&mut dyn LocalControl>) -> ServiceResponse {
    if request.operation != bcode_command::OP_INVOKE_COMMAND {
        return ServiceResponse::error(
            "unsupported_operation",
            "Unsupported Vox command operation",
        );
    }
    let Ok(value) = request.payload_json::<InvokeCommandRequest>() else {
        return ServiceResponse::error("invalid_request", "Invalid Vox command request");
    };
    if !value.args.is_empty() {
        return ServiceResponse::error("invalid_arguments", "Vox commands do not accept arguments");
    }
    let selected = match value.command_id.as_str() {
        "vox.start" => Some(Action::Start),
        "vox.stop" => Some(Action::Stop),
        "vox.toggle" => Some(Action::Toggle),
        "vox.cancel" => Some(Action::Cancel),
        "vox.status" => None,
        _ => return ServiceResponse::error("unknown_command", "Unknown Vox command"),
    };
    let response = if let Some(selected) = selected {
        action(selected, control)
    } else {
        status(control.as_deref())
    };
    if response.error.is_some() {
        return response;
    }
    let message = if selected.is_some() {
        "Vox action accepted; completion is reported by local status".to_owned()
    } else {
        let Ok(value) = response.payload_json::<StatusResponse>() else {
            return ServiceResponse::error("encode_failed", "Vox status unavailable");
        };
        match value.status {
            None => "Vox unavailable: no client-local frontend".into(),
            Some(Status::Idle) => "Vox idle".into(),
            Some(Status::Recording { .. }) => "Vox recording".into(),
            Some(Status::Transcribing { .. }) => "Vox transcribing".into(),
            Some(Status::Delivering { .. }) => "Vox delivering".into(),
            Some(Status::Failed { .. }) => "Vox local operation failed".into(),
        }
    };
    encode(&InvokeCommandResponse {
        success: true,
        message: Some(message),
        updated_model: None,
        updated_provider: None,
        updated_thinking: None,
        effects: Vec::new(),
    })
}
