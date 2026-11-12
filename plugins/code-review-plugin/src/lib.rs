#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Local Git code review plugin for Bcode.
//!
//! The plugin serves `bcode.code_review/v1` as JSON adapters over the `bcode_code_review` data
//! layer and, when statically bundled, contributes the review app's keymap actions to bcode's
//! keymap and registers the `bcode review` command, which runs the `bcode_code_review_ui` app in
//! process.

#[cfg(feature = "static-bundled")]
mod cli;

mod service;
#[cfg(feature = "static-bundled")]
pub mod tui;

pub use bcode_code_review::{CodeReviewConfig, CodeReviewStateLocation};
use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandOwner,
    CommandSurface, InvokeCommandRequest, InvokeCommandResponse, OP_INVOKE_COMMAND,
};
use bcode_plugin_sdk::prelude::*;
use service::json_response;

/// Code review plugin service interface.
pub const CODE_REVIEW_SERVICE_INTERFACE_ID: &str = "bcode.code_review/v1";

/// Operation that creates an ephemeral local review from a Git target.
pub const OP_CREATE_REVIEW: &str = "create_review";
/// Operation that lists persisted draft comments for a review target.
pub const OP_DRAFT_LIST: &str = "draft.list";
/// Operation that saves a persisted draft comment.
pub const OP_DRAFT_SAVE: &str = "draft.save";
/// Operation that deletes a persisted draft comment.
pub const OP_DRAFT_DELETE: &str = "draft.delete";
/// Operation that updates a persisted draft comment.
pub const OP_DRAFT_UPDATE: &str = "draft.update";
/// Operation that links a review thread to a Bcode session.
pub const OP_THREAD_LINK_SESSION: &str = "thread.link_session";
/// Operation that resolves or reopens a review thread.
pub const OP_THREAD_RESOLVE: &str = "thread.resolve";
/// Operation that returns review context metadata.
pub const OP_REVIEW_CONTEXT_GET: &str = "review.context.get";
/// Operation that lists review comments.
pub const OP_REVIEW_COMMENTS_LIST: &str = "review.comments.list";
/// Operation that returns one review thread.
pub const OP_REVIEW_THREAD_GET: &str = "review.thread.get";
/// Operation that returns file diff context.
pub const OP_REVIEW_DIFF_GET: &str = "review.diff.get";
/// Read-only workflow operation that validates one typed adversarial review assignment.
pub const OP_REVIEW_ASSIGNMENT_VALIDATE: &str = "review.assignment.validate";
/// Read-only workflow operation that materializes deterministic panel assignments.
pub const OP_REVIEW_ASSIGNMENTS_MATERIALIZE: &str = "review.assignments.materialize";
/// Read-only workflow operation that validates one typed report against its assignment.
pub const OP_REVIEW_REPORT_VALIDATE: &str = "review.report.validate";
/// Read-only workflow operation that validates complete consensus against one exact panel.
pub const OP_REVIEW_CONSENSUS_VALIDATE: &str = "review.consensus.validate";
/// Operation that returns a provider-neutral review bundle.
pub const OP_REVIEW_BUNDLE_GET: &str = "review.bundle.get";
/// Operation that lists review publishers.
pub const OP_REVIEW_PUBLISHERS_LIST: &str = "review.publishers.list";
/// Operation that previews a review publish operation.
pub const OP_REVIEW_PUBLISH_PREVIEW: &str = "review.publish.preview";
/// Operation that submits a review publish operation.
pub const OP_REVIEW_PUBLISH_SUBMIT: &str = "review.publish.submit";

/// Local code review plugin.
#[derive(Default)]
pub struct CodeReviewPlugin {
    /// Repository read state (long-lived `cat-file` processes and bounded caches).
    data: bcode_code_review::ReviewData,
}

impl RustPlugin for CodeReviewPlugin {
    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        registrar
            .register(&diff_toggle_command())
            .map_err(|error| PluginError::failed(error.to_string()))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id == COMMAND_INTERFACE_ID {
            return invoke_command_service(&context.request);
        }
        if context.request.interface_id != CODE_REVIEW_SERVICE_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported code review plugin service interface",
            );
        }
        if let Some(response) = service::dispatch_data(&mut self.data, &context.request) {
            return response;
        }
        service::dispatch_store(&context.request, || context.config_or_default()).unwrap_or_else(
            || {
                ServiceResponse::error(
                    "unsupported_operation",
                    "unsupported code review service operation",
                )
            },
        )
    }
}

fn diff_toggle_command() -> CommandContribution {
    CommandContribution {
        id: "diff.toggle".to_string(),
        title: "Diff: Toggle Panel".to_string(),
        description: Some("Show or hide the inline diff review panel".to_string()),
        category: Some("diff".to_string()),
        surfaces: std::collections::BTreeSet::from([CommandSurface::Palette]),
        slash: None,
        arguments: Vec::new(),
        session: bcode_command::CommandSessionRequirement::Optional,
        execution: bcode_command::CommandExecution::Normal,
        owner: CommandOwner::Plugin {
            plugin_id: "bcode.code_review".to_string(),
        },
        action: CommandAction::Plugin {
            plugin_id: "bcode.code_review".to_string(),
            command_id: "diff.toggle".to_string(),
        },
    }
}

fn invoke_command_service(request: &ServiceRequest) -> ServiceResponse {
    if request.operation != OP_INVOKE_COMMAND {
        return ServiceResponse::error(
            "unsupported_operation",
            "unsupported code review command operation",
        );
    }
    let Ok(request) = serde_json::from_slice::<InvokeCommandRequest>(&request.payload) else {
        return ServiceResponse::error(
            "invalid_request",
            "invalid code review command invocation request",
        );
    };
    match request.command_id.as_str() {
        "diff.toggle" => json_response(&InvokeCommandResponse {
            success: true,
            message: None,
            updated_model: None,
            updated_provider: None,
            updated_thinking: None,
            effects: vec![CommandEffect::ToggleSurface {
                surface_id: "diff".to_string(),
            }],
        }),
        _ => ServiceResponse::error("unknown_command", "unknown code review command"),
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    let mut vtable = bcode_plugin_sdk::static_plugin_vtable!(
        CodeReviewPlugin,
        include_str!("../bcode-plugin.toml")
    );
    vtable.cli_registration = Some(cli::registration);
    vtable
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(CodeReviewPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod command_registration_tests {
    use super::*;

    #[test]
    fn code_review_plugin_registers_diff_toggle_from_plugin_code() {
        extern "C" fn register_command(
            payload: *const u8,
            payload_len: usize,
            user_data: *mut std::ffi::c_void,
        ) {
            assert!(!payload.is_null());
            assert!(!user_data.is_null());
            let bytes = unsafe { std::slice::from_raw_parts(payload, payload_len) };
            let contribution = serde_json::from_slice::<CommandContribution>(bytes)
                .expect("command contribution should decode");
            let registry = unsafe { &mut *(user_data.cast::<bcode_command::CommandRegistry>()) };
            registry.register(contribution);
        }

        let mut plugin = CodeReviewPlugin::default();
        let mut registry = bcode_command::CommandRegistry::new();
        plugin
            .register_commands(CommandRegistrar::new(
                Some(register_command),
                std::ptr::from_mut(&mut registry).cast::<std::ffi::c_void>(),
            ))
            .expect("code review plugin should register commands");

        let commands = registry.commands_for_surface(&CommandSurface::Palette);

        assert!(commands.iter().any(|command| {
            command.id == "diff.toggle"
                && command.action
                    == CommandAction::Plugin {
                        plugin_id: "bcode.code_review".to_string(),
                        command_id: "diff.toggle".to_string(),
                    }
        }));
    }
}
