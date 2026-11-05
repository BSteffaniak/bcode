#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! External-session entry points. This plugin owns no process, session store or scheduler.
//! The invocation-scoped application bridge owns admission and canonical receipts.

pub mod configuration;
mod protocol;
mod tools;
mod workflow;
pub use workflow::{EXECUTE_OPERATION, ManagedWorkflowExecution};

pub use protocol::{
    MANAGED_SESSION_INTERFACE_ID, MAX_REQUEST_BYTES, ManagedSessionRequest, OPERATIONS,
    validated_payload,
};

use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandOwner,
    CommandSurface, InvokeCommandRequest, InvokeCommandResponse, OP_INVOKE_COMMAND,
};
use bcode_plugin_sdk::prelude::*;
use std::collections::BTreeSet;

/// Manifest identity of the disableable generic harness integration.
pub const PLUGIN_ID: &str = "bcode.harness";

/// Stateless integration; canonical history and recovery stay host-owned.
#[derive(Default)]
pub struct HarnessPlugin;

impl RustPlugin for HarnessPlugin {
    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        for operation in OPERATIONS {
            let id = format!("harness.{operation}");
            registrar.register(&CommandContribution {
                id: id.clone(),
                title: format!("External Session: {operation}"),
                description: Some("Use the managed-session application boundary; retain mutation identities to reconcile lost acknowledgements".into()),
                category: Some("harness".into()),
                surfaces: BTreeSet::from([CommandSurface::Palette]),
                slash: None,
                arguments: vec![bcode_command::CommandArgumentContribution {
                    name: "request_json".into(), kind: bcode_command::CommandArgumentKind::Json,
                    required: true, description: Some("Bounded ManagedSessionRequest; no sensitive auth material".into()),
                }],
                session: bcode_command::CommandSessionRequirement::Optional,
                execution: bcode_command::CommandExecution::Normal,
                owner: CommandOwner::Plugin { plugin_id: PLUGIN_ID.into() },
                action: CommandAction::Plugin { plugin_id: PLUGIN_ID.into(), command_id: id },
            }).map_err(|_| PluginError::failed("harness command registration failed"))?;
        }
        Ok(())
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match (
            context.request.interface_id.as_str(),
            context.request.operation.as_str(),
        ) {
            (configuration::INTERFACE_ID, _) => configuration::invoke(&context),
            (bcode_tool::TOOL_SERVICE_INTERFACE_ID, _) => tools::invoke(&context),
            (bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID, _) => workflow::invoke(&context),
            (COMMAND_INTERFACE_ID, OP_INVOKE_COMMAND) => invoke_command(&context),
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported managed harness operation",
            ),
        }
    }
}

fn invoke_command(context: &NativeServiceContext) -> ServiceResponse {
    let Ok(request) = context.request.payload_json::<InvokeCommandRequest>() else {
        return ServiceResponse::error("invalid_request", "invalid harness command");
    };
    let Some(operation) = request
        .command_id
        .strip_prefix("harness.")
        .filter(|op| OPERATIONS.contains(op))
    else {
        return ServiceResponse::error("unknown_command", "unknown harness command");
    };
    // Command args carry no route or caller authority. The host verifies the active command
    // invocation and separately admits the operation, just as for prepared tools.
    let Some(encoded) = request
        .args
        .get("request_json")
        .filter(|text| text.len() <= MAX_REQUEST_BYTES)
    else {
        return ServiceResponse::error(
            "invalid_request",
            "supply bounded request_json with retained mutation identities",
        );
    };
    if request.args.len() != 1 {
        return ServiceResponse::error("invalid_request", "unexpected harness command arguments");
    }
    let Ok(value) = serde_json::from_str(encoded) else {
        return ServiceResponse::error("invalid_request", "invalid harness request JSON");
    };
    let Ok(payload) = validated_payload(operation, value) else {
        return ServiceResponse::error("invalid_request", "invalid managed harness request");
    };
    // Correlation is not authority; the command bridge must bind its active caller independently.
    let invocation_id = payload["harness"]["request_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    match tools::bridge(context, invocation_id, None, operation, payload) {
        Ok(payload) => json_response(&InvokeCommandResponse {
            success: true,
            message: Some(
                "Managed session operation acknowledged; receipt is not assignment completion"
                    .into(),
            ),
            updated_model: None,
            updated_provider: None,
            updated_thinking: None,
            effects: vec![CommandEffect::AppendText {
                text: payload.to_string(),
                format: bcode_command::CommandTextFormat::Json,
            }],
        }),
        Err(()) => ServiceResponse::error(
            "managed_session_unavailable",
            "managed session application rejected or could not reconcile this operation",
        ),
    }
}

pub(crate) fn json_response<T: serde::Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value).unwrap_or_else(|_| {
        ServiceResponse::error(
            "encode_failed",
            "managed harness response could not be encoded",
        )
    })
}

/// Statically bundled registration with the same manifest and service implementation.
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(HarnessPlugin, include_str!("../bcode-plugin.toml"))
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(HarnessPlugin, include_str!("../bcode-plugin.toml"));
