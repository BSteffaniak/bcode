#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Plugin-owned entry point for shallow, local session observation.

use std::collections::BTreeSet;

use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandOwner,
    CommandSurface, InvokeCommandRequest, InvokeCommandResponse, OP_INVOKE_COMMAND,
    SlashCommandContribution,
};
use bcode_plugin_sdk::prelude::*;

const PLUGIN_ID: &str = "bcode.xray";
const OPEN: &str = "xray.open";

/// Contributes Open X-ray without owning the host or any session execution.
#[derive(Default)]
pub struct XrayPlugin;

impl RustPlugin for XrayPlugin {
    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        registrar
            .register(&open_command())
            .map_err(|error| PluginError::failed(error.to_string()))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != COMMAND_INTERFACE_ID {
            return ServiceResponse::error("unsupported_interface", "unsupported X-ray interface");
        }
        if context.request.operation != OP_INVOKE_COMMAND {
            return ServiceResponse::error("unsupported_operation", "unsupported X-ray operation");
        }
        let Ok(request) = serde_json::from_slice::<InvokeCommandRequest>(&context.request.payload)
        else {
            return ServiceResponse::error("invalid_request", "invalid X-ray command request");
        };
        let response = invoke(&request);
        ServiceResponse::json(&response).unwrap_or_else(|_| {
            ServiceResponse::error("serialization_error", "X-ray response unavailable")
        })
    }
}

fn open_command() -> CommandContribution {
    CommandContribution {
        id: OPEN.to_owned(),
        title: "Open X-ray".to_owned(),
        description: Some("Open shallow, local observation of this session".to_owned()),
        category: Some("session".to_owned()),
        surfaces: BTreeSet::from([CommandSurface::Palette, CommandSurface::Slash]),
        owner: CommandOwner::Plugin {
            plugin_id: PLUGIN_ID.to_owned(),
        },
        action: CommandAction::Plugin {
            plugin_id: PLUGIN_ID.to_owned(),
            command_id: OPEN.to_owned(),
        },
        slash: Some(SlashCommandContribution {
            name: "xray".to_owned(),
            aliases: BTreeSet::new(),
        }),
        arguments: Vec::new(),
        session: bcode_command::CommandSessionRequirement::Required,
        execution: bcode_command::CommandExecution::Normal,
    }
}

fn invoke(request: &InvokeCommandRequest) -> InvokeCommandResponse {
    let session_id = request
        .context
        .as_ref()
        .and_then(|context| context.session_id);
    let (success, message, effects) = if request.command_id != OPEN {
        (false, "Unknown X-ray command", Vec::new())
    } else if !request.args.is_empty() {
        (
            false,
            "X-ray uses the current session; arguments are not accepted",
            Vec::new(),
        )
    } else if let Some(session_id) = session_id {
        (
            true,
            "Opening X-ray",
            vec![CommandEffect::OpenXray { session_id }],
        )
    } else {
        (false, "Open a session before opening X-ray", Vec::new())
    };
    InvokeCommandResponse {
        success,
        message: Some(message.to_owned()),
        updated_model: None,
        updated_provider: None,
        updated_thinking: None,
        effects,
    }
}

/// Static registration for the bundled plugin registry.
#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(XrayPlugin, include_str!("../bcode-plugin.toml"))
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(XrayPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_command::CommandInvocationContext;

    fn request() -> InvokeCommandRequest {
        InvokeCommandRequest {
            command_id: OPEN.to_owned(),
            args: std::collections::BTreeMap::new(),
            context: Some(CommandInvocationContext {
                session_id: Some("00000000-0000-0000-0000-000000000001".parse().unwrap()),
                working_directory: "/not-a-target".into(),
            }),
        }
    }

    #[test]
    fn exact_typed_current_session_is_the_only_target() {
        let request = request();
        let response = invoke(&request);
        assert!(response.success);
        assert_eq!(
            response.effects,
            vec![CommandEffect::OpenXray {
                session_id: request.context.unwrap().session_id.unwrap(),
            }]
        );
        assert!(response.updated_model.is_none());
        assert!(response.updated_provider.is_none());
        let encoded = serde_json::to_vec(&response).unwrap();
        let decoded: InvokeCommandResponse = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.effects, response.effects);
    }

    #[test]
    fn no_ambient_or_argument_target_fallback() {
        let mut request = request();
        request.context = None;
        assert!(!invoke(&request).success);
        assert_eq!(invoke(&request).effects, [] as [CommandEffect; 0]);
        request.context = Some(CommandInvocationContext {
            session_id: None,
            working_directory: "/not-a-target".into(),
        });
        assert!(!invoke(&request).success);
        request = self::request();
        request
            .args
            .insert("session_id".to_owned(), "other".to_owned());
        assert!(!invoke(&request).success);
        assert_eq!(invoke(&request).effects, [] as [CommandEffect; 0]);
        request.args.clear();
        request.command_id = "other".to_owned();
        assert!(!invoke(&request).success);
    }

    #[test]
    fn native_service_returns_the_typed_effect() {
        let request = request();
        let mut plugin = XrayPlugin;
        let response = plugin.invoke_service(NativeServiceContext {
            diagnostics: None,
            plugin_id: PLUGIN_ID.to_owned(),
            request: ServiceRequest {
                interface_id: COMMAND_INTERFACE_ID.to_owned(),
                operation: OP_INVOKE_COMMAND.to_owned(),
                payload: serde_json::to_vec(&request).unwrap(),
            },
            config: bcode_plugin_sdk::PluginConfigContext::default(),
            events: ServiceEventEmitter::default(),
            cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
            bridge: ServiceBridge::new(
                None,
                std::ptr::null_mut(),
                bcode_plugin_sdk::ServiceCancellation::default(),
            ),
            transient_progress_limits: TransientProgressLimits::default(),
        });
        assert!(response.error.is_none());
        let decoded: InvokeCommandResponse = serde_json::from_slice(&response.payload).unwrap();
        assert_eq!(decoded, invoke(&request));
    }

    #[test]
    fn palette_and_slash_share_the_plugin_route() {
        let command = open_command();
        assert!(command.surfaces.contains(&CommandSurface::Palette));
        assert!(command.surfaces.contains(&CommandSurface::Slash));
        let slash = command.slash.unwrap();
        assert_eq!(slash.name, "xray");
        assert_eq!(slash.aliases, BTreeSet::new());
        assert_eq!(
            command.session,
            bcode_command::CommandSessionRequirement::Required
        );
        assert_eq!(
            command.action,
            CommandAction::Plugin {
                plugin_id: PLUGIN_ID.to_owned(),
                command_id: OPEN.to_owned()
            }
        );
        assert_eq!(
            command.owner,
            CommandOwner::Plugin {
                plugin_id: PLUGIN_ID.to_owned()
            }
        );
    }
}
