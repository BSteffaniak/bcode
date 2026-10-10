#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Disableable milestone policy and command plugin. Canonical reads remain host-owned.

pub mod policy;

use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandExecution,
    CommandOwner, CommandSessionRequirement, CommandSurface, CommandTextFormat,
    InvokeCommandRequest, InvokeCommandResponse, OP_INVOKE_COMMAND, SlashCommandContribution,
};
use bcode_plugin_sdk::milestones::{MilestonesHostRequest, MilestonesHostResponse};
use bcode_plugin_sdk::prelude::*;
use bcode_plugin_sdk::{ServiceBridgeRequest, ServiceBridgeResponse};
use bcode_tool::{ToolInvocationServiceRequest, ToolInvocationServiceResolution};
use policy::{PolicyRequest, PolicyResponse};
use std::collections::BTreeSet;

const PLUGIN_ID: &str = "bcode.session-milestones";
const COMMAND_ID: &str = "session-milestones.milestones";

/// Native milestone policy plugin, containing no provider or persistence implementation.
#[derive(Default)]
pub struct SessionMilestonesPlugin;

impl RustPlugin for SessionMilestonesPlugin {
    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        registrar
            .register(&command())
            .map_err(|error| PluginError::failed(error.to_string()))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id == bcode_plugin_sdk::settled_turn::INTERFACE_ID {
            return invoke_observer(&context);
        }
        if context.request.interface_id == policy::INTERFACE_ID {
            return invoke_policy(&context);
        }
        if context.request.interface_id != COMMAND_INTERFACE_ID
            || context.request.operation != OP_INVOKE_COMMAND
        {
            return ServiceResponse::error(
                "unsupported_operation",
                "unsupported milestone operation",
            );
        }
        invoke_command(&context)
    }
}

fn command() -> CommandContribution {
    CommandContribution {
        id: COMMAND_ID.into(),
        title: "Session Milestones".into(),
        description: Some(
            "Read milestones; use /milestones generate for explicitly configured paid generation"
                .into(),
        ),
        category: Some("session".into()),
        surfaces: BTreeSet::from([CommandSurface::Palette, CommandSurface::Slash]),
        slash: Some(SlashCommandContribution {
            name: "milestones".into(),
            aliases: BTreeSet::new(),
        }),
        arguments: Vec::new(),
        session: CommandSessionRequirement::Required,
        execution: CommandExecution::Immediate,
        owner: CommandOwner::Plugin {
            plugin_id: PLUGIN_ID.into(),
        },
        action: CommandAction::Plugin {
            plugin_id: PLUGIN_ID.into(),
            command_id: COMMAND_ID.into(),
        },
    }
}

fn observer_response(
    facts: &bcode_plugin_sdk::settled_turn::SettledTurn,
) -> bcode_plugin_sdk::settled_turn::ObserverResponse {
    use bcode_plugin_sdk::settled_turn::ObserverResponse;
    if facts.cancellation_requested
        || facts.continuity == bcode_plugin_sdk::settled_turn::ExecutionContinuity::Recovery
        || facts.outcome != bcode_session_models::ModelTurnOutcome::Completed
        || facts.priority != bcode_session_models::TurnPriority::Interactive
        || facts.has_origin
        || facts.correlated
    {
        return ObserverResponse::NoAction;
    }
    ObserverResponse::Application {
        request: ToolInvocationServiceRequest {
            invocation_id: "settled-turn".into(),
            request_id: format!("settled-{}-{}", facts.session_id, facts.trigger_sequence),
            route_id: None,
            interface_id: policy::HOST_INTERFACE_ID.into(),
            operation: "request".into(),
            payload: serde_json::json!({"operation": "evaluate_settled", "session_id": facts.session_id}),
        },
    }
}

fn invoke_observer(context: &NativeServiceContext) -> ServiceResponse {
    if context.request.operation != bcode_plugin_sdk::settled_turn::OP_OBSERVE
        || context.cancellation.is_cancelled()
    {
        return ServiceResponse::error("observer_unavailable", "settled observer unavailable");
    }
    let Ok(facts) = context
        .request
        .payload_json::<bcode_plugin_sdk::settled_turn::SettledTurn>()
    else {
        return ServiceResponse::error("invalid_request", "invalid settled facts");
    };
    json_response(&observer_response(&facts))
}

fn invoke_policy(context: &NativeServiceContext) -> ServiceResponse {
    if context.request.operation != "request" {
        return ServiceResponse::error(
            "unsupported_operation",
            "unsupported milestone policy operation",
        );
    }
    let Ok(request) = context.request.payload_json::<PolicyRequest>() else {
        return ServiceResponse::error("invalid_request", "invalid milestone policy request");
    };
    let response = match request {
        PolicyRequest::Decide { config, facts } => PolicyResponse::Decide {
            decision: policy::decide(&config, &facts),
        },
        PolicyRequest::Prompt {
            context,
            max_input_tokens,
        } => match policy::prompt(&context, max_input_tokens) {
            Ok((system, user)) => PolicyResponse::Prompt { system, user },
            Err(error) => return ServiceResponse::error("invalid_context", error),
        },
        PolicyRequest::Validate { text } => match policy::validate(&text) {
            Ok(generated) => PolicyResponse::Validate { generated },
            Err(error) => return ServiceResponse::error("invalid_output", error),
        },
    };
    json_response(&response)
}

fn command_request(
    session_id: bcode_session_models::SessionId,
    args: &str,
) -> Result<MilestonesHostRequest, &'static str> {
    match args.trim() {
        "" | "list" => Ok(MilestonesHostRequest::Read { session_id }),
        "generate" => Ok(MilestonesHostRequest::Generate { session_id }),
        _ => Err("usage: /milestones [list|generate]"),
    }
}

fn invoke_command(context: &NativeServiceContext) -> ServiceResponse {
    let Ok(request) = context.request.payload_json::<InvokeCommandRequest>() else {
        return ServiceResponse::error("invalid_request", "invalid milestone command");
    };
    if request.command_id != COMMAND_ID {
        return ServiceResponse::error("unknown_command", "unknown milestone command");
    }
    let Some(session_id) = request.context.and_then(|context| context.session_id) else {
        return ServiceResponse::error("session_required", "milestones require an active session");
    };
    let args = request
        .args
        .get("arguments")
        .map_or("", String::as_str)
        .trim();
    let host_request = match command_request(session_id, args) {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_arguments", error),
    };
    let Ok(payload) = serde_json::to_value(host_request) else {
        return ServiceResponse::error("encode_failed", "cannot encode milestone request");
    };
    let response = context.bridge.request(&ServiceBridgeRequest::InvokeService(
        ToolInvocationServiceRequest {
            invocation_id: COMMAND_ID.into(),
            request_id: format!("{COMMAND_ID}-{session_id}"),
            route_id: None,
            interface_id: policy::HOST_INTERFACE_ID.into(),
            operation: "request".into(),
            payload,
        },
    ));
    let response: MilestonesHostResponse = match response {
        Ok(ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded {
            payload,
        })) => match serde_json::from_value(payload) {
            Ok(response) => response,
            Err(_) => {
                return ServiceResponse::error(
                    "invalid_response",
                    "invalid milestone host response",
                );
            }
        },
        Ok(ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Cancelled)) => {
            return ServiceResponse::error("cancelled", "milestone operation cancelled");
        }
        _ => return ServiceResponse::error("unavailable", "milestone host service unavailable"),
    };
    json_response(&InvokeCommandResponse {
        success: response.success,
        message: None,
        updated_model: None,
        updated_provider: None,
        updated_thinking: None,
        effects: vec![CommandEffect::AppendText {
            text: response.text,
            format: CommandTextFormat::PlainText,
        }],
    })
}

fn json_response<T: serde::Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value).unwrap_or_else(|_| {
        ServiceResponse::error("encode_failed", "cannot encode milestone response")
    })
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(
        SessionMilestonesPlugin,
        include_str!("../bcode-plugin.toml")
    )
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(
    SessionMilestonesPlugin,
    include_str!("../bcode-plugin.toml")
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_never_requests_paid_generation_implicitly() {
        let session_id = bcode_session_models::SessionId::new();
        assert!(matches!(
            command_request(session_id, ""),
            Ok(MilestonesHostRequest::Read { .. })
        ));
        assert!(matches!(
            command_request(session_id, "list"),
            Ok(MilestonesHostRequest::Read { .. })
        ));
        assert!(matches!(
            command_request(session_id, "generate"),
            Ok(MilestonesHostRequest::Generate { .. })
        ));
        assert!(command_request(session_id, "force").is_err());
    }

    #[test]
    fn milestone_command_is_session_scoped_and_plugin_owned() {
        let command = command();
        assert_eq!(command.slash.unwrap().name, "milestones");
        assert_eq!(command.session, CommandSessionRequirement::Required);
        assert_eq!(
            command.owner,
            CommandOwner::Plugin {
                plugin_id: PLUGIN_ID.into()
            }
        );
    }
}

#[cfg(test)]
mod observer_tests {
    use super::observer_response;
    use bcode_plugin_sdk::settled_turn::{ObserverResponse, SettledTurn};
    use bcode_session_models::{ModelTurnOutcome, SessionId, TurnPriority};
    #[test]
    fn applicability_is_plugin_owned_and_automatic_is_not_explicit() {
        let facts = SettledTurn {
            session_id: SessionId::new(),
            turn_id: "turn".into(),
            trigger_sequence: 2,
            outcome: ModelTurnOutcome::Completed,
            priority: TurnPriority::Interactive,
            has_origin: false,
            correlated: false,
            continuity: bcode_plugin_sdk::settled_turn::ExecutionContinuity::Fresh,
            cancellation_requested: false,
        };
        let ObserverResponse::Application { request } = observer_response(&facts) else {
            panic!("eligible turn");
        };
        assert_eq!(request.payload["operation"], "evaluate_settled");
        for excluded in [
            SettledTurn {
                cancellation_requested: true,
                ..facts.clone()
            },
            SettledTurn {
                continuity: bcode_plugin_sdk::settled_turn::ExecutionContinuity::Recovery,
                ..facts.clone()
            },
            SettledTurn {
                has_origin: true,
                ..facts.clone()
            },
            SettledTurn {
                correlated: true,
                ..facts.clone()
            },
            SettledTurn {
                priority: TurnPriority::Background,
                ..facts.clone()
            },
            SettledTurn {
                outcome: ModelTurnOutcome::Cancelled,
                ..facts.clone()
            },
            SettledTurn {
                outcome: ModelTurnOutcome::Error,
                ..facts
            },
        ] {
            assert!(matches!(
                observer_response(&excluded),
                ObserverResponse::NoAction
            ));
        }
    }
}
