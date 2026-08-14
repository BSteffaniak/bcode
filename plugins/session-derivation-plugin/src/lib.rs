#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Plugin-owned fork and clone product workflows over generic session derivation.

use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandExecution,
    CommandOwner, CommandSessionRequirement, CommandSurface, InvokeCommandRequest,
    InvokeCommandResponse, OP_INVOKE_COMMAND, SessionOpenFocus, SlashCommandContribution,
};
use bcode_plugin_sdk::prelude::*;
use bcode_plugin_sdk::{
    SESSION_DERIVATION_INTERFACE_ID, ServiceBridgeRequest, ServiceBridgeResponse,
    SessionDerivationServiceRequest, SessionDerivationServiceResponse,
};
use bcode_session_models::{
    SESSION_DERIVATION_CONTRACT_VERSION, SessionDerivationLineage, SessionDerivationOperationId,
    SessionDerivationRequest, SessionDerivationSourcePolicy, SessionDerivationTerminalOutcome,
};
use bcode_tool::{ToolInvocationServiceRequest, ToolInvocationServiceResolution};
use std::collections::{BTreeMap, BTreeSet};

const PLUGIN_ID: &str = "bcode.session-derivation";
const FORK_COMMAND_ID: &str = "session-derivation.fork";
const CLONE_COMMAND_ID: &str = "session-derivation.clone";

#[derive(Default)]
pub struct SessionDerivationPlugin;

impl RustPlugin for SessionDerivationPlugin {
    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        for command in commands() {
            registrar
                .register(&command)
                .map_err(|error| PluginError::failed(error.to_string()))?;
        }
        Ok(())
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != COMMAND_INTERFACE_ID
            || context.request.operation != OP_INVOKE_COMMAND
        {
            return ServiceResponse::error("unsupported_operation", "unsupported operation");
        }
        let request = match context.request.payload_json::<InvokeCommandRequest>() {
            Ok(request) => request,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
        invoke_command(&context, request)
    }
}

fn commands() -> Vec<CommandContribution> {
    vec![
        command(
            FORK_COMMAND_ID,
            "fork",
            "Fork Session",
            "Create a new session before a selected prompt sequence",
        ),
        command(
            CLONE_COMMAND_ID,
            "clone",
            "Clone Session",
            "Copy the stable current conversation into a new session",
        ),
    ]
}

fn command(id: &str, slash_name: &str, title: &str, description: &str) -> CommandContribution {
    CommandContribution {
        id: id.to_owned(),
        title: title.to_owned(),
        description: Some(description.to_owned()),
        category: Some("session".to_owned()),
        surfaces: BTreeSet::from([CommandSurface::Palette, CommandSurface::Slash]),
        slash: Some(SlashCommandContribution {
            name: slash_name.to_owned(),
            aliases: BTreeSet::new(),
        }),
        arguments: Vec::new(),
        session: CommandSessionRequirement::Required,
        execution: CommandExecution::Immediate,
        owner: CommandOwner::Plugin {
            plugin_id: PLUGIN_ID.to_owned(),
        },
        action: CommandAction::Plugin {
            plugin_id: PLUGIN_ID.to_owned(),
            command_id: id.to_owned(),
        },
    }
}

fn invoke_command(
    context: &NativeServiceContext,
    request: InvokeCommandRequest,
) -> ServiceResponse {
    let Some(command_context) = request.context else {
        return ServiceResponse::error("session_required", "command requires session context");
    };
    let Some(session_id) = command_context.session_id else {
        return ServiceResponse::error("session_required", "command requires an active session");
    };
    let snapshot = match call_derivation_service(
        context,
        &request.command_id,
        SessionDerivationServiceRequest::Snapshot { session_id },
    ) {
        Ok(SessionDerivationServiceResponse::Snapshot { snapshot }) => snapshot,
        Ok(_) => {
            return ServiceResponse::error("unexpected_response", "unexpected snapshot response");
        }
        Err(error) => return ServiceResponse::error("snapshot_failed", error),
    };
    let args = parse_arguments(request.args.get("arguments").map_or("", String::as_str));
    let (operation_kind, cutoff_sequence, selected_source_sequence, default_prefix) = match request
        .command_id
        .as_str()
    {
        CLONE_COMMAND_ID => ("clone", snapshot.latest_sequence, None, "clone"),
        FORK_COMMAND_ID => {
            let Some(sequence) = args
                .get("sequence")
                .and_then(|value| value.parse::<u64>().ok())
            else {
                return ServiceResponse::error(
                    "prompt_sequence_required",
                    "fork currently requires `sequence=<user-message-sequence>`; the generic selection surface is not yet available",
                );
            };
            ("fork", sequence.saturating_sub(1), Some(sequence), "fork")
        }
        _ => {
            return ServiceResponse::error("unknown_command", "unknown session derivation command");
        }
    };
    let destination_name = args.get("name").cloned().or_else(|| {
        snapshot
            .title
            .as_ref()
            .map(|title| format!("[{default_prefix}] {title}"))
    });
    let derive = SessionDerivationRequest {
        version: SESSION_DERIVATION_CONTRACT_VERSION,
        operation_id: SessionDerivationOperationId::new(),
        idempotency_key: format!("{}-{}", operation_kind, SessionDerivationOperationId::new()),
        source: snapshot,
        source_policy: SessionDerivationSourcePolicy::ExactGeneration,
        cutoff_sequence,
        destination_working_directory: None,
        destination_name,
        initial_draft: args.get("draft").cloned(),
        lineage: SessionDerivationLineage {
            producer: PLUGIN_ID.to_owned(),
            operation_kind: format!("{PLUGIN_ID}/{operation_kind}"),
            selected_source_sequence,
        },
    };
    let outcome = match call_derivation_service(
        context,
        &request.command_id,
        SessionDerivationServiceRequest::Derive {
            request: Box::new(derive),
        },
    ) {
        Ok(SessionDerivationServiceResponse::Derived { outcome }) => outcome,
        Ok(_) => {
            return ServiceResponse::error("unexpected_response", "unexpected derivation response");
        }
        Err(error) => return ServiceResponse::error("derivation_failed", error),
    };
    let SessionDerivationTerminalOutcome::Succeeded { session } = outcome else {
        return ServiceResponse::error(
            "derivation_incomplete",
            "session derivation did not succeed",
        );
    };
    json_response(&InvokeCommandResponse {
        success: true,
        message: Some(format!("Created {}", session.display_title())),
        updated_model: None,
        updated_provider: None,
        updated_thinking: None,
        effects: vec![CommandEffect::OpenSession {
            session_id: session.id,
            focus: if operation_kind == "fork" {
                SessionOpenFocus::Composer
            } else {
                SessionOpenFocus::Default
            },
        }],
    })
}

fn call_derivation_service(
    context: &NativeServiceContext,
    invocation_id: &str,
    request: SessionDerivationServiceRequest,
) -> Result<SessionDerivationServiceResponse, String> {
    let payload = serde_json::to_value(request).map_err(|error| error.to_string())?;
    let response = context
        .bridge
        .request(&ServiceBridgeRequest::InvokeService(
            ToolInvocationServiceRequest {
                invocation_id: invocation_id.to_owned(),
                request_id: format!("{invocation_id}-session-derivation"),
                route_id: None,
                interface_id: SESSION_DERIVATION_INTERFACE_ID.to_owned(),
                operation: "request".to_owned(),
                payload,
            },
        ))
        .map_err(|error| error.to_string())?;
    match response {
        ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded { payload }) => {
            serde_json::from_value(payload).map_err(|error| error.to_string())
        }
        ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Failed {
            message, ..
        }) => Err(message),
        ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Cancelled) => {
            Err("session derivation cancelled".to_owned())
        }
        _ => Err("session derivation service unavailable".to_owned()),
    }
}

fn parse_arguments(arguments: &str) -> BTreeMap<String, String> {
    arguments
        .split_whitespace()
        .filter_map(|part| part.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn json_response<T: serde::Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value)
        .unwrap_or_else(|error| ServiceResponse::error("encode_failed", error.to_string()))
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(
        SessionDerivationPlugin,
        include_str!("../bcode-plugin.toml")
    )
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(
    SessionDerivationPlugin,
    include_str!("../bcode-plugin.toml")
);
