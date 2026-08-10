#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Shell execution tool plugin for Bcode.
//!
//! This plugin exclusively owns shell/terminal recording schemas, PTY byte capture, replay
//! interpretation, terminal emulation, and shell-result rendering. Host, session, server, and
//! generic TUI-extension code must treat shell recordings as opaque tool artifacts and must not
//! branch on shell schema IDs, recording reference keys, MIME types, ANSI, PTY, resize, grid, or
//! scrollback semantics. Live presentation uses invocation-owned primary replacement updates
//! carrying bounded recording artifact revisions; durable replay uses shell-owned artifact
//! references.

mod contracts;
pub mod recording;
#[cfg(feature = "static-bundled")]
pub mod shell_run_tui;
mod terminal_clean;

use bcode_config::{
    ShellToolConfig, ShellToolEnvAutoFallback, ShellToolEnvConfig, ShellToolEnvMode,
    ShellToolOutputConfig, ShellToolPreludeGateTarget, default_config_paths_from_with_environment,
    load_config_from_paths_with_environment,
};
use bcode_plugin_sdk::path::display;
use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolArtifact,
    ToolArtifactRef, ToolContributionArtifact, ToolDefinition, ToolInvocationLifecycleEvent,
    ToolInvocationLifecycleStage, ToolInvocationRequest, ToolInvocationResponse,
    ToolInvocationResult, ToolList,
};
use contracts::{
    DEFAULT_SHELL_TIMEOUT_MS, SHELL_INVOCATION_INPUT_SCHEMA, SHELL_RECORDING_CONTENT_TYPE,
    SHELL_RECORDING_REF_KEY, SHELL_RUN_SCHEMA, SHELL_RUN_TOOL_NAME, SHELL_SCHEMA_VERSION,
    ShellInvocationAction, ShellLiveRecordingPayload, ShellRunArguments, ShellRunResult,
    TERMINAL_PTY_STREAM_CONTENT_TYPE, TERMINAL_PTY_STREAM_REF_KEY,
};
pub use contracts::{
    ShellWorkflowCommandPlan, ShellWorkflowCommandPlanResult, ShellWorkflowCommandResult,
    ShellWorkflowCommandStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

const DEFAULT_TERMINAL_COLUMNS: u16 = 120;
const DEFAULT_TERMINAL_ROWS: u16 = 30;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 10 * 1024 * 1024;
const MAX_INLINE_TERMINAL_OUTPUT_BYTES: usize = 16 * 1024;

/// shell plugin.
#[derive(Default)]
pub struct ShellPlugin;

impl ConcurrentRustPlugin for ShellPlugin {
    fn invoke_service_concurrent(&self, context: NativeServiceContext) -> ServiceResponse {
        invoke_shell_service(&context)
    }
}

impl RustPlugin for ShellPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        invoke_shell_service(&context)
    }
}

fn invoke_shell_service(context: &NativeServiceContext) -> ServiceResponse {
    if context.request.interface_id == bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID {
        return if context.request.operation == bcode_workflow::WORKFLOW_BLOCK_PREPARE_OPERATION {
            prepare_workflow_block_contract(&context.request)
        } else {
            invoke_workflow_block_contract(context)
        };
    }
    if context.request.interface_id != TOOL_SERVICE_INTERFACE_ID {
        return ServiceResponse::error(
            "unsupported_interface",
            "unsupported shell plugin service interface",
        );
    }

    match context.request.operation.as_str() {
        OP_LIST_TOOLS => list_tools(&context.request),
        bcode_tool::OP_PREPARE_TOOL => prepare_shell_tool(&context.request),
        OP_INVOKE_TOOL => invoke_tool(context),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported tool service operation",
        ),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct ShellPreparationDescriptor {
    #[serde(default)]
    workspace_root: Option<PathBuf>,
    #[serde(default)]
    artifact_root: Option<PathBuf>,
    timeout_ms: u64,
}

fn shell_preparation_descriptor(
    preparation: &bcode_tool::ToolPreparationRequest,
) -> Result<ShellPreparationDescriptor, String> {
    let timeout_ms = preparation
        .invocation
        .arguments
        .get("timeout_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(DEFAULT_SHELL_TIMEOUT_MS);
    Ok(ShellPreparationDescriptor {
        workspace_root: shell_context_path(
            preparation,
            bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA,
            bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION,
            "working_directory",
        )?,
        artifact_root: shell_context_path(
            preparation,
            bcode_tool::TOOL_ARTIFACT_CONTEXT_SCHEMA,
            bcode_tool::TOOL_ARTIFACT_CONTEXT_SCHEMA_VERSION,
            "root",
        )?,
        timeout_ms,
    })
}

fn shell_context_path(
    preparation: &bcode_tool::ToolPreparationRequest,
    schema: &str,
    expected_version: u32,
    field: &str,
) -> Result<Option<PathBuf>, String> {
    let mut matching = preparation
        .host_context
        .iter()
        .filter(|entry| entry.schema == schema);
    let Some(entry) = matching.next() else {
        return Ok(None);
    };
    if matching.next().is_some() {
        return Err(format!("duplicate Shell host context for {schema}"));
    }
    if entry.schema_version != expected_version {
        return Err(format!(
            "unsupported Shell host context version for {schema}: {}; expected {expected_version}",
            entry.schema_version
        ));
    }
    let path = entry
        .payload
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("Shell host context {schema} field {field} is missing"))?;
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(format!(
            "Shell host context {schema} field {field} must be absolute"
        ));
    }
    Ok(Some(path))
}

fn prepare_shell_tool(request: &ServiceRequest) -> ServiceResponse {
    let preparation = match request.payload_json::<bcode_tool::ToolPreparationRequest>() {
        Ok(preparation) => preparation,
        Err(error) => return ServiceResponse::error("invalid_preparation", error.to_string()),
    };
    let definition = shell_tool_definition();
    if preparation.invocation.tool_name != definition.name {
        return ServiceResponse::error(
            "invalid_preparation",
            format!(
                "tool not found during preparation: {}",
                preparation.invocation.tool_name
            ),
        );
    }
    let descriptor = match shell_preparation_descriptor(&preparation) {
        Ok(descriptor) => descriptor,
        Err(message) => return ServiceResponse::error("invalid_preparation", message),
    };
    match bcode_agent_profile::prepare_tool_policy_with_operation(
        &preparation,
        &definition,
        true,
        shell_policy_identity(),
        shell_policy_operation(&preparation),
    ) {
        Ok(mut response) => {
            response.descriptor = match serde_json::to_value(descriptor) {
                Ok(descriptor) => descriptor,
                Err(error) => {
                    return ServiceResponse::error("invalid_preparation", error.to_string());
                }
            };
            json_response(&response)
        }
        Err(message) => ServiceResponse::error("invalid_preparation", message),
    }
}

fn shell_policy_identity() -> bcode_plugin_sdk::ToolPolicyIdentity {
    bcode_plugin_sdk::ToolPolicyIdentity {
        aliases: Vec::new(),
        compatibility_aliases: vec![bcode_tool::ToolCompatibilityAlias::new("claude", "Bash")],
        capabilities: vec!["shell.run".to_string(), "process.execute".to_string()],
        permission_category: Some("command".to_string()),
    }
}

fn shell_policy_operation(
    request: &bcode_tool::ToolPreparationRequest,
) -> bcode_plugin_sdk::ToolPolicyOperation {
    let command = request
        .invocation
        .arguments
        .get("command")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string);
    let (analysis, analysis_error) = command.as_ref().map_or_else(
        || {
            (
                None,
                Some(bcode_shell_command_analysis_models::ShellAnalysisError {
                    kind: bcode_shell_command_analysis_models::ShellAnalysisErrorKind::Parser,
                    message: "shell.run command is missing or is not a string".to_owned(),
                    dialect: bcode_shell_command_analysis_models::ShellDialect::Posix,
                    span: None,
                }),
            )
        },
        |command| match bcode_shell_command_analysis::analyze(
            &bcode_shell_command_analysis_models::ShellAnalysisRequest::posix(command),
        ) {
            Ok(analysis) => (Some(analysis), None),
            Err(error) => (None, Some(error)),
        },
    );
    bcode_plugin_sdk::ToolPolicyOperation::Command {
        command,
        analysis,
        analysis_error,
    }
}

fn shell_tool_definition() -> ToolDefinition {
    ToolDefinition {
                name: SHELL_RUN_TOOL_NAME.to_owned(),
                description: "Run a non-interactive shell command in pseudo-terminal mode. Commands must complete without user input: avoid editors, REPLs, watchers, pagers, and prompts; use non-interactive flags and disable paging (for example, `git --no-pager`). Interactive commands will time out.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["command"],
                    "properties": {
                        "command": { "type": "string" },
                        "cwd": { "type": "string" },
                        "timeout_ms": { "type": "integer", "minimum": 1 },
                        "columns": { "type": "integer", "minimum": 1 },
                        "rows": { "type": "integer", "minimum": 1 },
                        "format_commands": {
                            "type": "boolean",
                            "description": "Format the displayed shell command for readability. Defaults to shell output configuration."
                        }
                    }
                }),
            }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellWorkflowPreparationDescriptor {
    version: u32,
    block_id: String,
    input_sha256: String,
}

fn workflow_block_input_sha256(input: &serde_json::Value) -> Result<String, String> {
    bcode_workflow::workflow_block_input_sha256(input)
}

fn shell_workflow_command_plan(
    input: &serde_json::Value,
) -> Result<ShellWorkflowCommandPlan, String> {
    if input.is_string() || input.get("script").is_some() {
        let request = if let Some(script) = input.as_str() {
            contracts::ShellWorkflowScriptRequest {
                version: contracts::SHELL_SCRIPT_VERSION,
                script: script.to_string(),
                shell: None,
                cwd: std::path::PathBuf::from("."),
                environment: std::collections::BTreeMap::new(),
                timeout_ms: 300_000,
                accepted_exit_codes: vec![0],
                continue_on_unaccepted_exit: false,
                output: contracts::ShellWorkflowOutputPolicy {
                    preview_bytes: 8_192,
                    artifact_spill: true,
                },
            }
        } else {
            serde_json::from_value::<contracts::ShellWorkflowScriptRequest>(input.clone())
                .map_err(|error| error.to_string())?
        };
        shell_script_command_plan(request)
    } else {
        serde_json::from_value::<ShellWorkflowCommandPlan>(input.clone())
            .map_err(|error| error.to_string())
    }
}

fn posix_quote_workflow_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/'))
    {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

fn workflow_command_analysis(
    input: &serde_json::Value,
    plan: &ShellWorkflowCommandPlan,
) -> bcode_plugin_sdk::ToolPolicyOperation {
    let command = input
        .as_str()
        .or_else(|| input.get("script").and_then(serde_json::Value::as_str))
        .map_or_else(
            || {
                plan.commands
                    .iter()
                    .map(|command| {
                        command
                            .argv
                            .iter()
                            .map(|word| posix_quote_workflow_word(word))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .collect::<Vec<_>>()
                    .join(" ; ")
            },
            ToString::to_string,
        );
    let (analysis, analysis_error) = match bcode_shell_command_analysis::analyze(
        &bcode_shell_command_analysis_models::ShellAnalysisRequest::posix(command.clone()),
    ) {
        Ok(analysis) => (Some(analysis), None),
        Err(error) => (None, Some(error)),
    };
    bcode_plugin_sdk::ToolPolicyOperation::Command {
        command: Some(command),
        analysis,
        analysis_error,
    }
}

fn prepare_workflow_block_contract(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<bcode_workflow::WorkflowBlockPreparationRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_preparation", error.to_string()),
    };
    if let Err(error) = request.validate() {
        return ServiceResponse::error("invalid_preparation", error.to_string());
    }
    if request.block.plugin_id != "bcode.shell"
        || request.block.operation != "exec"
        || request.block.block_id != "exec"
        || request.block.block_version != 1
    {
        return ServiceResponse::error(
            "invalid_preparation",
            "unsupported or malformed shell workflow preparation request",
        );
    }
    let input_sha256 = match workflow_block_input_sha256(&request.input) {
        Ok(checksum) => checksum,
        Err(error) => return ServiceResponse::error("invalid_preparation", error),
    };
    let plan = match shell_workflow_command_plan(&request.input) {
        Ok(plan) => plan,
        Err(error) => return ServiceResponse::error("invalid_preparation", error),
    };
    let operation = workflow_command_analysis(&request.input, &plan);
    if matches!(
        &operation,
        bcode_plugin_sdk::ToolPolicyOperation::Command { analysis: None, .. }
            | bcode_plugin_sdk::ToolPolicyOperation::Command {
                analysis: Some(bcode_shell_command_analysis_models::ShellAnalysis {
                    completeness:
                        bcode_shell_command_analysis_models::ShellAnalysisCompleteness::Incomplete { .. },
                    ..
                }),
                ..
            }
    ) {
        return ServiceResponse::error(
            "command_analysis_failed",
            "shell workflow command analysis failed closed",
        );
    }
    let descriptor = ShellWorkflowPreparationDescriptor {
        version: 1,
        block_id: request.block.block_id,
        input_sha256: input_sha256.clone(),
    };
    let policy_identity = shell_policy_identity();
    match ServiceResponse::json(&bcode_workflow::WorkflowBlockPreparationResponse {
        version: bcode_workflow::WORKFLOW_BLOCK_PREPARATION_VERSION,
        input_sha256,
        owner_id: "bcode.shell".to_string(),
        operation_facts: serde_json::to_value(
            bcode_agent_profile::ToolPolicyAuthorizationMetadata {
                requires_permission: true,
                aliases: policy_identity.aliases,
                compatibility_aliases: policy_identity.compatibility_aliases,
                capabilities: policy_identity.capabilities,
                permission_category: policy_identity.permission_category,
                operation,
            },
        )
        .unwrap_or(serde_json::Value::Null),
        descriptor: serde_json::to_value(descriptor).unwrap_or(serde_json::Value::Null),
        diagnostics: Vec::new(),
    }) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("preparation_encoding_failed", error.to_string()),
    }
}

#[allow(clippy::too_many_lines)]
fn invoke_workflow_block_contract(context: &NativeServiceContext) -> ServiceResponse {
    if !matches!(context.request.operation.as_str(), "exec") {
        return ServiceResponse::error(
            "unsupported_operation",
            "unsupported shell workflow block operation",
        );
    }
    if context.cancellation.is_cancelled() {
        return ServiceResponse::error("cancelled", "shell command plan cancelled");
    }
    let invocation = match context
        .request
        .payload_json::<bcode_workflow::WorkflowBlockInvocation>()
    {
        Ok(invocation) => invocation,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let Some(preparation) = invocation.preparation.as_ref() else {
        return ServiceResponse::error(
            "invalid_preparation",
            "shell workflow invocation requires owner preparation",
        );
    };
    if let Err(error) = preparation.validate() {
        return ServiceResponse::error("invalid_preparation", error.to_string());
    }
    let descriptor = match serde_json::from_value::<ShellWorkflowPreparationDescriptor>(
        preparation.descriptor.clone(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) => return ServiceResponse::error("invalid_preparation", error.to_string()),
    };
    let input_sha256 = match workflow_block_input_sha256(&invocation.input) {
        Ok(checksum) => checksum,
        Err(error) => return ServiceResponse::error("invalid_preparation", error),
    };
    if preparation.owner_id != "bcode.shell"
        || serde_json::from_value::<bcode_agent_profile::ToolPolicyAuthorizationMetadata>(
            preparation.operation_facts.clone(),
        )
        .is_err()
        || descriptor.version != 1
        || descriptor.block_id != context.request.operation
        || descriptor.input_sha256 != input_sha256
        || preparation.input_sha256 != input_sha256
    {
        return ServiceResponse::error(
            "invalid_preparation",
            "shell workflow preparation descriptor does not match the invocation",
        );
    }
    let plan = match shell_workflow_command_plan(&invocation.input) {
        Ok(plan) => plan,
        Err(error) => return ServiceResponse::error("invalid_request", error),
    };
    if plan.version != contracts::SHELL_COMMAND_PLAN_VERSION
        || plan.commands.is_empty()
        || plan.commands.len() > 64
        || plan.cwd.is_absolute()
        || plan.cwd.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
        || plan.commands.iter().any(|command| {
            command.argv.is_empty()
                || command.argv.len() > 256
                || command.timeout_ms == 0
                || command.timeout_ms > 300_000
                || command.accepted_exit_codes.as_ref().is_some_and(|codes| {
                    codes.is_empty()
                        || codes.len() > 64
                        || codes
                            .iter()
                            .collect::<std::collections::BTreeSet<_>>()
                            .len()
                            != codes.len()
                })
        })
        || plan.environment.set.len() > 128
        || plan.output.preview_bytes > 65_536
    {
        return ServiceResponse::error(
            "invalid_request",
            "shell command plan exceeds version, path, command, timeout, environment, or output bounds",
        );
    }
    match execute_workflow_command_plan(context, &invocation, &plan) {
        Ok(result) => json_response(&result),
        Err(error) => ServiceResponse::error("command_plan_failed", error),
    }
}

fn shell_script_command_plan(
    request: contracts::ShellWorkflowScriptRequest,
) -> Result<ShellWorkflowCommandPlan, String> {
    if request.version != contracts::SHELL_SCRIPT_VERSION
        || request.script.is_empty()
        || request.script.len() > 65_536
        || request.timeout_ms == 0
        || request.timeout_ms > 300_000
        || request.accepted_exit_codes.is_empty()
        || request.accepted_exit_codes.len() > 64
    {
        return Err(
            "shell script request exceeds version, script, timeout, or exit-code bounds"
                .to_string(),
        );
    }
    let contracts::ShellWorkflowScriptRequest {
        version: _,
        script,
        shell,
        cwd,
        environment,
        timeout_ms,
        accepted_exit_codes,
        continue_on_unaccepted_exit,
        output,
    } = request;
    let mut shell = shell.unwrap_or_else(|| {
        if cfg!(windows) {
            vec!["cmd".to_string(), "/C".to_string()]
        } else {
            vec!["sh".to_string(), "-c".to_string()]
        }
    });
    if shell.is_empty() || shell.len() > 32 || shell.iter().any(|part| part.contains('\0')) {
        return Err("shell interpreter argv is invalid".to_string());
    }
    shell.push(script);
    Ok(ShellWorkflowCommandPlan {
        version: contracts::SHELL_COMMAND_PLAN_VERSION,
        cwd,
        commands: vec![contracts::ShellWorkflowCommand {
            argv: shell,
            timeout_ms,
            accepted_exit_codes: Some(accepted_exit_codes),
            continue_on_unaccepted_exit,
        }],
        environment: contracts::ShellWorkflowEnvironment {
            inherit: true,
            set: environment,
        },
        output,
    })
}

const SHELL_WORKFLOW_ARTIFACT_CONTENT_TYPE: &str = "application/octet-stream";
const SHELL_WORKFLOW_STDOUT_SCHEMA: &str = "bcode.shell.exec.stdout";
const SHELL_WORKFLOW_STDERR_SCHEMA: &str = "bcode.shell.exec.stderr";

fn normalized_command_plan(plan: &ShellWorkflowCommandPlan) -> ShellWorkflowCommandPlan {
    let mut normalized = plan.clone();
    if normalized.version == contracts::SHELL_COMMAND_PLAN_VERSION {
        for command in &mut normalized.commands {
            let mut codes = command
                .accepted_exit_codes
                .take()
                .unwrap_or_else(|| vec![0]);
            codes.sort_unstable();
            codes.dedup();
            command.accepted_exit_codes = Some(codes);
        }
    }
    normalized
}

fn canonical_command_plan_sha256(plan: &ShellWorkflowCommandPlan) -> Result<String, String> {
    let normalized = normalized_command_plan(plan);
    let normalized = serde_json::to_vec(&normalized)
        .map_err(|error| format!("failed to encode canonical shell command plan: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(normalized)))
}

fn execute_workflow_command_plan(
    context: &NativeServiceContext,
    invocation: &bcode_workflow::WorkflowBlockInvocation,
    plan: &ShellWorkflowCommandPlan,
) -> Result<ShellWorkflowCommandPlanResult, String> {
    let mut progress = context.transient_progress(
        invocation.dispatch_identity.clone(),
        "command-plan-progress",
        "bcode.shell.exec.progress",
        1,
    );
    let workspace = invocation
        .workspace_root
        .canonicalize()
        .map_err(|error| format!("workflow workspace is unavailable: {error}"))?;
    let cwd = workspace.join(&plan.cwd);
    let cwd = cwd
        .canonicalize()
        .map_err(|error| format!("workflow cwd is unavailable: {error}"))?;
    if !cwd.starts_with(&workspace) || !cwd.is_dir() {
        return Err("workflow cwd escapes the immutable workspace".to_string());
    }
    validate_workflow_environment(&plan.environment)?;
    let mut commands = Vec::with_capacity(plan.commands.len());
    let mut artifacts = Vec::new();
    for (index, command) in plan.commands.iter().enumerate() {
        let _ = progress.upsert_if_ready(&serde_json::json!({
            "state": "running",
            "command_index": index,
            "command_count": plan.commands.len(),
        }));
        if context.cancellation.is_cancelled() {
            commands.push(cancelled_workflow_command_result(
                index,
                plan.version,
                command_accepted_exit_codes(plan.version, command),
            ));
            break;
        }
        let (result, command_artifacts) =
            execute_workflow_command(context, invocation, plan, command, index, &cwd)?;
        let accepted_exit = result.exit_accepted;
        let continue_on_unaccepted = command.continue_on_unaccepted_exit;
        let should_stop = result.status != ShellWorkflowCommandStatus::Exited
            || !accepted_exit && !continue_on_unaccepted;
        commands.push(result);
        artifacts.extend(command_artifacts);
        if should_stop {
            break;
        }
    }
    let _ = progress.finish();
    let passed = commands.len() == plan.commands.len()
        && commands.iter().all(|result| {
            result.status == ShellWorkflowCommandStatus::Exited && result.exit_accepted
        });
    Ok(ShellWorkflowCommandPlanResult {
        version: plan.version,
        plan_sha256: canonical_command_plan_sha256(plan)?,
        passed,
        commands,
        artifacts,
    })
}

fn command_accepted_exit_codes(
    _plan_version: u32,
    command: &contracts::ShellWorkflowCommand,
) -> Vec<i32> {
    let mut codes = command
        .accepted_exit_codes
        .clone()
        .unwrap_or_else(|| vec![0]);
    codes.sort_unstable();
    codes.dedup();
    codes
}

fn workflow_environment_name_is_sensitive(name: &str) -> bool {
    let normalized = name.to_ascii_uppercase();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "API_KEY",
        "PRIVATE_KEY",
        "ACCESS_KEY",
        "AUTH",
        "CREDENTIAL",
        "COOKIE",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

fn validate_workflow_environment(
    environment: &contracts::ShellWorkflowEnvironment,
) -> Result<(), String> {
    if environment.set.iter().any(|(name, value)| {
        name.is_empty()
            || workflow_environment_name_is_sensitive(name)
            || name.contains(['=', '\0'])
            || value.contains('\0')
            || name.len() > 256
            || value.len() > 65_536
    }) {
        return Err("workflow command plan contains an invalid environment entry".to_string());
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn execute_workflow_command(
    context: &NativeServiceContext,
    invocation: &bcode_workflow::WorkflowBlockInvocation,
    plan: &ShellWorkflowCommandPlan,
    command: &contracts::ShellWorkflowCommand,
    index: usize,
    cwd: &Path,
) -> Result<
    (
        ShellWorkflowCommandResult,
        Vec<bcode_workflow::ArtifactReference>,
    ),
    String,
> {
    let accepted_exit_codes = command_accepted_exit_codes(plan.version, command);
    let started = Instant::now();
    let argv: Vec<String> = command
        .argv
        .iter()
        .map(|argument| serde_json::to_string(argument).expect("string argument serializes"))
        .collect();
    bcode_shell_command_analysis::analyze(
        &bcode_shell_command_analysis_models::ShellAnalysisRequest::posix(argv.join(" ")),
    )
    .map_err(|error| format!("workflow command analysis failed: {}", error.message))?;
    let runtime = bcode_tool_runtime::ToolExecutionRuntime::new(1);
    let request = bcode_tool_runtime::ProcessExecutionRequest {
        program: command.argv[0].clone(),
        args: command.argv[1..].to_vec(),
        cwd: Some(cwd.to_path_buf()),
        timeout: Some(Duration::from_millis(command.timeout_ms)),
        max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        inherit_environment: plan.environment.inherit,
        environment: plan.environment.set.clone(),
    };
    let cancellation = context.cancellation.clone();
    let cancellation_wait = Duration::from_millis(command.timeout_ms);
    let handle = runtime.cancellation_handle();
    let watcher_handle = handle.clone();
    let watcher_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher_done_thread = Arc::clone(&watcher_done);
    let watcher = std::thread::spawn(move || {
        let started = Instant::now();
        while !watcher_done_thread.load(std::sync::atomic::Ordering::SeqCst)
            && started.elapsed() < cancellation_wait
        {
            if cancellation.wait_cancelled(Duration::from_millis(10)) {
                watcher_handle.cancel();
                break;
            }
        }
    });
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?
        .block_on(runtime.run_process_cancellable(request, &handle));
    watcher_done.store(true, std::sync::atomic::Ordering::SeqCst);
    watcher
        .join()
        .map_err(|_| "workflow cancellation watcher panicked".to_string())?;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(bcode_tool_runtime::ToolRuntimeError::Io(error)) => {
            let error = error.to_string();
            let (stderr_preview, stderr_utf8, stderr_truncated) =
                bounded_preview(error.as_bytes(), plan.output.preview_bytes);
            return Ok((
                ShellWorkflowCommandResult {
                    index: u32::try_from(index).map_err(|error| error.to_string())?,
                    status: ShellWorkflowCommandStatus::SpawnFailed,
                    exit_code: None,
                    accepted_exit_codes,
                    exit_accepted: false,
                    signal: None,
                    duration_ms: elapsed_millis(started),
                    stdout_preview: String::new(),
                    stderr_preview,
                    stdout_bytes: 0,
                    stderr_bytes: u64::try_from(error.len()).unwrap_or(u64::MAX),
                    stdout_encoding: contracts::ShellWorkflowOutputEncoding::Utf8,
                    stderr_encoding: if stderr_utf8 {
                        contracts::ShellWorkflowOutputEncoding::Utf8
                    } else {
                        contracts::ShellWorkflowOutputEncoding::Binary
                    },
                    stdout_truncated: false,
                    stderr_truncated,
                },
                Vec::new(),
            ));
        }
        Err(error) => return Err(error.to_string()),
    };
    let (stdout_preview, stdout_utf8, stdout_truncated) =
        bounded_preview(&outcome.stdout.bytes, plan.output.preview_bytes);
    let (stderr_preview, stderr_utf8, stderr_truncated) =
        bounded_preview(&outcome.stderr.bytes, plan.output.preview_bytes);
    let stdout_bytes = u64::try_from(outcome.stdout.bytes.len()).unwrap_or(u64::MAX);
    let stderr_bytes = u64::try_from(outcome.stderr.bytes.len()).unwrap_or(u64::MAX);
    let mut artifacts = Vec::new();
    if plan.output.artifact_spill && stdout_truncated {
        artifacts.push(write_workflow_output_artifact(
            context,
            invocation,
            index,
            "stdout",
            SHELL_WORKFLOW_STDOUT_SCHEMA,
            outcome.stdout.bytes,
        )?);
    }
    if plan.output.artifact_spill && stderr_truncated {
        artifacts.push(write_workflow_output_artifact(
            context,
            invocation,
            index,
            "stderr",
            SHELL_WORKFLOW_STDERR_SCHEMA,
            outcome.stderr.bytes,
        )?);
    }
    let (status, exit_code, signal) = if outcome.cancelled {
        (ShellWorkflowCommandStatus::Cancelled, None, None)
    } else if outcome.timed_out {
        (ShellWorkflowCommandStatus::TimedOut, None, None)
    } else {
        match outcome.termination {
            bcode_tool_runtime::ProcessTermination::Exited { code } => {
                (ShellWorkflowCommandStatus::Exited, Some(code), None)
            }
            bcode_tool_runtime::ProcessTermination::Signaled { signal } => {
                (ShellWorkflowCommandStatus::Signaled, None, Some(signal))
            }
            bcode_tool_runtime::ProcessTermination::Unknown => {
                (ShellWorkflowCommandStatus::Signaled, None, None)
            }
        }
    };
    let exit_accepted = exit_code.is_some_and(|code| accepted_exit_codes.contains(&code));
    Ok((
        ShellWorkflowCommandResult {
            index: u32::try_from(index).map_err(|error| error.to_string())?,
            status,
            exit_code,
            accepted_exit_codes,
            exit_accepted,
            signal,
            duration_ms: outcome.duration_ms,
            stdout_preview,
            stderr_preview,
            stdout_bytes,
            stderr_bytes,
            stdout_encoding: if stdout_utf8 {
                contracts::ShellWorkflowOutputEncoding::Utf8
            } else {
                contracts::ShellWorkflowOutputEncoding::Binary
            },
            stderr_encoding: if stderr_utf8 {
                contracts::ShellWorkflowOutputEncoding::Utf8
            } else {
                contracts::ShellWorkflowOutputEncoding::Binary
            },
            stdout_truncated,
            stderr_truncated,
        },
        artifacts,
    ))
}

fn bounded_preview(bytes: &[u8], limit: u32) -> (String, bool, bool) {
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let retained = bytes.get(..bytes.len().min(limit)).unwrap_or(bytes);
    let utf8 = std::str::from_utf8(retained).is_ok();
    (
        String::from_utf8_lossy(retained).into_owned(),
        utf8,
        bytes.len() > retained.len(),
    )
}

fn write_workflow_output_artifact(
    context: &NativeServiceContext,
    invocation: &bcode_workflow::WorkflowBlockInvocation,
    index: usize,
    stream: &str,
    schema: &str,
    bytes: Vec<u8>,
) -> Result<bcode_workflow::ArtifactReference, String> {
    let artifact_id = format!("command-{index}-{stream}");
    let response = context
        .bridge
        .request(&ServiceBridgeRequest::WriteArtifact(
            bcode_tool::ToolArtifactWriteRequest {
                invocation_id: invocation.dispatch_identity.clone(),
                artifact_id: artifact_id.clone(),
                content_type: SHELL_WORKFLOW_ARTIFACT_CONTENT_TYPE.to_string(),
                bytes,
                metadata: serde_json::json!({"schema": schema, "schema_version": 1}),
            },
        ))
        .map_err(|error| error.to_string())?;
    match response {
        ServiceBridgeResponse::Artifact(bcode_tool::ToolArtifactWriteResolution::Written {
            reference,
            ..
        }) => Ok(bcode_workflow::ArtifactReference::new(
            artifact_id,
            schema,
            1,
            SHELL_WORKFLOW_ARTIFACT_CONTENT_TYPE,
            reference.to_string(),
        )),
        ServiceBridgeResponse::Artifact(resolution) => Err(format!(
            "workflow output artifact was not written: {resolution:?}"
        )),
        _ => Err("workflow output artifact returned an unexpected response".to_string()),
    }
}

fn cancelled_workflow_command_result(
    index: usize,
    _plan_version: u32,
    accepted_exit_codes: Vec<i32>,
) -> ShellWorkflowCommandResult {
    ShellWorkflowCommandResult {
        index: u32::try_from(index).unwrap_or(u32::MAX),
        status: ShellWorkflowCommandStatus::Cancelled,
        exit_code: None,
        accepted_exit_codes,
        exit_accepted: false,
        signal: None,
        duration_ms: 0,
        stdout_preview: String::new(),
        stderr_preview: String::new(),
        stdout_bytes: 0,
        stderr_bytes: 0,
        stdout_encoding: contracts::ShellWorkflowOutputEncoding::Utf8,
        stderr_encoding: contracts::ShellWorkflowOutputEncoding::Utf8,
        stdout_truncated: false,
        stderr_truncated: false,
    }
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn list_tools(request: &ServiceRequest) -> ServiceResponse {
    if let Err(error) = request.payload_json::<ListToolsRequest>() {
        return invalid_request(&error);
    }
    json_response(&ToolList {
        tools: vec![shell_tool_definition()],
    })
}

fn invoke_tool(context: &NativeServiceContext) -> ServiceResponse {
    let request = match context.request.payload_json::<ToolInvocationRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let descriptor = match serde_json::from_value::<ShellPreparationDescriptor>(
        request.preparation_descriptor.clone(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            return json_response(&ToolInvocationResponse {
                output: format!("invalid Shell preparation descriptor: {error}"),
                is_error: true,
                content: Vec::new(),
                full_output: None,
                result: None,
            });
        }
    };
    let mut arguments = request.arguments;
    if let Some(arguments) = arguments.as_object_mut() {
        arguments.insert(
            "timeout_ms".to_owned(),
            serde_json::Value::from(descriptor.timeout_ms),
        );
        if arguments.get("cwd").is_none()
            && let Some(workspace_root) = descriptor.workspace_root.as_deref()
        {
            arguments.insert(
                "cwd".to_owned(),
                serde_json::Value::String(workspace_root.display().to_string()),
            );
        }
    }
    let primary_presentation = Arc::new(StdMutex::new(
        PrimaryPresentationPublisher::with_limits_and_cancellation(
            context.events,
            &request.tool_call_id,
            "bcode.shell",
            "bcode.tool.request.shell.run",
            SHELL_SCHEMA_VERSION,
            bcode_tool::ToolPresentationRetention::RetainLatest,
            context.transient_progress_limits,
            context.cancellation.clone(),
        ),
    ));
    if let Ok(mut presentation) = primary_presentation.lock() {
        let _ = presentation.replace(&arguments);
    }
    let response = match request.name.as_str() {
        "shell.run" => run_shell_tool(
            context,
            context.events,
            &request.tool_call_id,
            arguments,
            descriptor.workspace_root.as_deref(),
            TerminalRunPaths {
                session_cwd: descriptor.workspace_root.as_deref(),
                artifact_dir: descriptor.artifact_root.as_deref(),
                input_bridge: Some(&context.bridge),
                primary_presentation: Some(Arc::clone(&primary_presentation)),
            },
        ),
        _ => ToolInvocationResponse {
            output: format!("unknown shell tool: {}", request.name),
            is_error: true,
            content: Vec::new(),
            full_output: None,
            result: None,
        },
    };
    json_response(&response)
}

fn run_shell_tool(
    context: &NativeServiceContext,
    events: ServiceEventEmitter,
    tool_call_id: &str,
    arguments: serde_json::Value,
    session_cwd: Option<&std::path::Path>,
    paths: TerminalRunPaths<'_>,
) -> ToolInvocationResponse {
    let arguments = match serde_json::from_value::<ShellRunArguments>(arguments) {
        Ok(arguments) => arguments,
        Err(error) => {
            return ToolInvocationResponse {
                output: error.to_string(),
                is_error: true,
                content: Vec::new(),
                full_output: None,
                result: None,
            };
        }
    };
    if arguments.command.trim().is_empty() {
        return ToolInvocationResponse {
            output: "command must not be empty".to_string(),
            is_error: true,
            content: Vec::new(),
            full_output: None,
            result: None,
        };
    }
    let arguments_json = serde_json::to_value(&arguments).unwrap_or_else(|_| json!({}));
    emit_tool_lifecycle(
        events,
        &ToolInvocationLifecycleEvent {
            invocation_id: tool_call_id.to_owned(),
            sequence: 1,
            stage: ToolInvocationLifecycleStage::Progress,
            message: Some(format!("starting command: {}", arguments.command)),
            metadata: serde_json::Value::Null,
        },
    );
    run_terminal_shell_command(
        events,
        &context.cancellation,
        context.transient_progress_limits,
        tool_call_id,
        &arguments,
        arguments_json,
        TerminalRunPaths {
            session_cwd,
            ..paths
        },
    )
}

#[derive(Debug, Serialize)]
struct TerminalCommandOutput {
    mode: &'static str,
    exit_code: Option<i32>,
    timed_out: bool,
    cancelled: bool,
    command: String,
    cwd: Option<String>,
    output: String,
    output_truncated: bool,
    output_bytes: u64,
    retained_output_bytes: u64,
    columns: u16,
    rows: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LimitedOutput {
    text: String,
    original_bytes: usize,
    retained_bytes: usize,
    truncated: bool,
}

struct TerminalStreamOutput {
    raw: LimitedOutput,
    replay: LimitedOutput,
    clean: LimitedOutput,
    raw_artifact_path: Option<PathBuf>,
    replay_artifact_path: Option<PathBuf>,
    clean_artifact_path: Option<PathBuf>,
    recording_path: Option<PathBuf>,
    recording_writer: Option<recording::AsyncShellRecordingWriter>,
    prelude_suppressed: bool,
}

fn resolve_effective_cwd(
    arguments: &ShellRunArguments,
    session_cwd: Option<&Path>,
) -> Option<PathBuf> {
    arguments.cwd.as_deref().map_or_else(
        || session_cwd.map(Path::to_path_buf),
        |cwd| {
            if cwd.is_absolute() {
                Some(cwd.to_path_buf())
            } else {
                session_cwd
                    .map(|base| base.join(cwd))
                    .or_else(|| Some(cwd.to_path_buf()))
            }
        },
    )
}

fn shell_config_with_environment(
    cwd: Option<&Path>,
    environment: &impl bcode_config::ConfigEnvironment,
) -> Result<ShellToolConfig, String> {
    let paths = cwd.map_or_else(
        || bcode_config::default_config_paths_with_environment(environment),
        |cwd| default_config_paths_from_with_environment(cwd, environment),
    );
    load_config_from_paths_with_environment(&paths, environment)
        .map(|config| config.tools.shell)
        .map_err(|error| error.to_string())
}

fn direnv_file_for(cwd: &Path) -> Option<PathBuf> {
    let mut current = cwd.to_path_buf();
    loop {
        let envrc = current.join(".envrc");
        if envrc.exists() {
            return Some(envrc);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn direnv_available() -> bool {
    Command::new("direnv")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn should_use_direnv(cwd: Option<&Path>, config: ShellToolEnvConfig) -> Result<bool, String> {
    match config.mode {
        ShellToolEnvMode::Inherit => Ok(false),
        ShellToolEnvMode::Direnv => {
            if direnv_available() {
                Ok(true)
            } else {
                Err("shell env mode is direnv, but `direnv` is not available on PATH".to_owned())
            }
        }
        ShellToolEnvMode::Auto => {
            let Some(cwd) = cwd else {
                return Ok(false);
            };
            let Some(envrc) = direnv_file_for(cwd) else {
                return Ok(false);
            };
            if direnv_available() {
                Ok(true)
            } else if config.auto_fallback == ShellToolEnvAutoFallback::Inherit {
                Ok(false)
            } else {
                Err(format!(
                    "found {}, but `direnv` is not available on PATH; install direnv or set `[tools.shell.env] auto_fallback = \"inherit\"`",
                    display(&envrc, cwd)
                ))
            }
        }
    }
}

struct ShellCommandPlan {
    program: String,
    args: Vec<String>,
    prelude_marker: Option<String>,
}

fn shell_format_commands(
    arguments: &ShellRunArguments,
    output_config: &ShellToolOutputConfig,
    arguments_json: &mut serde_json::Value,
) -> bool {
    let format_commands = arguments
        .format_commands
        .unwrap_or(output_config.format_commands);
    if let Some(arguments) = arguments_json.as_object_mut() {
        arguments.insert("format_commands".to_owned(), json!(format_commands));
    }
    format_commands
}

fn prelude_marker(tool_call_id: &str) -> String {
    let safe_id = tool_call_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("__BCODE_DIRENV_READY_{safe_id}__")
}

fn direnv_wrapped_command(command: &str, marker: &str) -> String {
    format!("printf '%s\\n' '{marker}'\n{command}")
}

fn direnv_shell_command_plan(
    command: &str,
    cwd: &Path,
    env_config: ShellToolEnvConfig,
    tool_call_id: &str,
) -> ShellCommandPlan {
    let marker = env_config
        .hide_direnv_prelude
        .then(|| prelude_marker(tool_call_id));
    let command = marker.as_deref().map_or_else(
        || command.to_owned(),
        |marker| direnv_wrapped_command(command, marker),
    );
    ShellCommandPlan {
        program: "direnv".to_owned(),
        args: vec![
            "exec".to_owned(),
            cwd.display().to_string(),
            shell_program().to_owned(),
            "-o".to_owned(),
            "pipefail".to_owned(),
            "-c".to_owned(),
            command,
        ],
        prelude_marker: marker,
    }
}

fn shell_program_and_args(
    command: &str,
    cwd: Option<&Path>,
    env_config: ShellToolEnvConfig,
    tool_call_id: &str,
) -> Result<ShellCommandPlan, String> {
    if should_use_direnv(cwd, env_config)? {
        let cwd = cwd.ok_or_else(|| "direnv shell mode requires a working directory".to_owned())?;
        Ok(direnv_shell_command_plan(
            command,
            cwd,
            env_config,
            tool_call_id,
        ))
    } else {
        Ok(ShellCommandPlan {
            program: shell_program().to_owned(),
            args: shell_args(command),
            prelude_marker: None,
        })
    }
}

#[derive(Debug, Clone)]
struct TerminalRunPaths<'a> {
    session_cwd: Option<&'a Path>,
    artifact_dir: Option<&'a Path>,
    input_bridge: Option<&'a ServiceBridge>,
    primary_presentation: Option<Arc<StdMutex<PrimaryPresentationPublisher>>>,
}

fn run_terminal_shell_command(
    events: ServiceEventEmitter,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
    progress_limits: bcode_plugin_sdk::TransientProgressLimits,
    tool_call_id: &str,
    arguments: &ShellRunArguments,
    arguments_json: serde_json::Value,
    paths: TerminalRunPaths<'_>,
) -> ToolInvocationResponse {
    run_terminal_shell_command_with_environment(
        events,
        cancellation,
        progress_limits,
        tool_call_id,
        arguments,
        arguments_json,
        paths,
        &bcode_config::ProcessConfigEnvironment,
    )
}

#[allow(clippy::too_many_arguments)] // Testable environment boundary keeps execution inputs explicit.
fn run_terminal_shell_command_with_environment(
    events: ServiceEventEmitter,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
    progress_limits: bcode_plugin_sdk::TransientProgressLimits,
    tool_call_id: &str,
    arguments: &ShellRunArguments,
    arguments_json: serde_json::Value,
    paths: TerminalRunPaths<'_>,
    environment: &impl bcode_config::ConfigEnvironment,
) -> ToolInvocationResponse {
    match run_terminal_shell_command_inner(
        events,
        cancellation,
        progress_limits,
        tool_call_id,
        arguments,
        arguments_json,
        paths,
        environment,
    ) {
        Ok(response) => response,
        Err(error) => ToolInvocationResponse {
            output: error,
            is_error: true,
            content: Vec::new(),
            full_output: None,
            result: None,
        },
    }
}

#[derive(Debug, Clone, Copy)]
struct ShellAppliedResize {
    columns: u16,
    rows: u16,
}

struct ShellInvocationActionReader {
    bridge: ServiceBridge,
    invocation_id: String,
    started: Instant,
    recording: Option<recording::AsyncShellRecordingResizeSender>,
    applied_resizes: Arc<StdMutex<Vec<ShellAppliedResize>>>,
}

impl ShellInvocationActionReader {
    fn poll(&self, master: &dyn portable_pty::MasterPty) -> Result<(), String> {
        loop {
            let response = self
                .bridge
                .request(&ServiceBridgeRequest::ReceiveInput {
                    invocation_id: self.invocation_id.clone(),
                    timeout_ms: Some(1),
                })
                .map_err(|error| format!("shell input routing failed: {error}"))?;
            let ServiceBridgeResponse::Input(resolution) = response else {
                return Err("shell input request returned unexpected bridge response".to_string());
            };
            let input = match resolution {
                bcode_tool::ToolInvocationInputResolution::Received { input } => input,
                bcode_tool::ToolInvocationInputResolution::TimedOut
                | bcode_tool::ToolInvocationInputResolution::Closed => break,
                bcode_tool::ToolInvocationInputResolution::Cancelled => {
                    return Err("shell input routing cancelled".to_string());
                }
                bcode_tool::ToolInvocationInputResolution::Failed { code, message } => {
                    return Err(format!("shell input routing failed ({code}): {message}"));
                }
            };
            if input.producer_id != "bcode.shell"
                || input.schema != SHELL_INVOCATION_INPUT_SCHEMA
                || input.schema_version != 1
            {
                return Err("unsupported shell invocation input schema".to_owned());
            }
            let event = serde_json::from_value::<ShellInvocationAction>(input.payload)
                .map_err(|error| format!("invalid shell invocation input: {error}"))?;
            match event {
                ShellInvocationAction::Resize { columns, rows } => {
                    if columns == 0 || rows == 0 {
                        return Err("terminal resize dimensions must be positive".to_owned());
                    }
                    let size = portable_pty::PtySize {
                        rows,
                        cols: columns,
                        pixel_width: 0,
                        pixel_height: 0,
                    };
                    if let Some(recording) = &self.recording {
                        recording
                            .write_resize_with(
                                u64::try_from(self.started.elapsed().as_micros())
                                    .unwrap_or(u64::MAX),
                                columns,
                                rows,
                                || {
                                    master
                                        .resize(size)
                                        .map_err(|error| io::Error::other(error.to_string()))?;
                                    Ok(())
                                },
                            )
                            .map_err(|error| error.to_string())?;
                    } else {
                        master.resize(size).map_err(|error| error.to_string())?;
                    }
                    self.applied_resizes
                        .lock()
                        .map_err(|_| "shell applied resize state poisoned".to_owned())?
                        .push(ShellAppliedResize { columns, rows });
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct TerminalShellStatus {
    exit_code: i32,
    signal: Option<String>,
    success: bool,
    timed_out: bool,
    cancelled: bool,
}

#[allow(clippy::too_many_arguments)]
fn wait_for_terminal_shell_status(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
    timeout: Duration,
    tool_call_id: &str,
    events: ServiceEventEmitter,
    control: Option<&ShellInvocationActionReader>,
    master: Option<&dyn portable_pty::MasterPty>,
) -> Result<TerminalShellStatus, String> {
    let started = Instant::now();
    let mut timed_out = false;
    let mut cancelled = false;
    let status = loop {
        if let (Some(control), Some(master)) = (control, master) {
            control.poll(master)?;
        }
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            break status;
        }
        if cancellation.is_cancelled() {
            cancelled = true;
            emit_tool_lifecycle(
                events,
                &ToolInvocationLifecycleEvent {
                    invocation_id: tool_call_id.to_owned(),
                    sequence: 2,
                    stage: ToolInvocationLifecycleStage::Progress,
                    message: Some("cancellation requested; killing terminal process".to_owned()),
                    metadata: serde_json::Value::Null,
                },
            );
            child.kill().map_err(|error| error.to_string())?;
            break child.wait().map_err(|error| error.to_string())?;
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            emit_tool_lifecycle(
                events,
                &ToolInvocationLifecycleEvent {
                    invocation_id: tool_call_id.to_owned(),
                    sequence: 2,
                    stage: ToolInvocationLifecycleStage::Progress,
                    message: Some("timeout reached; killing terminal process".to_owned()),
                    metadata: serde_json::Value::Null,
                },
            );
            child.kill().map_err(|error| error.to_string())?;
            break child.wait().map_err(|error| error.to_string())?;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ok(TerminalShellStatus {
        exit_code: i32::try_from(status.exit_code()).unwrap_or(i32::MAX),
        signal: status.signal().map(ToOwned::to_owned),
        success: status.success(),
        timed_out,
        cancelled,
    })
}

fn encode_terminal_output(
    command: &str,
    cwd: Option<&Path>,
    status: &TerminalShellStatus,
    output: &LimitedOutput,
    columns: u16,
    rows: u16,
) -> Result<(String, String, LimitedOutput), String> {
    let inline_output = limit_terminal_inline_output(output);
    let terminal_output = TerminalCommandOutput {
        mode: "terminal",
        exit_code: Some(status.exit_code),
        timed_out: status.timed_out,
        cancelled: status.cancelled,
        command: command.to_owned(),
        cwd: cwd.map(|cwd| cwd.display().to_string()),
        output: inline_output.text.clone(),
        output_truncated: inline_output.truncated,
        output_bytes: u64::try_from(inline_output.original_bytes).unwrap_or(u64::MAX),
        retained_output_bytes: u64::try_from(inline_output.retained_bytes).unwrap_or(u64::MAX),
        columns,
        rows,
    };
    let full_terminal_output = TerminalCommandOutput {
        mode: "terminal",
        exit_code: Some(status.exit_code),
        timed_out: status.timed_out,
        cancelled: status.cancelled,
        command: command.to_owned(),
        cwd: cwd.map(|cwd| cwd.display().to_string()),
        output: output.text.clone(),
        output_truncated: output.truncated,
        output_bytes: u64::try_from(output.original_bytes).unwrap_or(u64::MAX),
        retained_output_bytes: u64::try_from(output.retained_bytes).unwrap_or(u64::MAX),
        columns,
        rows,
    };
    let encoded = serde_json::to_string(&terminal_output).map_err(|error| error.to_string())?;
    let full_encoded =
        serde_json::to_string(&full_terminal_output).map_err(|error| error.to_string())?;
    Ok((encoded, full_encoded, inline_output))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_terminal_shell_command_inner(
    events: ServiceEventEmitter,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
    progress_limits: bcode_plugin_sdk::TransientProgressLimits,
    tool_call_id: &str,
    arguments: &ShellRunArguments,
    mut arguments_json: serde_json::Value,
    paths: TerminalRunPaths<'_>,
    environment: &impl bcode_config::ConfigEnvironment,
) -> Result<ToolInvocationResponse, String> {
    let timeout = Duration::from_millis(arguments.timeout_ms.unwrap_or(DEFAULT_SHELL_TIMEOUT_MS));
    let cwd = resolve_effective_cwd(arguments, paths.session_cwd);
    let shell_config = shell_config_with_environment(cwd.as_deref(), environment)?;
    let format_commands =
        shell_format_commands(arguments, &shell_config.output, &mut arguments_json);
    let env_config = shell_config.env;
    let columns = arguments.terminal_columns(DEFAULT_TERMINAL_COLUMNS);
    let rows = arguments.terminal_rows(DEFAULT_TERMINAL_ROWS);
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize {
            rows,
            cols: columns,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| error.to_string())?;

    let command_plan =
        shell_program_and_args(&arguments.command, cwd.as_deref(), env_config, tool_call_id)?;
    let ShellCommandPlan {
        program,
        args,
        prelude_marker,
    } = command_plan;
    let mut prelude_markers = prelude_markers_from_output_config(&shell_config.output);
    if let Some(prelude_marker) = prelude_marker {
        prelude_markers.live.push(prelude_marker.clone());
        prelude_markers.replay.push(prelude_marker.clone());
        prelude_markers.clean.push(prelude_marker);
    }
    let mut command = portable_pty::CommandBuilder::new(program);
    for arg in args {
        command.arg(arg);
    }
    if let Some(cwd) = cwd.as_deref() {
        command.cwd(cwd);
    }
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");

    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| error.to_string())?;
    drop(pair.slave);
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| error.to_string())?;
    let clean_artifact_path = clean_artifact_path(paths.artifact_dir, tool_call_id)?;
    let raw_artifact_path = raw_artifact_path(paths.artifact_dir, tool_call_id)?;
    let replay_artifact_path = replay_artifact_path(paths.artifact_dir, tool_call_id)?;
    let recording_path = recording_artifact_path(paths.artifact_dir, tool_call_id)?;
    let (recording_ready_tx, recording_ready_rx) = std::sync::mpsc::channel();
    let started = Instant::now();
    let cancellation_for_reader = cancellation.clone();
    let reader_thread = std::thread::spawn({
        let tool_call_id = tool_call_id.to_owned();
        move || {
            read_limited_streaming(
                &mut reader,
                events,
                &tool_call_id,
                &ShellVisualStreamContext {
                    columns,
                    rows,
                    timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                    arguments: arguments_json,
                    primary_presentation: paths.primary_presentation,
                    prelude_markers,
                    progress_limits,
                    cancellation: cancellation_for_reader,
                },
                TerminalStreamPaths {
                    clean: clean_artifact_path,
                    raw: raw_artifact_path,
                    replay: replay_artifact_path,
                    recording: recording_path,
                    recording_ready: Some(recording_ready_tx),
                },
            )
        }
    });

    let recording = recording_ready_rx
        .recv()
        .map_err(|_| "recording reader did not initialize".to_owned())?;
    let applied_resizes = Arc::new(StdMutex::new(Vec::new()));
    let control = paths
        .input_bridge
        .map(|bridge| ShellInvocationActionReader {
            bridge: bridge.clone(),
            invocation_id: tool_call_id.to_owned(),
            started,
            recording,
            applied_resizes: Arc::clone(&applied_resizes),
        });
    let status = wait_for_terminal_shell_status(
        &mut child,
        cancellation,
        timeout,
        tool_call_id,
        events,
        control.as_ref(),
        Some(&*pair.master),
    )?;
    drop(pair.master);
    let mut stream_output = join_reader(reader_thread)?;
    let recording_ref = finalize_recording(&mut stream_output, started, &status, columns, rows)?;
    let (final_columns, final_rows) = applied_resizes
        .lock()
        .map_err(|_| "shell applied resize state poisoned".to_owned())?
        .last()
        .map_or((columns, rows), |resize| (resize.columns, resize.rows));
    terminal_shell_response(
        tool_call_id,
        TerminalShellResponseInput {
            arguments,
            cwd: cwd.as_deref(),
            status,
            started,
            stream_output: &stream_output,
            recording_ref,
            columns: final_columns,
            rows: final_rows,
            format_commands,
        },
    )
}

#[derive(Clone)]
struct TerminalShellResponseInput<'a> {
    arguments: &'a ShellRunArguments,
    cwd: Option<&'a Path>,
    status: TerminalShellStatus,
    started: Instant,
    stream_output: &'a TerminalStreamOutput,
    recording_ref: Option<ToolArtifactRef>,
    columns: u16,
    rows: u16,
    format_commands: bool,
}

fn terminal_shell_response(
    tool_call_id: &str,
    input: TerminalShellResponseInput<'_>,
) -> Result<ToolInvocationResponse, String> {
    let (encoded, full_encoded, _clean_inline_output) = encode_terminal_output(
        &input.arguments.command,
        input.cwd,
        &input.status,
        &input.stream_output.clean,
        input.columns,
        input.rows,
    )?;
    let raw_inline_output = limit_terminal_inline_output(&input.stream_output.raw);
    let replay_inline_output = limit_terminal_inline_output(&input.stream_output.replay);
    let artifact_inline_output = if input.stream_output.prelude_suppressed {
        &replay_inline_output
    } else {
        &raw_inline_output
    };
    let replay_output = if input.stream_output.prelude_suppressed {
        &input.stream_output.replay
    } else {
        &input.stream_output.raw
    };
    let replay_path = if input.stream_output.prelude_suppressed {
        input.stream_output.replay_artifact_path.as_deref()
    } else {
        input.stream_output.raw_artifact_path.as_deref()
    };
    let replay_ref = input.recording_ref.or_else(|| {
        replay_path.map(|path| raw_artifact_ref(path, replay_output, input.columns, input.rows))
    });
    Ok(ToolInvocationResponse {
        output: encoded,
        is_error: input.status.timed_out || input.status.cancelled || !input.status.success,
        content: Vec::new(),
        full_output: Some(full_encoded),
        result: Some(shell_run_artifact(
            tool_call_id,
            &ShellRunResult::Terminal {
                exit_code: Some(input.status.exit_code),
                timed_out: input.status.timed_out,
                cancelled: input.status.cancelled,
                duration_ms: Some(
                    u64::try_from(input.started.elapsed().as_millis()).unwrap_or(u64::MAX),
                ),
                output_tail: artifact_inline_output.text.clone(),
                output_truncated: artifact_inline_output.truncated,
                output_bytes: Some(
                    u64::try_from(artifact_inline_output.original_bytes).unwrap_or(u64::MAX),
                ),
                retained_output_bytes: Some(
                    u64::try_from(artifact_inline_output.retained_bytes).unwrap_or(u64::MAX),
                ),
                columns: input.columns,
                rows: input.rows,
                format_commands: input.format_commands,
            },
            input
                .stream_output
                .clean_artifact_path
                .as_deref()
                .map(|path| clean_artifact_ref(path, &input.stream_output.clean)),
            replay_ref,
        )),
    })
}

fn limit_terminal_inline_output(output: &LimitedOutput) -> LimitedOutput {
    let bytes = output.text.as_bytes();
    let limit = MAX_INLINE_TERMINAL_OUTPUT_BYTES.min(bytes.len());
    let start = bytes.len().saturating_sub(limit);
    let start = utf8_boundary_at_or_after(&output.text, start);
    let text = output.text[start..].to_owned();
    LimitedOutput {
        text,
        original_bytes: output.original_bytes,
        retained_bytes: bytes.len().saturating_sub(start),
        truncated: output.truncated || start > 0,
    }
}

const fn utf8_boundary_at_or_after(value: &str, mut index: usize) -> usize {
    while index < value.len() && !value.is_char_boundary(index) {
        index = index.saturating_add(1);
    }
    index
}

#[cfg(unix)]
const fn shell_program() -> &'static str {
    "sh"
}

#[cfg(windows)]
const fn shell_program() -> &'static str {
    "cmd"
}

#[cfg(unix)]
fn shell_args(command: &str) -> Vec<String> {
    vec![
        "-o".to_string(),
        "pipefail".to_string(),
        "-c".to_string(),
        command.to_string(),
    ]
}

#[cfg(windows)]
fn shell_args(command: &str) -> Vec<String> {
    vec!["/C".to_string(), command.to_string()]
}

struct TerminalStreamPaths {
    clean: Option<PathBuf>,
    raw: Option<PathBuf>,
    replay: Option<PathBuf>,
    recording: Option<PathBuf>,
    recording_ready:
        Option<std::sync::mpsc::Sender<Option<recording::AsyncShellRecordingResizeSender>>>,
}

#[derive(Clone, Default)]
struct PreludeGateMarkers {
    live: Vec<String>,
    replay: Vec<String>,
    clean: Vec<String>,
}

#[derive(Clone)]
struct ShellVisualStreamContext {
    columns: u16,
    rows: u16,
    timeout_ms: u64,
    arguments: serde_json::Value,
    primary_presentation: Option<Arc<StdMutex<PrimaryPresentationPublisher>>>,
    prelude_markers: PreludeGateMarkers,
    progress_limits: bcode_plugin_sdk::TransientProgressLimits,
    cancellation: bcode_plugin_sdk::ServiceCancellation,
}

const PRELUDE_GATE_BUFFER_LIMIT: usize = 4 * 1024 * 1024;
const STREAM_READ_BUFFER_BYTES: usize = 16 * 1024;

struct PreludeGate {
    markers: Vec<Vec<u8>>,
    buffer: Vec<u8>,
    passed: bool,
    failed_open: bool,
}

impl PreludeGate {
    fn new(markers: Vec<String>) -> Self {
        let markers = markers
            .into_iter()
            .filter(|marker| !marker.is_empty())
            .map(String::into_bytes)
            .collect::<Vec<_>>();
        let passed = markers.is_empty();
        Self {
            markers,
            buffer: Vec::new(),
            passed,
            failed_open: false,
        }
    }

    fn write(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.markers.is_empty() {
            return chunk.to_vec();
        }
        if self.passed || self.failed_open {
            return chunk.to_vec();
        }
        self.buffer.extend_from_slice(chunk);
        if let Some((index, marker_len)) = find_first_marker(&self.buffer, &self.markers) {
            let mut start = index.saturating_add(marker_len);
            if self.buffer.get(start) == Some(&b'\r') {
                start = start.saturating_add(1);
            }
            if self.buffer.get(start) == Some(&b'\n') {
                start = start.saturating_add(1);
            }
            let output = self.buffer[start..].to_vec();
            self.buffer.clear();
            self.passed = true;
            return output;
        }
        if self.buffer.len() > PRELUDE_GATE_BUFFER_LIMIT {
            self.failed_open = true;
            return std::mem::take(&mut self.buffer);
        }
        Vec::new()
    }

    fn finish(&mut self) -> Vec<u8> {
        if self.passed || self.failed_open {
            Vec::new()
        } else {
            self.failed_open = true;
            std::mem::take(&mut self.buffer)
        }
    }

    const fn suppressed_prelude(&self) -> bool {
        !self.markers.is_empty() && self.passed && !self.failed_open
    }
}

fn find_first_marker(haystack: &[u8], markers: &[Vec<u8>]) -> Option<(usize, usize)> {
    markers
        .iter()
        .filter_map(|marker| find_bytes(haystack, marker).map(|index| (index, marker.len())))
        .min_by_key(|(index, _len)| *index)
}

fn prelude_markers_from_output_config(config: &ShellToolOutputConfig) -> PreludeGateMarkers {
    let mut markers = PreludeGateMarkers::default();
    for gate in config
        .prelude_gates
        .iter()
        .filter(|gate| gate.enabled && !gate.marker.is_empty())
    {
        if gate.hide_from.contains(&ShellToolPreludeGateTarget::Live) {
            markers.live.push(gate.marker.clone());
        }
        if gate.hide_from.contains(&ShellToolPreludeGateTarget::Replay) {
            markers.replay.push(gate.marker.clone());
        }
        if gate.hide_from.contains(&ShellToolPreludeGateTarget::Clean) {
            markers.clean.push(gate.marker.clone());
        }
    }
    markers
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

struct RetainedStream {
    bytes: Vec<u8>,
    original_bytes: usize,
    truncated: bool,
}

impl RetainedStream {
    const fn new() -> Self {
        Self {
            bytes: Vec::new(),
            original_bytes: 0,
            truncated: false,
        }
    }

    fn write_chunk(
        &mut self,
        writer: &mut dyn Write,
        chunk: &[u8],
        max_bytes: usize,
    ) -> Result<(), String> {
        self.original_bytes = self.original_bytes.saturating_add(chunk.len());
        let remaining = max_bytes.saturating_sub(self.bytes.len());
        if remaining == 0 {
            self.truncated = true;
            return Ok(());
        }
        let retained = chunk.len().min(remaining);
        writer
            .write_all(&chunk[..retained])
            .map_err(|error| error.to_string())?;
        self.bytes.extend_from_slice(&chunk[..retained]);
        self.truncated = self.truncated || retained < chunk.len();
        Ok(())
    }

    fn limited_output(&self, max_bytes: usize) -> LimitedOutput {
        limit_output_bytes_with_original(
            &self.bytes,
            self.original_bytes,
            max_bytes,
            self.truncated,
        )
    }
}

#[allow(clippy::too_many_lines)]
fn read_limited_streaming<R>(
    mut reader: R,
    events: ServiceEventEmitter,
    tool_call_id: &str,
    visual_context: &ShellVisualStreamContext,
    paths: TerminalStreamPaths,
) -> Result<TerminalStreamOutput, String>
where
    R: Read,
{
    let mut raw = RetainedStream::new();
    let mut replay = RetainedStream::new();
    let mut raw_writer = raw_artifact_writer(paths.raw.as_deref())?;
    let mut replay_writer = raw_artifact_writer(paths.replay.as_deref())?;
    let mut clean_writer = clean_artifact_writer(paths.clean.as_deref())?;
    let mut recording_writer = paths
        .recording
        .as_deref()
        .map(|path| {
            recording::AsyncShellRecordingWriter::create_with_observer(
                path,
                visual_context.columns,
                visual_context.rows,
                Some(shell_recording_commit_observer(
                    visual_context.primary_presentation.clone(),
                    events,
                    tool_call_id,
                    visual_context.timeout_ms,
                    visual_context.arguments.clone(),
                    visual_context.progress_limits,
                    visual_context.cancellation.clone(),
                )),
            )
        })
        .transpose()
        .map_err(|error| error.to_string())?;
    let recording_resize_sender = recording_writer
        .as_ref()
        .map(recording::AsyncShellRecordingWriter::resize_sender);
    if let Some(ready) = paths.recording_ready.as_ref() {
        let _ = ready.send(recording_resize_sender);
    }
    let mut cleaner = terminal_clean::TerminalCleanWriter::new(
        &mut clean_writer,
        visual_context.columns,
        visual_context.rows,
        MAX_INLINE_TERMINAL_OUTPUT_BYTES,
    );
    let mut buffer = [0_u8; STREAM_READ_BUFFER_BYTES];
    let recording_started = Instant::now();
    let mut live_gate = PreludeGate::new(visual_context.prelude_markers.live.clone());
    let mut replay_gate = PreludeGate::new(visual_context.prelude_markers.replay.clone());
    let mut clean_gate = PreludeGate::new(visual_context.prelude_markers.clean.clone());
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        raw.write_chunk(&mut *raw_writer, chunk, DEFAULT_MAX_OUTPUT_BYTES)?;
        let live = live_gate.write(chunk);
        let replay_chunk = replay_gate.write(chunk);
        let clean = clean_gate.write(chunk);
        if let Some(writer) = &mut recording_writer {
            writer
                .write_output_with(
                    u64::try_from(recording_started.elapsed().as_micros()).unwrap_or(u64::MAX),
                    chunk,
                    Some(&live),
                    || {},
                )
                .map_err(|error| error.to_string())?;
        }
        write_stream_outputs(
            StreamOutputs {
                replay: &replay_chunk,
                clean: &clean,
            },
            &mut replay,
            &mut *replay_writer,
            &mut cleaner,
        )?;
    }
    let live = live_gate.finish();
    let replay_chunk = replay_gate.finish();
    let clean = clean_gate.finish();
    write_stream_outputs(
        StreamOutputs {
            replay: &replay_chunk,
            clean: &clean,
        },
        &mut replay,
        &mut *replay_writer,
        &mut cleaner,
    )?;
    if !live.is_empty()
        && let Some(writer) = &mut recording_writer
    {
        writer
            .write_output_with(
                u64::try_from(recording_started.elapsed().as_micros()).unwrap_or(u64::MAX),
                &[],
                Some(&live),
                || {},
            )
            .map_err(|error| error.to_string())?;
    }
    let prelude_suppressed = live_gate.suppressed_prelude()
        || replay_gate.suppressed_prelude()
        || clean_gate.suppressed_prelude();
    raw_writer.flush().map_err(|error| error.to_string())?;
    replay_writer.flush().map_err(|error| error.to_string())?;
    let clean_summary = cleaner.finish().map_err(|error| error.to_string())?;
    let clean_bytes = clean_summary.tail.into_bytes();
    Ok(TerminalStreamOutput {
        raw: raw.limited_output(DEFAULT_MAX_OUTPUT_BYTES),
        replay: replay.limited_output(DEFAULT_MAX_OUTPUT_BYTES),
        clean: limit_output_bytes_with_original(
            &clean_bytes,
            usize::try_from(clean_summary.bytes_written).unwrap_or(usize::MAX),
            MAX_INLINE_TERMINAL_OUTPUT_BYTES,
            clean_summary.tail_truncated,
        ),
        raw_artifact_path: paths.raw,
        replay_artifact_path: paths.replay,
        clean_artifact_path: paths.clean,
        recording_path: paths.recording,
        recording_writer,
        prelude_suppressed,
    })
}

#[derive(Clone, Copy)]
struct StreamOutputs<'a> {
    replay: &'a [u8],
    clean: &'a [u8],
}

fn write_stream_outputs<W: Write>(
    outputs: StreamOutputs<'_>,
    replay: &mut RetainedStream,
    replay_writer: &mut dyn Write,
    cleaner: &mut terminal_clean::TerminalCleanWriter<&mut W>,
) -> Result<(), String> {
    if !outputs.replay.is_empty() {
        replay.write_chunk(replay_writer, outputs.replay, DEFAULT_MAX_OUTPUT_BYTES)?;
    }
    if !outputs.clean.is_empty() {
        cleaner
            .write_chunk(outputs.clean)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn raw_artifact_path(
    artifact_dir: Option<&Path>,
    tool_call_id: &str,
) -> Result<Option<PathBuf>, String> {
    artifact_path(artifact_dir, tool_call_id, |safe_tool_call_id| {
        format!("tool-output-{safe_tool_call_id}-pty.txt")
    })
}

fn replay_artifact_path(
    artifact_dir: Option<&Path>,
    tool_call_id: &str,
) -> Result<Option<PathBuf>, String> {
    artifact_path(artifact_dir, tool_call_id, |safe_tool_call_id| {
        format!("tool-output-{safe_tool_call_id}-replay-pty.txt")
    })
}

fn recording_artifact_path(
    artifact_dir: Option<&Path>,
    tool_call_id: &str,
) -> Result<Option<PathBuf>, String> {
    artifact_path(artifact_dir, tool_call_id, |safe_tool_call_id| {
        format!("tool-output-{safe_tool_call_id}.bcsr")
    })
}

fn clean_artifact_path(
    artifact_dir: Option<&Path>,
    tool_call_id: &str,
) -> Result<Option<PathBuf>, String> {
    artifact_path(artifact_dir, tool_call_id, |safe_tool_call_id| {
        format!("tool-output-{safe_tool_call_id}-clean.txt")
    })
}

fn artifact_path(
    artifact_dir: Option<&Path>,
    tool_call_id: &str,
    name: impl FnOnce(&str) -> String,
) -> Result<Option<PathBuf>, String> {
    let Some(artifact_dir) = artifact_dir else {
        return Ok(None);
    };
    std::fs::create_dir_all(artifact_dir).map_err(|error| error.to_string())?;
    let safe_tool_call_id = tool_call_id
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>();
    Ok(Some(artifact_dir.join(name(&safe_tool_call_id))))
}

fn raw_artifact_writer(path: Option<&Path>) -> Result<Box<dyn Write + Send>, String> {
    artifact_writer(path)
}

fn clean_artifact_writer(path: Option<&Path>) -> Result<Box<dyn Write + Send>, String> {
    artifact_writer(path)
}

fn artifact_writer(path: Option<&Path>) -> Result<Box<dyn Write + Send>, String> {
    path.map_or_else(
        || Ok(Box::new(Vec::<u8>::new()) as Box<dyn Write + Send>),
        |path| {
            File::create(path)
                .map(|file| Box::new(file) as Box<dyn Write + Send>)
                .map_err(|error| error.to_string())
        },
    )
}

fn shell_recording_commit_observer(
    existing_presentation: Option<Arc<StdMutex<PrimaryPresentationPublisher>>>,
    events: ServiceEventEmitter,
    tool_call_id: &str,
    timeout_ms: u64,
    arguments: serde_json::Value,
    limits: bcode_plugin_sdk::TransientProgressLimits,
    cancellation: bcode_plugin_sdk::ServiceCancellation,
) -> recording::ShellRecordingCommitObserver {
    let tool_call_id = tool_call_id.to_owned();
    let publication_state = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let presentation = existing_presentation.unwrap_or_else(|| {
        Arc::new(StdMutex::new(
            PrimaryPresentationPublisher::with_limits_and_cancellation(
                events,
                &tool_call_id,
                "bcode.shell",
                SHELL_RUN_SCHEMA,
                SHELL_SCHEMA_VERSION,
                bcode_tool::ToolPresentationRetention::RetainLatest,
                limits,
                cancellation,
            ),
        ))
    });
    Arc::new(move |commit| {
        let artifact = ToolContributionArtifact {
            artifact_id: format!("{tool_call_id}-shell-run"),
            reference_key: SHELL_RECORDING_REF_KEY.to_owned(),
            content_type: Some(SHELL_RECORDING_CONTENT_TYPE.to_owned()),
            storage_uri: file_storage_uri(&commit.path)
                .unwrap_or_else(|| commit.path.display().to_string()),
            committed_bytes: commit.committed_bytes,
            revision: commit.committed_bytes,
            finalized: commit.finalized,
            availability: None,
        };
        let mut presentation = presentation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let publish_immediately = if commit.finalized {
            true
        } else if commit.committed_bytes > recording::RECORDING_HEADER_AND_START_BYTES {
            publication_state.store(2, std::sync::atomic::Ordering::Release);
            true
        } else if publication_state.load(std::sync::atomic::Ordering::Acquire) == 0 {
            publication_state.store(1, std::sync::atomic::Ordering::Release);
            true
        } else {
            false
        };
        if publish_immediately {
            let _ = presentation.replace_with_artifact_as(
                SHELL_RUN_SCHEMA,
                SHELL_SCHEMA_VERSION,
                &ShellLiveRecordingPayload {
                    mode: "terminal",
                    timeout_ms,
                    arguments: arguments.clone(),
                },
                artifact,
            );
        } else {
            let _ = presentation.replace_with_artifact_as_if_ready(
                SHELL_RUN_SCHEMA,
                SHELL_SCHEMA_VERSION,
                &ShellLiveRecordingPayload {
                    mode: "terminal",
                    timeout_ms,
                    arguments: arguments.clone(),
                },
                artifact,
            );
        }
    })
}

fn emit_tool_lifecycle(events: ServiceEventEmitter, event: &ToolInvocationLifecycleEvent) {
    if let Ok(payload) = serde_json::to_vec(event) {
        events.emit(&payload);
    }
}

#[cfg(test)]
fn limit_output_bytes(bytes: &[u8], max_bytes: usize) -> LimitedOutput {
    limit_output_bytes_with_original(bytes, bytes.len(), max_bytes, false)
}

fn limit_output_bytes_with_original(
    bytes: &[u8],
    original_bytes: usize,
    max_bytes: usize,
    already_truncated: bool,
) -> LimitedOutput {
    let retained_len = valid_utf8_prefix_len(bytes, max_bytes.min(bytes.len()));
    let text = String::from_utf8_lossy(&bytes[..retained_len]).into_owned();
    LimitedOutput {
        text,
        original_bytes,
        retained_bytes: retained_len,
        truncated: already_truncated || retained_len < bytes.len() || bytes.len() < original_bytes,
    }
}

fn valid_utf8_prefix_len(bytes: &[u8], max_len: usize) -> usize {
    let mut len = max_len.min(bytes.len());
    while len > 0 && std::str::from_utf8(&bytes[..len]).is_err() {
        len = len.saturating_sub(1);
    }
    len
}

fn join_reader(
    handle: std::thread::JoinHandle<Result<TerminalStreamOutput, String>>,
) -> Result<TerminalStreamOutput, String> {
    handle
        .join()
        .map_err(|_| "output reader thread panicked".to_string())?
}

fn json_response<T: serde::Serialize>(value: &T) -> ServiceResponse {
    match ServiceResponse::json(value) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("encode_failed", error.to_string()),
    }
}

fn shell_run_artifact(
    tool_call_id: &str,
    result: &ShellRunResult,
    clean_ref: Option<ToolArtifactRef>,
    raw_ref: Option<ToolArtifactRef>,
) -> ToolInvocationResult {
    ToolInvocationResult::Artifact {
        artifact: Box::new(ToolArtifact {
            artifact_id: format!("{tool_call_id}-shell-run"),
            producer_plugin_id: "bcode.shell".to_string(),
            schema: SHELL_RUN_SCHEMA.to_owned(),
            schema_version: SHELL_SCHEMA_VERSION,
            tool_call_id: Some(tool_call_id.to_string()),
            title: Some("Shell run".to_string()),
            metadata: serde_json::to_value(result).unwrap_or_else(|_| json!({})),
            refs: clean_ref.into_iter().chain(raw_ref).collect(),
        }),
    }
}

fn finalize_recording(
    output: &mut TerminalStreamOutput,
    started: Instant,
    status: &TerminalShellStatus,
    columns: u16,
    rows: u16,
) -> Result<Option<ToolArtifactRef>, String> {
    let Some(writer) = output.recording_writer.take() else {
        return Ok(None);
    };
    let summary = writer
        .finish(
            u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            Some(status.exit_code),
            status.signal.clone(),
            status.timed_out,
            status.cancelled,
        )
        .map_err(|error| error.to_string())?;
    let path = output
        .recording_path
        .as_deref()
        .ok_or_else(|| "recording writer had no final path".to_owned())?;
    Ok(Some(ToolArtifactRef {
        key: SHELL_RECORDING_REF_KEY.to_owned(),
        content_type: Some(SHELL_RECORDING_CONTENT_TYPE.to_owned()),
        storage_uri: file_storage_uri(path),
        byte_len: std::fs::metadata(path).ok().map(|metadata| metadata.len()),
        metadata: Some(json!({
            "format": "bcode.shell.recording",
            "format_version": 3,
            "authoritative_replay": true,
            "columns": columns,
            "rows": rows,
            "frame_count": summary.frame_count,
            "output_bytes": summary.output_bytes,
            "checksum_sha256": summary.checksum_sha256,
            "availability": "complete",
            "complete": true,
            "retention": "session_lifetime",
            "eviction": "none",
        })),
    }))
}

fn clean_artifact_ref(path: &Path, output: &LimitedOutput) -> ToolArtifactRef {
    ToolArtifactRef {
        key: "clean_output".to_string(),
        content_type: Some("text/plain; charset=utf-8".to_string()),
        storage_uri: file_storage_uri(path),
        byte_len: Some(u64::try_from(output.original_bytes).unwrap_or(u64::MAX)),
        metadata: Some(json!({
            "description": "Model-oriented terminal transcript normalized by the shell plugin",
            "retained_tail_bytes": output.retained_bytes,
            "tail_truncated": output.truncated,
        })),
    }
}

fn raw_artifact_ref(
    path: &Path,
    output: &LimitedOutput,
    columns: u16,
    rows: u16,
) -> ToolArtifactRef {
    ToolArtifactRef {
        key: TERMINAL_PTY_STREAM_REF_KEY.to_string(),
        content_type: Some(TERMINAL_PTY_STREAM_CONTENT_TYPE.to_string()),
        storage_uri: file_storage_uri(path),
        byte_len: Some(u64::try_from(output.retained_bytes).unwrap_or(u64::MAX)),
        metadata: Some(json!({
            "description": "Raw terminal PTY stream for display replay",
            "stream": "pty",
            "columns": columns,
            "rows": rows,
            "retained_tail_bytes": output.retained_bytes,
            "tail_truncated": output.truncated,
            "encoding": "utf-8-lossy",
        })),
    }
}

fn file_storage_uri(path: &Path) -> Option<String> {
    url::Url::from_file_path(path)
        .ok()
        .map(|uri| uri.to_string())
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_concurrent_plugin_vtable!(
        ShellPlugin,
        include_str!("../bcode-plugin.toml")
    )
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn shell_tui_registry() -> bcode_plugin_sdk::tui::PluginTuiRegistry {
    let mut registry = bcode_plugin_sdk::tui::PluginTuiRegistry::default();
    registry.register_visual_adapter(
        ["shell-run-request-card", "shell-run-terminal-card"],
        Box::new(shell_run_tui::ShellRunTuiVisualAdapter::default()),
    );
    registry
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_concurrent_plugin!(ShellPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn workflow_command_plan(
        workspace: &Path,
        commands: Vec<contracts::ShellWorkflowCommand>,
    ) -> (
        bcode_workflow::WorkflowBlockInvocation,
        ShellWorkflowCommandPlan,
    ) {
        (
            bcode_workflow::WorkflowBlockInvocation {
                version: bcode_workflow::WorkflowBlockInvocation::VERSION,
                dispatch_identity: "dispatch-test".to_string(),
                workspace_root: workspace.to_path_buf(),
                input: serde_json::Value::Null,
                preparation: None,
            },
            ShellWorkflowCommandPlan {
                version: contracts::SHELL_COMMAND_PLAN_VERSION,
                cwd: PathBuf::from("."),
                commands,
                environment: contracts::ShellWorkflowEnvironment {
                    inherit: false,
                    set: std::collections::BTreeMap::new(),
                },
                output: contracts::ShellWorkflowOutputPolicy {
                    preview_bytes: 4,
                    artifact_spill: false,
                },
            },
        )
    }

    fn workflow_context_with_bridge(
        invocation: &bcode_workflow::WorkflowBlockInvocation,
        cancellation: bcode_plugin_sdk::ServiceCancellation,
        bridge: ServiceBridge,
    ) -> NativeServiceContext {
        NativeServiceContext {
            plugin_id: "bcode.shell".to_string(),
            request: ServiceRequest {
                interface_id: bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID.to_string(),
                operation: "exec".to_string(),
                payload: serde_json::to_vec(invocation).expect("invocation"),
            },
            config: bcode_plugin_sdk::PluginConfigContext::default(),
            events: ServiceEventEmitter::default(),
            cancellation,
            bridge,
            transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
        }
    }

    fn workflow_context(
        invocation: &bcode_workflow::WorkflowBlockInvocation,
        cancellation: bcode_plugin_sdk::ServiceCancellation,
    ) -> NativeServiceContext {
        workflow_context_with_bridge(invocation, cancellation, ServiceBridge::default())
    }

    #[test]
    fn shell_workflow_preparation_is_required_and_stale_descriptors_fail_closed() {
        let workspace = tempfile::tempdir().expect("workspace");
        let (mut invocation, plan) = workflow_command_plan(
            workspace.path(),
            vec![contracts::ShellWorkflowCommand {
                argv: vec!["true".to_string()],
                timeout_ms: 1_000,
                accepted_exit_codes: Some(vec![0]),
                continue_on_unaccepted_exit: false,
            }],
        );
        invocation.input = serde_json::to_value(&plan).expect("input");
        let context = workflow_context(
            &invocation,
            bcode_plugin_sdk::ServiceCancellation::default(),
        );
        let missing = invoke_workflow_block_contract(&context);
        assert_eq!(
            missing.error.as_ref().map(|error| error.code.as_str()),
            Some("invalid_preparation")
        );

        let block = shell_workflow_block_definition("exec");
        let request = bcode_workflow::WorkflowBlockPreparationRequest {
            version: bcode_workflow::WORKFLOW_BLOCK_PREPARATION_VERSION,
            block,
            context: bcode_workflow::WorkflowBlockPreparationContext {
                run_id: "run".to_string(),
                node_id: "node".to_string(),
                activation_id: "activation".to_string(),
                attempt: 0,
                preparation_identity: "workflow-preparation:run:node:activation".to_string(),
                workspace_root: workspace.path().to_path_buf(),
            },
            input: invocation.input.clone(),
        };
        let response = prepare_workflow_block_contract(&ServiceRequest {
            interface_id: bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID.to_string(),
            operation: bcode_workflow::WORKFLOW_BLOCK_PREPARE_OPERATION.to_string(),
            payload: serde_json::to_vec(&request).expect("request"),
        });
        let preparation: bcode_workflow::WorkflowBlockPreparationResponse =
            serde_json::from_slice(&response.payload).expect("preparation");
        let policy: bcode_agent_profile::ToolPolicyAuthorizationMetadata =
            serde_json::from_value(preparation.operation_facts.clone()).expect("policy facts");
        assert!(policy.requires_permission);
        match policy.operation {
            bcode_agent_profile::ToolPolicyOperation::Command {
                command,
                analysis,
                analysis_error,
            } => {
                assert!(command.is_some_and(|command| command.contains("true")));
                assert!(analysis.is_some());
                assert!(analysis_error.is_none());
            }
            operation => panic!("unexpected shell policy operation: {operation:?}"),
        }
        let duplicate = prepare_workflow_block_contract(&ServiceRequest {
            interface_id: bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID.to_string(),
            operation: bcode_workflow::WORKFLOW_BLOCK_PREPARE_OPERATION.to_string(),
            payload: serde_json::to_vec(&request).expect("duplicate request"),
        });
        let duplicate: bcode_workflow::WorkflowBlockPreparationResponse =
            serde_json::from_slice(&duplicate.payload).expect("duplicate preparation");
        assert_eq!(
            duplicate, preparation,
            "duplicate preparation must be stable"
        );

        let mut stale_preparation = preparation.clone();
        stale_preparation.input_sha256 = "f".repeat(64);
        invocation.preparation = Some(stale_preparation);
        let stale = invoke_workflow_block_contract(&workflow_context(
            &invocation,
            bcode_plugin_sdk::ServiceCancellation::default(),
        ));
        assert_eq!(
            stale.error.as_ref().map(|error| error.code.as_str()),
            Some("invalid_preparation")
        );

        invocation.preparation = Some(preparation);
        let cancelled = bcode_plugin_sdk::ServiceCancellation::default();
        cancelled.cancel();
        let cancelled_response =
            invoke_workflow_block_contract(&workflow_context(&invocation, cancelled));
        assert_eq!(
            cancelled_response
                .error
                .as_ref()
                .map(|error| error.code.as_str()),
            Some("cancelled")
        );
    }

    fn shell_workflow_block_definition(operation: &str) -> bcode_workflow::WorkflowBlockDefinition {
        bcode_workflow::WorkflowBlockDefinition {
            block_id: operation.to_string(),
            block_version: 1,
            plugin_id: "bcode.shell".to_string(),
            operation: operation.to_string(),
            input: bcode_workflow::ValueSchema::of::<serde_json::Value>(),
            output: bcode_workflow::ValueSchema::of::<serde_json::Value>(),
            effect: bcode_workflow::WorkflowBlockEffect::Mutating,
            resources: vec![bcode_workflow::ResourceClaim::write("repository")],
            authorization: bcode_workflow::WorkflowBlockAuthorization {
                capability: bcode_workflow::WorkflowToolCapability::Mutating,
                explicit_grant_required: true,
            },
            timeout_ms: 300_000,
            cancellation_supported: true,
            reconciliation: bcode_workflow::WorkflowBlockReconciliation::RepairRequired,
            automatic_retry: None,
            preparation_required: true,
        }
    }

    #[test]
    fn shell_workflow_preparation_fails_closed_on_invalid_script_analysis() {
        let block = shell_workflow_block_definition("exec");
        let request = bcode_workflow::WorkflowBlockPreparationRequest {
            version: bcode_workflow::WORKFLOW_BLOCK_PREPARATION_VERSION,
            block,
            context: bcode_workflow::WorkflowBlockPreparationContext {
                run_id: "run".to_string(),
                node_id: "node".to_string(),
                activation_id: "activation".to_string(),
                attempt: 0,
                preparation_identity: "workflow-preparation:run:node:activation".to_string(),
                workspace_root: std::env::temp_dir(),
            },
            input: serde_json::json!({"script": "eval \"$DYNAMIC_SOURCE\""}),
        };
        let response = prepare_workflow_block_contract(&ServiceRequest {
            interface_id: bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID.to_string(),
            operation: bcode_workflow::WORKFLOW_BLOCK_PREPARE_OPERATION.to_string(),
            payload: serde_json::to_vec(&request).expect("request"),
        });
        assert_eq!(
            response.error.as_ref().map(|error| error.code.as_str()),
            Some("command_analysis_failed")
        );
    }

    #[test]
    fn shell_script_request_defaults_and_advanced_fields_are_plugin_owned() {
        let scalar = contracts::ShellWorkflowScriptRequest {
            version: contracts::SHELL_SCRIPT_VERSION,
            script: "printf scalar".to_string(),
            shell: None,
            cwd: PathBuf::from("."),
            environment: BTreeMap::new(),
            timeout_ms: 300_000,
            accepted_exit_codes: vec![0],
            continue_on_unaccepted_exit: false,
            output: contracts::ShellWorkflowOutputPolicy {
                preview_bytes: 8_192,
                artifact_spill: true,
            },
        };
        let scalar_plan = shell_script_command_plan(scalar).expect("scalar plan");
        assert_eq!(scalar_plan.commands[0].accepted_exit_codes, Some(vec![0]));
        assert_eq!(
            scalar_plan.commands[0].argv.last().map(String::as_str),
            Some("printf scalar")
        );

        let advanced: contracts::ShellWorkflowScriptRequest = serde_json::from_value(json!({
            "script": "exit 7",
            "shell": ["bash", "-c"],
            "cwd": "subdir",
            "environment": {"MODE": "ci"},
            "timeout_ms": 120_000,
            "accepted_exit_codes": [0, 7],
            "continue_on_unaccepted_exit": true,
            "output": {"preview_bytes": 4096, "artifact_spill": false}
        }))
        .expect("advanced request");
        let advanced_plan = shell_script_command_plan(advanced).expect("advanced plan");
        assert_eq!(advanced_plan.cwd, PathBuf::from("subdir"));
        assert_eq!(advanced_plan.environment.set["MODE"], "ci");
        assert_eq!(advanced_plan.commands[0].argv, ["bash", "-c", "exit 7"]);
        assert_eq!(
            advanced_plan.commands[0].accepted_exit_codes,
            Some(vec![0, 7])
        );
        assert!(advanced_plan.commands[0].continue_on_unaccepted_exit);
        assert_eq!(advanced_plan.output.preview_bytes, 4096);
    }

    #[test]
    fn workflow_command_plan_rejects_persisted_secret_environment_values() {
        let workspace = tempfile::tempdir().expect("workspace");
        let (invocation, mut plan) = workflow_command_plan(
            workspace.path(),
            vec![contracts::ShellWorkflowCommand {
                argv: vec!["true".to_string()],
                timeout_ms: 1_000,
                accepted_exit_codes: None,
                continue_on_unaccepted_exit: false,
            }],
        );
        plan.environment
            .set
            .insert("API_TOKEN".to_string(), "secret-value".to_string());
        assert!(
            execute_workflow_command_plan(
                &workflow_context(
                    &invocation,
                    bcode_plugin_sdk::ServiceCancellation::new(Arc::new(
                        std::sync::atomic::AtomicBool::new(false),
                    )),
                ),
                &invocation,
                &plan,
            )
            .is_err()
        );
    }

    #[test]
    fn workflow_output_preview_preserves_binary_identity_without_claiming_utf8() {
        let bytes = [b'f', b'o', 0x80, b'o'];
        let (preview, utf8, truncated) = bounded_preview(&bytes, 4);
        assert_eq!(preview, "fo�o");
        assert!(!utf8);
        assert!(!truncated);
        let (_, utf8, truncated) = bounded_preview(b"abcdef", 3);
        assert!(utf8);
        assert!(truncated);
    }

    #[cfg(unix)]
    #[test]
    fn workflow_command_plan_v2_accepts_declared_exit_codes_and_controls_continuation() {
        let workspace = tempfile::tempdir().expect("workspace");
        let command = |script: &str, accepted_exit_codes, continue_on_unaccepted_exit| {
            contracts::ShellWorkflowCommand {
                argv: vec!["sh".to_string(), "-c".to_string(), script.to_string()],
                timeout_ms: 5_000,
                accepted_exit_codes,
                continue_on_unaccepted_exit,
            }
        };
        let (invocation, mut plan) = workflow_command_plan(
            workspace.path(),
            vec![
                command("exit 7", Some(vec![7, 0, 7]), false),
                command("printf reached", None, false),
            ],
        );
        plan.version = contracts::SHELL_COMMAND_PLAN_VERSION;
        let result = execute_workflow_command_plan(
            &workflow_context(
                &invocation,
                bcode_plugin_sdk::ServiceCancellation::default(),
            ),
            &invocation,
            &plan,
        )
        .expect("accepted result");
        assert!(result.passed);
        assert_eq!(result.version, contracts::SHELL_COMMAND_PLAN_VERSION);
        assert_eq!(result.commands[0].exit_code, Some(7));
        assert_eq!(result.commands[0].accepted_exit_codes, vec![0, 7]);
        assert!(result.commands[0].exit_accepted);

        plan.commands = vec![
            command("exit 8", Some(vec![7]), true),
            command("printf reached", None, false),
        ];
        let continued = execute_workflow_command_plan(
            &workflow_context(
                &invocation,
                bcode_plugin_sdk::ServiceCancellation::default(),
            ),
            &invocation,
            &plan,
        )
        .expect("continued result");
        assert!(!continued.passed);
        assert_eq!(continued.commands.len(), 2);
        assert_eq!(continued.commands[0].accepted_exit_codes, vec![7]);
        assert!(!continued.commands[0].exit_accepted);
    }

    #[cfg(unix)]
    #[test]
    fn workflow_command_plan_runs_sequentially_and_branches_on_nonzero() {
        let workspace = tempfile::tempdir().expect("workspace");
        let command = |script: &str, continue_on_unaccepted_exit| contracts::ShellWorkflowCommand {
            argv: vec!["sh".to_string(), "-c".to_string(), script.to_string()],
            timeout_ms: 5_000,
            accepted_exit_codes: None,
            continue_on_unaccepted_exit,
        };
        let (invocation, plan) = workflow_command_plan(
            workspace.path(),
            vec![
                command("printf first; printf error >&2; exit 7", true),
                command("printf second", false),
            ],
        );
        let result = execute_workflow_command_plan(
            &workflow_context(
                &invocation,
                bcode_plugin_sdk::ServiceCancellation::default(),
            ),
            &invocation,
            &plan,
        )
        .expect("result");
        assert!(!result.passed);
        assert_eq!(result.version, contracts::SHELL_COMMAND_PLAN_VERSION);
        assert!(
            result
                .commands
                .iter()
                .all(|command| command.accepted_exit_codes == [0])
        );
        assert_eq!(result.commands.len(), 2);
        assert_eq!(result.commands[0].exit_code, Some(7));
        assert_eq!(result.commands[0].stdout_preview, "firs");
        assert!(result.commands[0].stdout_truncated);
        assert_eq!(result.commands[1].stdout_preview, "seco");

        let (invocation, plan) = workflow_command_plan(
            workspace.path(),
            vec![
                command("exit 7", false),
                command("printf unreachable", false),
            ],
        );
        let stopped = execute_workflow_command_plan(
            &workflow_context(
                &invocation,
                bcode_plugin_sdk::ServiceCancellation::default(),
            ),
            &invocation,
            &plan,
        )
        .expect("stopped result");
        assert!(!stopped.passed);
        assert_eq!(stopped.commands.len(), 1);

        let (invocation, plan) = workflow_command_plan(
            workspace.path(),
            vec![command("printf success", false), command("true", false)],
        );
        let success = execute_workflow_command_plan(
            &workflow_context(
                &invocation,
                bcode_plugin_sdk::ServiceCancellation::default(),
            ),
            &invocation,
            &plan,
        )
        .expect("success");
        assert!(success.passed);
        assert_eq!(success.commands.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn workflow_command_plan_reports_signal_termination() {
        let workspace = tempfile::tempdir().expect("workspace");
        let (invocation, mut plan) = workflow_command_plan(
            workspace.path(),
            vec![contracts::ShellWorkflowCommand {
                argv: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "kill -TERM $$".to_string(),
                ],
                timeout_ms: 1_000,
                accepted_exit_codes: None,
                continue_on_unaccepted_exit: false,
            }],
        );
        plan.version = contracts::SHELL_COMMAND_PLAN_VERSION;
        let result = execute_workflow_command_plan(
            &workflow_context(
                &invocation,
                bcode_plugin_sdk::ServiceCancellation::default(),
            ),
            &invocation,
            &plan,
        )
        .expect("signaled result");
        assert!(!result.passed);
        assert_eq!(
            result.commands[0].status,
            ShellWorkflowCommandStatus::Signaled
        );
        assert_eq!(result.commands[0].exit_code, None);
        assert_eq!(result.commands[0].signal, Some(15));
        assert!(!result.commands[0].exit_accepted);
    }

    #[cfg(unix)]
    #[test]
    fn workflow_command_plan_reports_timeout_spawn_failure_and_cancellation() {
        let workspace = tempfile::tempdir().expect("workspace");
        let cases = [
            (
                contracts::ShellWorkflowCommand {
                    argv: vec!["sh".to_string(), "-c".to_string(), "sleep 1".to_string()],
                    timeout_ms: 10,
                    accepted_exit_codes: None,
                    continue_on_unaccepted_exit: false,
                },
                ShellWorkflowCommandStatus::TimedOut,
                bcode_plugin_sdk::ServiceCancellation::default(),
            ),
            (
                contracts::ShellWorkflowCommand {
                    argv: vec!["bcode-definitely-missing-command".to_string()],
                    timeout_ms: 1_000,
                    accepted_exit_codes: None,
                    continue_on_unaccepted_exit: false,
                },
                ShellWorkflowCommandStatus::SpawnFailed,
                bcode_plugin_sdk::ServiceCancellation::default(),
            ),
        ];
        for (command, expected, cancellation) in cases {
            let (invocation, plan) = workflow_command_plan(workspace.path(), vec![command]);
            let result = execute_workflow_command_plan(
                &workflow_context(&invocation, cancellation),
                &invocation,
                &plan,
            )
            .expect("result");
            assert_eq!(result.commands[0].status, expected);
        }
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let cancellation = bcode_plugin_sdk::ServiceCancellation::new(cancelled);
        let (invocation, plan) = workflow_command_plan(
            workspace.path(),
            vec![contracts::ShellWorkflowCommand {
                argv: vec!["sh".to_string(), "-c".to_string(), "sleep 1".to_string()],
                timeout_ms: 1_000,
                accepted_exit_codes: None,
                continue_on_unaccepted_exit: false,
            }],
        );
        let result = execute_workflow_command_plan(
            &workflow_context(&invocation, cancellation),
            &invocation,
            &plan,
        )
        .expect("result");
        assert_eq!(
            result.commands[0].status,
            ShellWorkflowCommandStatus::Cancelled
        );
    }

    #[cfg(unix)]
    extern "C" fn workflow_artifact_bridge(
        request_ptr: *const u8,
        request_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
        _user_data: *mut std::ffi::c_void,
    ) -> i32 {
        // SAFETY: the SDK supplies a valid encoded request for the synchronous callback.
        let request = unsafe { std::slice::from_raw_parts(request_ptr, request_len) };
        let request: ServiceBridgeRequest = serde_json::from_slice(request).expect("request");
        let ServiceBridgeRequest::WriteArtifact(artifact) = request else {
            panic!("expected artifact write");
        };
        let response =
            ServiceBridgeResponse::Artifact(bcode_tool::ToolArtifactWriteResolution::Written {
                artifact_id: artifact.artifact_id,
                byte_len: u64::try_from(artifact.bytes.len()).expect("length"),
                reference: serde_json::json!({"storage_uri": "file:///tmp/workflow-output"}),
            });
        let encoded = serde_json::to_vec(&response).expect("response");
        assert!(encoded.len() <= output_capacity);
        // SAFETY: the SDK supplies a writable response buffer of `output_capacity` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
            *output_len = encoded.len();
        }
        bcode_plugin_sdk::SERVICE_BRIDGE_STATUS_OK
    }

    #[cfg(unix)]
    #[test]
    fn workflow_command_plan_spills_truncated_output_to_artifact() {
        let workspace = tempfile::tempdir().expect("workspace");
        let (invocation, mut plan) = workflow_command_plan(
            workspace.path(),
            vec![contracts::ShellWorkflowCommand {
                argv: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "printf abcdef".to_string(),
                ],
                timeout_ms: 1_000,
                accepted_exit_codes: None,
                continue_on_unaccepted_exit: false,
            }],
        );
        plan.output.artifact_spill = true;
        let context = workflow_context_with_bridge(
            &invocation,
            bcode_plugin_sdk::ServiceCancellation::default(),
            ServiceBridge::new(
                Some(workflow_artifact_bridge),
                std::ptr::null_mut(),
                bcode_plugin_sdk::ServiceCancellation::default(),
            ),
        );
        let result = execute_workflow_command_plan(&context, &invocation, &plan).expect("result");
        assert!(result.commands[0].stdout_truncated);
        assert_eq!(result.artifacts.len(), 1);
        assert_eq!(result.artifacts[0].schema, SHELL_WORKFLOW_STDOUT_SCHEMA);
    }

    #[cfg(unix)]
    #[test]
    fn workflow_command_plan_rejects_symlink_cwd_escape() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        symlink(outside.path(), workspace.path().join("escape")).expect("symlink");
        let (invocation, mut plan) = workflow_command_plan(
            workspace.path(),
            vec![contracts::ShellWorkflowCommand {
                argv: vec!["true".to_string()],
                timeout_ms: 1_000,
                accepted_exit_codes: None,
                continue_on_unaccepted_exit: false,
            }],
        );
        plan.cwd = PathBuf::from("escape");
        assert!(
            execute_workflow_command_plan(
                &workflow_context(
                    &invocation,
                    bcode_plugin_sdk::ServiceCancellation::default()
                ),
                &invocation,
                &plan,
            )
            .is_err()
        );
    }

    #[test]
    fn workflow_command_plan_digest_is_stable_and_changes_with_exact_plan() {
        let plan = workflow_command_plan(
            std::env::temp_dir().as_path(),
            vec![contracts::ShellWorkflowCommand {
                argv: vec!["true".to_string()],
                timeout_ms: 1_000,
                accepted_exit_codes: None,
                continue_on_unaccepted_exit: false,
            }],
        )
        .1;
        let first = canonical_command_plan_sha256(&plan).expect("digest");
        assert_eq!(first, canonical_command_plan_sha256(&plan).expect("digest"));
        assert_eq!(first.len(), 64);
        let mut explicit_default = plan.clone();
        explicit_default.version = contracts::SHELL_COMMAND_PLAN_VERSION;
        explicit_default.commands[0].accepted_exit_codes = Some(vec![0]);
        let mut implicit_default = explicit_default.clone();
        implicit_default.commands[0].accepted_exit_codes = None;
        assert_eq!(
            canonical_command_plan_sha256(&explicit_default).expect("explicit default"),
            canonical_command_plan_sha256(&implicit_default).expect("implicit default")
        );
        let mut changed = plan;
        changed.commands[0].timeout_ms = 2_000;
        assert_ne!(
            first,
            canonical_command_plan_sha256(&changed).expect("changed digest")
        );
    }

    #[test]
    fn nonzero_shell_result_is_branchable_without_provider_dispatch() {
        let predicate = bcode_workflow::PredicateExpression::Equals {
            version: bcode_workflow::WORKFLOW_PREDICATE_VERSION,
            path: "passed".to_string(),
            value: serde_json::json!(false),
        };
        let result = ShellWorkflowCommandPlanResult {
            version: contracts::SHELL_COMMAND_PLAN_VERSION,
            plan_sha256: "a".repeat(64),
            passed: false,
            commands: vec![ShellWorkflowCommandResult {
                index: 0,
                status: ShellWorkflowCommandStatus::Exited,
                exit_code: Some(7),
                accepted_exit_codes: vec![0],
                exit_accepted: false,
                signal: None,
                duration_ms: 1,
                stdout_preview: String::new(),
                stderr_preview: String::new(),
                stdout_bytes: 0,
                stderr_bytes: 0,
                stdout_encoding: contracts::ShellWorkflowOutputEncoding::Utf8,
                stderr_encoding: contracts::ShellWorkflowOutputEncoding::Utf8,
                stdout_truncated: false,
                stderr_truncated: false,
            }],
            artifacts: Vec::new(),
        };
        let value = serde_json::to_value(result).expect("value");
        assert!(predicate.evaluate_value(&value).expect("predicate"));
        assert!(
            !workflow_context(
                &bcode_workflow::WorkflowBlockInvocation {
                    version: bcode_workflow::WorkflowBlockInvocation::VERSION,
                    dispatch_identity: "dispatch".to_string(),
                    workspace_root: std::env::temp_dir(),
                    input: serde_json::Value::Null,
                    preparation: None,
                },
                bcode_plugin_sdk::ServiceCancellation::default(),
            )
            .bridge
            .is_available(),
            "procedural branchability does not invoke model/provider services"
        );
    }

    #[test]
    fn workflow_command_plan_rejects_cwd_escape_and_invalid_environment() {
        let workspace = tempfile::tempdir().expect("workspace");
        let (invocation, mut plan) = workflow_command_plan(
            workspace.path(),
            vec![contracts::ShellWorkflowCommand {
                argv: vec!["command".to_string()],
                timeout_ms: 1_000,
                accepted_exit_codes: None,
                continue_on_unaccepted_exit: false,
            }],
        );
        plan.cwd = PathBuf::from("../escape");
        assert!(
            execute_workflow_command_plan(
                &workflow_context(
                    &invocation,
                    bcode_plugin_sdk::ServiceCancellation::default()
                ),
                &invocation,
                &plan,
            )
            .is_err()
        );
        plan.cwd = PathBuf::from(".");
        plan.environment
            .set
            .insert("BAD=NAME".to_string(), "value".to_string());
        assert!(validate_workflow_environment(&plan.environment).is_err());

        let (_, mut oversized) = workflow_command_plan(
            workspace.path(),
            (0..65)
                .map(|_| contracts::ShellWorkflowCommand {
                    argv: vec!["true".to_string()],
                    timeout_ms: 1_000,
                    accepted_exit_codes: None,
                    continue_on_unaccepted_exit: false,
                })
                .collect(),
        );
        oversized.output.preview_bytes = 1;
        let context = workflow_context(
            &invocation,
            bcode_plugin_sdk::ServiceCancellation::default(),
        );
        let mut prepared_invocation = invocation;
        let prepared_input = serde_json::to_value(&oversized).expect("plan");
        let input_sha256 = workflow_block_input_sha256(&prepared_input).expect("checksum");
        prepared_invocation.input = prepared_input;
        prepared_invocation.preparation = Some(bcode_workflow::WorkflowBlockPreparationResponse {
            version: bcode_workflow::WORKFLOW_BLOCK_PREPARATION_VERSION,
            input_sha256: input_sha256.clone(),
            owner_id: "bcode.shell".to_string(),
            operation_facts: serde_json::to_value(
                bcode_agent_profile::ToolPolicyAuthorizationMetadata {
                    requires_permission: true,
                    aliases: vec![SHELL_RUN_TOOL_NAME.to_string()],
                    compatibility_aliases: Vec::new(),
                    capabilities: shell_policy_identity().capabilities,
                    permission_category: Some("command".to_string()),
                    operation: workflow_command_analysis(&prepared_invocation.input, &plan),
                },
            )
            .expect("facts"),
            descriptor: serde_json::to_value(ShellWorkflowPreparationDescriptor {
                version: 1,
                block_id: "exec".to_string(),
                input_sha256,
            })
            .expect("descriptor"),
            diagnostics: Vec::new(),
        });
        let response = invoke_workflow_block_contract(&NativeServiceContext {
            request: ServiceRequest {
                payload: serde_json::to_vec(&prepared_invocation).expect("invocation"),
                ..context.request.clone()
            },
            ..context
        });
        assert_eq!(
            response.error.as_ref().map(|error| error.code.as_str()),
            Some("invalid_request")
        );
    }

    fn preparation_request_with_context(
        arguments: serde_json::Value,
        host_context: Vec<bcode_tool::ToolHostContextEntry>,
    ) -> ServiceRequest {
        let preparation = bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "prepare-test".to_owned(),
                tool_name: "shell.run".to_owned(),
                arguments,
            },
            host_context,
        };
        ServiceRequest {
            interface_id: TOOL_SERVICE_INTERFACE_ID.to_owned(),
            operation: bcode_tool::OP_PREPARE_TOOL.to_owned(),
            payload: serde_json::to_vec(&preparation).expect("preparation request should encode"),
        }
    }

    fn preparation_request(arguments: serde_json::Value) -> ServiceRequest {
        preparation_request_with_context(arguments, Vec::new())
    }

    #[test]
    #[cfg(windows)]
    fn windows_shell_plan_uses_cmd_contract() {
        let plan = shell_program_and_args(
            "echo windows-smoke",
            None,
            ShellToolEnvConfig {
                mode: ShellToolEnvMode::Inherit,
                ..ShellToolEnvConfig::default()
            },
            "windows-shell-test",
        )
        .expect("Windows shell plan");
        assert_eq!(plan.program, "cmd");
        assert_eq!(plan.args, ["/C", "echo windows-smoke"]);
        assert!(plan.prelude_marker.is_none());
    }

    #[test]
    fn shell_preparation_serializes_owner_resolved_workspace_and_artifact_roots() {
        let response = prepare_shell_tool(&preparation_request_with_context(
            serde_json::json!({"command": "printf hello"}),
            vec![
                bcode_tool::ToolHostContextEntry {
                    schema: bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA.to_owned(),
                    schema_version: bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION,
                    payload: serde_json::json!({"working_directory": "/tmp/workspace"}),
                },
                bcode_tool::ToolHostContextEntry {
                    schema: bcode_tool::TOOL_ARTIFACT_CONTEXT_SCHEMA.to_owned(),
                    schema_version: bcode_tool::TOOL_ARTIFACT_CONTEXT_SCHEMA_VERSION,
                    payload: serde_json::json!({"root": "/tmp/artifacts/session-1"}),
                },
            ],
        ));
        assert!(response.error.is_none(), "{:?}", response.error);
        let prepared = response
            .payload_json::<bcode_tool::ToolPreparationResponse>()
            .expect("preparation response");

        assert_eq!(
            serde_json::from_value::<ShellPreparationDescriptor>(prepared.descriptor)
                .expect("Shell descriptor"),
            ShellPreparationDescriptor {
                workspace_root: Some(PathBuf::from("/tmp/workspace")),
                artifact_root: Some(PathBuf::from("/tmp/artifacts/session-1")),
                timeout_ms: DEFAULT_SHELL_TIMEOUT_MS,
            }
        );

        let explicit = prepare_shell_tool(&preparation_request(serde_json::json!({
            "command": "printf hello",
            "timeout_ms": 1_234,
        })));
        let prepared = explicit
            .payload_json::<bcode_tool::ToolPreparationResponse>()
            .expect("explicit timeout preparation response");
        assert_eq!(
            serde_json::from_value::<ShellPreparationDescriptor>(prepared.descriptor)
                .expect("Shell descriptor")
                .timeout_ms,
            1_234
        );
    }

    #[test]
    fn shell_tool_and_workflow_preparation_emit_identical_policy_facts() {
        let arguments = serde_json::json!({"command": "git status --short"});
        let tool = prepare_shell_tool(&preparation_request(arguments))
            .payload_json::<bcode_tool::ToolPreparationResponse>()
            .expect("tool preparation");
        let tool_policy = bcode_agent_profile::tool_policy_authorization_metadata(
            &tool.authorization,
            SHELL_RUN_TOOL_NAME,
        )
        .expect("tool policy");
        let block = shell_workflow_block_definition("exec");
        let workflow = prepare_workflow_block_contract(&ServiceRequest {
            interface_id: bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID.to_string(),
            operation: bcode_workflow::WORKFLOW_BLOCK_PREPARE_OPERATION.to_string(),
            payload: serde_json::to_vec(&bcode_workflow::WorkflowBlockPreparationRequest {
                version: bcode_workflow::WORKFLOW_BLOCK_PREPARATION_VERSION,
                block,
                context: bcode_workflow::WorkflowBlockPreparationContext {
                    run_id: "run".to_string(),
                    node_id: "node".to_string(),
                    activation_id: "activation".to_string(),
                    attempt: 0,
                    preparation_identity: "workflow-preparation:run:node:activation".to_string(),
                    workspace_root: PathBuf::from("/tmp/workspace"),
                },
                input: serde_json::json!("git status --short"),
            })
            .expect("workflow preparation request"),
        })
        .payload_json::<bcode_workflow::WorkflowBlockPreparationResponse>()
        .expect("workflow preparation");
        let workflow_policy: bcode_agent_profile::ToolPolicyAuthorizationMetadata =
            serde_json::from_value(workflow.operation_facts).expect("workflow policy");
        assert_eq!(workflow_policy, tool_policy);
    }

    #[test]
    fn shell_preparation_rejects_relative_artifact_root() {
        let response = prepare_shell_tool(&preparation_request_with_context(
            serde_json::json!({"command": "printf hello"}),
            vec![bcode_tool::ToolHostContextEntry {
                schema: bcode_tool::TOOL_ARTIFACT_CONTEXT_SCHEMA.to_owned(),
                schema_version: bcode_tool::TOOL_ARTIFACT_CONTEXT_SCHEMA_VERSION,
                payload: serde_json::json!({"root": "relative"}),
            }],
        ));

        assert!(response.payload.is_empty());
        let error = response.error.expect("invalid preparation error");
        assert_eq!(error.code, "invalid_preparation");
        assert!(error.message.contains("must be absolute"));
    }

    fn prepared_metadata(
        arguments: serde_json::Value,
    ) -> bcode_agent_profile::ToolPolicyAuthorizationMetadata {
        let response = prepare_shell_tool(&preparation_request(arguments));
        assert!(response.error.is_none(), "{:?}", response.error);
        let prepared = response
            .payload_json::<bcode_tool::ToolPreparationResponse>()
            .expect("preparation response should decode");
        bcode_agent_profile::tool_policy_authorization_metadata(
            &prepared.authorization,
            "shell.run",
        )
        .expect("authorization metadata should decode")
    }

    #[test]
    fn shell_preparation_encodes_complete_incomplete_and_invalid_analysis() {
        let complete = prepared_metadata(serde_json::json!({"command": "printf hello"}));
        let bcode_agent_profile::ToolPolicyOperation::Command {
            analysis,
            analysis_error,
            ..
        } = complete.operation
        else {
            panic!("shell preparation must produce command policy");
        };
        assert!(analysis.is_some_and(|analysis| analysis.completeness.is_complete()));
        assert!(analysis_error.is_none());

        let incomplete = prepared_metadata(serde_json::json!({
            "command": "cmd=printf; \"$cmd\" hello"
        }));
        let bcode_agent_profile::ToolPolicyOperation::Command {
            analysis,
            analysis_error,
            ..
        } = incomplete.operation
        else {
            panic!("shell preparation must produce command policy");
        };
        assert!(analysis.is_some_and(|analysis| !analysis.completeness.is_complete()));
        assert!(analysis_error.is_none());

        for arguments in [
            serde_json::Value::Null,
            serde_json::json!({"command": 7}),
            serde_json::json!({"command": "if true; then"}),
        ] {
            let invalid = prepared_metadata(arguments);
            let bcode_agent_profile::ToolPolicyOperation::Command {
                analysis,
                analysis_error,
                ..
            } = invalid.operation
            else {
                panic!("shell preparation must produce command policy");
            };
            assert!(analysis.is_none());
            assert!(analysis_error.is_some());
        }
    }

    #[test]
    fn shell_request_uses_primary_presentation_without_definition_ui() {
        let encoded =
            serde_json::to_value(shell_tool_definition()).expect("tool definition encodes");
        assert!(encoded.get("ui").is_none());
    }

    use std::ffi::c_void;
    use std::sync::Mutex;

    #[test]
    fn shell_catalog_preparation_accepts_missing_command() {
        let definition = shell_tool_definition();
        let request = bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "catalog".to_owned(),
                tool_name: definition.name,
                arguments: serde_json::Value::Null,
            },
            host_context: Vec::new(),
        };
        let bcode_plugin_sdk::ToolPolicyOperation::Command {
            command,
            analysis,
            analysis_error,
        } = shell_policy_operation(&request)
        else {
            panic!("shell owner must produce command policy");
        };
        assert!(command.is_none());
        assert!(analysis.is_none());
        assert!(analysis_error.is_some());
    }

    #[test]
    fn shell_owner_prepares_exact_command_without_generic_extractors() {
        let definition = shell_tool_definition();
        let request = bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "call".to_owned(),
                tool_name: definition.name,
                arguments: serde_json::json!({"command": "printf hello"}),
            },
            host_context: Vec::new(),
        };
        let identity = shell_policy_identity();
        assert_eq!(
            identity.compatibility_aliases,
            vec![bcode_tool::ToolCompatibilityAlias::new("claude", "Bash")]
        );
        assert_eq!(identity.capabilities, vec!["shell.run", "process.execute"]);
        assert_eq!(identity.permission_category.as_deref(), Some("command"));
        let bcode_plugin_sdk::ToolPolicyOperation::Command {
            command,
            analysis,
            analysis_error,
        } = shell_policy_operation(&request)
        else {
            panic!("shell owner must produce a command operation");
        };
        assert_eq!(command.as_deref(), Some("printf hello"));
        assert!(analysis.is_some_and(|analysis| analysis.completeness.is_complete()));
        assert!(analysis_error.is_none());
    }

    #[test]
    fn shell_request_payload_serializes_for_primary_presentation() {
        let arguments = ShellRunArguments {
            command: "echo test".to_owned(),
            cwd: None,
            timeout_ms: None,
            columns: None,
            rows: None,
            format_commands: None,
        };
        let payload = serde_json::to_value(arguments).expect("arguments encode");
        assert_eq!(payload["command"], "echo test");
    }

    extern "C" fn capture_service_event(
        payload: *const u8,
        payload_len: usize,
        user_data: *mut c_void,
    ) {
        // SAFETY: tests pass a live `Mutex<Vec<Vec<u8>>>` pointer for the entire invocation and the
        // emitter invokes this callback synchronously.
        let events = unsafe { &*(user_data.cast::<Mutex<Vec<Vec<u8>>>>()) };
        // SAFETY: the emitter provides a valid payload pointer and length for this callback.
        let payload = unsafe { std::slice::from_raw_parts(payload, payload_len) };
        events.lock().expect("event lock").push(payload.to_vec());
    }

    #[test]
    fn shell_primary_presentation_keeps_identity_generation_and_revision_across_schema_change() {
        let events = Mutex::new(Vec::<Vec<u8>>::new());
        let emitter = ServiceEventEmitter::new(
            Some(capture_service_event),
            std::ptr::from_ref(&events).cast_mut().cast(),
        );
        let presentation = Arc::new(StdMutex::new(
            PrimaryPresentationPublisher::with_limits_and_cancellation(
                emitter,
                "call-continuity",
                "bcode.shell",
                "bcode.tool.request.shell.run",
                SHELL_SCHEMA_VERSION,
                bcode_tool::ToolPresentationRetention::RetainLatest,
                bcode_plugin_sdk::TransientProgressLimits::default(),
                bcode_plugin_sdk::ServiceCancellation::default(),
            ),
        ));
        let arguments = serde_json::json!({
            "command": "printf hello",
            "cwd": "/tmp/project",
            "timeout_ms": 30_000,
            "format_commands": true,
            "columns": 100,
            "rows": 30
        });
        presentation
            .lock()
            .expect("presentation")
            .replace(&arguments)
            .expect("request presentation");
        let observer = shell_recording_commit_observer(
            Some(Arc::clone(&presentation)),
            emitter,
            "call-continuity",
            30_000,
            arguments.clone(),
            bcode_plugin_sdk::TransientProgressLimits::default(),
            bcode_plugin_sdk::ServiceCancellation::default(),
        );
        observer(recording::ShellRecordingCommit {
            path: PathBuf::from("call-continuity.bcsr.partial"),
            committed_bytes: recording::RECORDING_HEADER_AND_START_BYTES,
            finalized: false,
        });
        observer(recording::ShellRecordingCommit {
            path: PathBuf::from("call-continuity.bcsr"),
            committed_bytes: recording::RECORDING_HEADER_AND_START_BYTES + 10,
            finalized: true,
        });

        let updates = events
            .lock()
            .expect("events")
            .iter()
            .filter_map(|payload| {
                serde_json::from_slice::<bcode_tool::ToolPresentationUpdate>(payload).ok()
            })
            .collect::<Vec<_>>();
        assert_eq!(updates.len(), 3);
        assert_eq!(updates[0].schema, "bcode.tool.request.shell.run");
        assert!(
            updates[1..]
                .iter()
                .all(|update| update.schema == SHELL_RUN_SCHEMA)
        );
        assert!(updates.iter().all(|update| {
            update.invocation_id == "call-continuity"
                && update.identity == bcode_tool::ToolPresentationIdentity::Primary
                && update.generation == 0
                && update.retention == bcode_tool::ToolPresentationRetention::RetainLatest
        }));
        assert_eq!(
            updates
                .iter()
                .map(|update| update.revision)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        for update in &updates[1..] {
            assert_eq!(update.payload["arguments"], arguments);
            assert_eq!(update.payload["timeout_ms"], 30_000);
            assert!(update.artifact.is_some());
        }
        assert!(
            updates[2]
                .artifact
                .as_ref()
                .is_some_and(|artifact| artifact.finalized)
        );
    }

    #[test]
    fn shell_recording_publishes_first_output_revision_without_waiting_for_cadence() {
        let events = Mutex::new(Vec::<Vec<u8>>::new());
        let emitter = ServiceEventEmitter::new(
            Some(capture_service_event),
            std::ptr::from_ref(&events).cast_mut().cast(),
        );
        let observer = shell_recording_commit_observer(
            None,
            emitter,
            "live-recording",
            DEFAULT_SHELL_TIMEOUT_MS,
            serde_json::json!({"command": "printf hello"}),
            bcode_plugin_sdk::TransientProgressLimits {
                max_encoded_bytes: bcode_plugin_sdk::DEFAULT_TRANSIENT_PROGRESS_MAX_ENCODED_BYTES,
                min_interval_ms: 60_000,
            },
            bcode_plugin_sdk::ServiceCancellation::default(),
        );
        let path = PathBuf::from("live-recording.bcsr.partial");
        observer(recording::ShellRecordingCommit {
            path: path.clone(),
            committed_bytes: recording::RECORDING_HEADER_AND_START_BYTES,
            finalized: false,
        });
        observer(recording::ShellRecordingCommit {
            path,
            committed_bytes: recording::RECORDING_HEADER_AND_START_BYTES + 1,
            finalized: false,
        });

        let revisions = events
            .lock()
            .expect("events")
            .iter()
            .filter_map(|payload| {
                serde_json::from_slice::<bcode_tool::ToolPresentationUpdate>(payload).ok()
            })
            .filter_map(|update| update.artifact)
            .map(|artifact| artifact.revision)
            .collect::<Vec<_>>();
        assert_eq!(
            revisions,
            vec![
                recording::RECORDING_HEADER_AND_START_BYTES,
                recording::RECORDING_HEADER_AND_START_BYTES + 1,
            ]
        );
    }

    struct CapturedServiceEvent {
        payload: Vec<u8>,
        observed_at: Instant,
    }

    extern "C" fn capture_timed_service_event(
        payload: *const u8,
        payload_len: usize,
        user_data: *mut c_void,
    ) {
        // SAFETY: tests pass a live `Mutex<Vec<CapturedServiceEvent>>` pointer for the entire
        // invocation and the emitter invokes this callback synchronously.
        let events = unsafe { &*(user_data.cast::<Mutex<Vec<CapturedServiceEvent>>>()) };
        // SAFETY: the emitter provides a valid payload pointer and length for this callback.
        let payload = unsafe { std::slice::from_raw_parts(payload, payload_len) };
        events
            .lock()
            .expect("event lock")
            .push(CapturedServiceEvent {
                payload: payload.to_vec(),
                observed_at: Instant::now(),
            });
    }

    struct TestResizeInputState {
        next: std::sync::atomic::AtomicUsize,
    }

    extern "C" fn test_resize_input_bridge(
        request_ptr: *const u8,
        request_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
        user_data: *mut c_void,
    ) -> i32 {
        // SAFETY: the test keeps this state alive through the terminal invocation.
        let state = unsafe { &*user_data.cast::<TestResizeInputState>() };
        // SAFETY: the SDK supplies this callback with a valid request buffer.
        let request = unsafe { std::slice::from_raw_parts(request_ptr, request_len) };
        let request: ServiceBridgeRequest =
            serde_json::from_slice(request).expect("input bridge request");
        let ServiceBridgeRequest::ReceiveInput {
            invocation_id,
            timeout_ms: _,
        } = request
        else {
            panic!("expected input request");
        };
        assert_eq!(invocation_id, "test-active-resize");
        let index = state.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if index == 0 {
            std::thread::sleep(Duration::from_millis(40));
        }
        let response = if index < 2 {
            let (columns, rows) = if index == 0 { (100, 30) } else { (132, 40) };
            ServiceBridgeResponse::Input(bcode_tool::ToolInvocationInputResolution::Received {
                input: bcode_tool::ToolInvocationInput {
                    invocation_id,
                    input_id: format!("resize-{index}"),
                    producer_id: "bcode.shell".to_string(),
                    schema: SHELL_INVOCATION_INPUT_SCHEMA.to_owned(),
                    schema_version: SHELL_SCHEMA_VERSION,
                    payload: json!({"type":"resize","columns":columns,"rows":rows}),
                },
            })
        } else {
            ServiceBridgeResponse::Input(bcode_tool::ToolInvocationInputResolution::Closed)
        };
        let encoded = serde_json::to_vec(&response).expect("input bridge response");
        assert!(encoded.len() <= output_capacity);
        // SAFETY: the SDK supplies a response buffer with `output_capacity` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
            *output_len = encoded.len();
        }
        bcode_plugin_sdk::SERVICE_BRIDGE_STATUS_OK
    }
    fn isolated_config_environment(name: &str) -> bcode_config::ConfigEnvironmentSnapshot {
        let root = std::env::temp_dir().join(format!(
            "bcode-shell-plugin-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time should be after epoch")
                .as_nanos()
        ));
        bcode_config::ConfigEnvironmentSnapshot::isolated(root)
    }

    fn shell_result_from_artifact(response: &ToolInvocationResponse) -> Option<ShellRunResult> {
        let Some(ToolInvocationResult::Artifact { artifact }) = &response.result else {
            return None;
        };
        if artifact.schema != SHELL_RUN_SCHEMA {
            return None;
        }
        serde_json::from_value(artifact.metadata.clone()).ok()
    }

    fn test_limited_output() -> LimitedOutput {
        LimitedOutput {
            text: String::new(),
            original_bytes: 12,
            retained_bytes: 12,
            truncated: false,
        }
    }

    #[cfg(unix)]
    #[test]
    fn clean_artifact_ref_uses_encoded_file_uri() {
        let output = test_limited_output();
        let reference = clean_artifact_ref(Path::new("/tmp/bcode shell #output%?.txt"), &output);

        assert_eq!(
            reference.storage_uri.as_deref(),
            Some("file:///tmp/bcode%20shell%20%23output%25%3F.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn clean_artifact_ref_file_uri_round_trips_unicode_path() {
        let path = Path::new("/tmp/bcode café output.txt");
        let reference = clean_artifact_ref(path, &test_limited_output());
        let uri = reference
            .storage_uri
            .as_deref()
            .and_then(|value| url::Url::parse(value).ok())
            .expect("file uri should parse");

        assert_eq!(uri.scheme(), "file");
        assert_eq!(uri.to_file_path().expect("uri should become path"), path);
    }

    #[test]
    fn clean_artifact_ref_omits_storage_uri_for_relative_path() {
        let reference = clean_artifact_ref(
            Path::new("relative/path with spaces.txt"),
            &test_limited_output(),
        );

        assert_eq!(reference.storage_uri, None);
        assert_eq!(reference.key, "clean_output");
        assert_eq!(reference.byte_len, Some(12));
    }

    #[test]
    fn raw_artifact_ref_records_terminal_replay_metadata() {
        let reference = raw_artifact_ref(
            Path::new("/tmp/raw-pty.txt"),
            &test_limited_output(),
            80,
            24,
        );

        assert_eq!(reference.key, TERMINAL_PTY_STREAM_REF_KEY);
        assert_eq!(
            reference.content_type.as_deref(),
            Some(TERMINAL_PTY_STREAM_CONTENT_TYPE)
        );
        assert_eq!(reference.byte_len, Some(12));
        let metadata = reference.metadata.expect("metadata should exist");
        assert_eq!(metadata["stream"], "pty");
        assert_eq!(metadata["columns"], 80);
        assert_eq!(metadata["rows"], 24);
    }

    #[test]
    fn shell_run_schema_does_not_expose_terminal_toggle() {
        let request = ServiceRequest {
            interface_id: TOOL_SERVICE_INTERFACE_ID.to_string(),
            operation: OP_LIST_TOOLS.to_string(),
            payload: serde_json::to_vec(&ListToolsRequest::default())
                .expect("request should encode"),
        };
        let response = list_tools(&request);
        assert!(response.error.is_none());
        let tools = response
            .payload_json::<ToolList>()
            .expect("tool list should decode");
        let shell_run = tools
            .tools
            .iter()
            .find(|tool| tool.name == "shell.run")
            .expect("shell.run tool should be listed");
        let properties = shell_run
            .input_schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("schema should have object properties");

        assert!(!properties.contains_key("terminal"));
        assert!(shell_run.description.contains("non-interactive"));
        assert!(shell_run.description.contains("git --no-pager"));
    }

    #[cfg(unix)]
    #[test]
    fn timeout_terminates_shell_process_group() {
        let environment = isolated_config_environment("timeout");
        let started = Instant::now();
        let response = run_terminal_shell_command_with_environment(
            ServiceEventEmitter::default(),
            &bcode_plugin_sdk::ServiceCancellation::default(),
            bcode_plugin_sdk::TransientProgressLimits::default(),
            "test",
            &ShellRunArguments {
                command: "sh -c 'trap \"\" HUP TERM; sleep 5' | cat".to_string(),
                cwd: None,
                timeout_ms: Some(100),
                columns: None,
                rows: None,
                format_commands: None,
            },
            json!({}),
            TerminalRunPaths {
                session_cwd: None,
                artifact_dir: None,
                input_bridge: None,
                primary_presentation: None,
            },
            &environment,
        );

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(response.is_error);
        assert!(response.output.contains("\"timed_out\":true"));
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_terminates_shell_process_group() {
        let environment = isolated_config_environment("cancellation-process-group");
        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        let cancel = cancellation.clone();
        let cancel_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            cancel.cancel();
        });
        let started = Instant::now();
        let response = run_terminal_shell_command_with_environment(
            ServiceEventEmitter::default(),
            &cancellation,
            bcode_plugin_sdk::TransientProgressLimits::default(),
            "test-cancellation-process-group",
            &ShellRunArguments {
                command: "sh -c 'trap \"\" HUP TERM; sleep 5' | cat".to_string(),
                cwd: None,
                timeout_ms: Some(5_000),
                columns: None,
                rows: None,
                format_commands: None,
            },
            json!({}),
            TerminalRunPaths {
                session_cwd: None,
                artifact_dir: None,
                input_bridge: None,
                primary_presentation: None,
            },
            &environment,
        );
        cancel_thread.join().expect("cancellation thread");

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(response.is_error);
        assert!(response.output.contains("\"cancelled\":true"));
        assert!(!response.output.contains("\"timed_out\":true"));
    }
    #[cfg(windows)]
    #[test]
    fn cancellation_terminates_windows_terminal_child_promptly() {
        let environment = isolated_config_environment("windows-cancellation");
        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        let cancel = cancellation.clone();
        let cancel_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            cancel.cancel();
        });
        let started = Instant::now();
        let response = run_terminal_shell_command_with_environment(
            ServiceEventEmitter::default(),
            &cancellation,
            bcode_plugin_sdk::TransientProgressLimits::default(),
            "test-windows-cancellation",
            &ShellRunArguments {
                command: "ping -n 6 127.0.0.1 >NUL".to_string(),
                cwd: None,
                timeout_ms: Some(5_000),
                columns: None,
                rows: None,
                format_commands: None,
            },
            json!({}),
            TerminalRunPaths {
                session_cwd: None,
                artifact_dir: None,
                input_bridge: None,
                primary_presentation: None,
            },
            &environment,
        );
        cancel_thread.join().expect("cancellation thread");

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(response.is_error);
        assert!(response.output.contains("\"cancelled\":true"));
        assert!(!response.output.contains("\"timed_out\":true"));
    }

    #[test]
    fn limit_output_bytes_truncates_at_utf8_boundary() {
        let output = limit_output_bytes("abcé".as_bytes(), 4);

        assert_eq!(output.text, "abc");
        assert_eq!(output.original_bytes, 5);
        assert_eq!(output.retained_bytes, 3);
        assert!(output.truncated);
    }

    #[cfg(unix)]
    #[test]
    fn shell_pipeline_preserves_failing_left_side_status() {
        let environment = isolated_config_environment("pipeline");
        let response = run_terminal_shell_command_with_environment(
            ServiceEventEmitter::default(),
            &bcode_plugin_sdk::ServiceCancellation::default(),
            bcode_plugin_sdk::TransientProgressLimits::default(),
            "test",
            &ShellRunArguments {
                command: "false | sed -n '1,1p'".to_string(),
                cwd: None,
                timeout_ms: Some(1_000),
                columns: None,
                rows: None,
                format_commands: None,
            },
            json!({}),
            TerminalRunPaths {
                session_cwd: None,
                artifact_dir: None,
                input_bridge: None,
                primary_presentation: None,
            },
            &environment,
        );

        assert!(response.is_error);
        assert!(response.output.contains("\"exit_code\":1"));
    }

    #[cfg(unix)]
    #[test]
    fn active_terminal_control_resize_reaches_pty_and_recording() {
        let environment = isolated_config_environment("active-resize-recording");
        let artifact_dir = tempfile::tempdir().expect("artifact dir");
        let input_state = TestResizeInputState {
            next: std::sync::atomic::AtomicUsize::new(0),
        };
        let bridge = ServiceBridge::new(
            Some(test_resize_input_bridge),
            std::ptr::from_ref(&input_state).cast_mut().cast(),
            bcode_plugin_sdk::ServiceCancellation::default(),
        );
        let response = run_terminal_shell_command_with_environment(
            ServiceEventEmitter::default(),
            &bcode_plugin_sdk::ServiceCancellation::default(),
            bcode_plugin_sdk::TransientProgressLimits::default(),
            "test-active-resize",
            &ShellRunArguments {
                command: "sleep 0.15; printf 'resized\\n'".to_owned(),
                cwd: None,
                timeout_ms: Some(5_000),
                columns: Some(80),
                rows: Some(24),
                format_commands: None,
            },
            json!({}),
            TerminalRunPaths {
                session_cwd: None,
                artifact_dir: Some(artifact_dir.path()),
                input_bridge: Some(&bridge),
                primary_presentation: None,
            },
            &environment,
        );
        assert!(!response.is_error, "{}", response.output);
        let Some(ToolInvocationResult::Artifact { artifact }) = response.result else {
            panic!("expected artifact");
        };
        let recording = artifact
            .refs
            .iter()
            .find(|reference| reference.key == SHELL_RECORDING_REF_KEY)
            .expect("recording reference");
        let path = url::Url::parse(recording.storage_uri.as_deref().expect("recording URI"))
            .expect("recording URL")
            .to_file_path()
            .expect("recording path");
        let (_, frames) = recording::read_recording(&path).expect("valid recording");
        let recorded_resizes = frames
            .iter()
            .filter_map(|frame| match frame {
                recording::ShellRecordingFrame::Resize { columns, rows, .. } => {
                    Some((*columns, *rows))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(recorded_resizes, vec![(100, 30), (132, 40)]);
        let final_output: serde_json::Value =
            serde_json::from_str(&response.output).expect("terminal response JSON");
        assert_eq!(final_output["columns"], 132);
        assert_eq!(final_output["rows"], 40);
    }

    #[cfg(unix)]
    #[test]
    fn large_terminal_recording_keeps_semantic_response_bounded() {
        const COMPLETE_BYTES: u64 = 128 * 1024;
        let environment = isolated_config_environment("bounded-large-terminal");
        let artifact_dir = tempfile::tempdir().expect("artifact dir");
        let response = run_terminal_shell_command_with_environment(
            ServiceEventEmitter::default(),
            &bcode_plugin_sdk::ServiceCancellation::default(),
            bcode_plugin_sdk::TransientProgressLimits::default(),
            "test-bounded-large-terminal",
            &ShellRunArguments {
                command: "head -c 131072 /dev/zero | tr '\\0' x".to_owned(),
                cwd: None,
                timeout_ms: Some(60_000),
                columns: Some(80),
                rows: Some(24),
                format_commands: None,
            },
            json!({}),
            TerminalRunPaths {
                session_cwd: None,
                artifact_dir: Some(artifact_dir.path()),
                input_bridge: None,
                primary_presentation: None,
            },
            &environment,
        );
        assert!(!response.is_error, "large terminal command failed");
        assert!(response.output.len() <= MAX_INLINE_TERMINAL_OUTPUT_BYTES + 1_024);
        assert!(
            response
                .full_output
                .as_ref()
                .is_some_and(|output| output.len() <= MAX_INLINE_TERMINAL_OUTPUT_BYTES + 1_024)
        );
        let Some(ToolInvocationResult::Artifact { artifact }) = response.result else {
            panic!("expected shell artifact");
        };
        let recording = artifact
            .refs
            .iter()
            .find(|reference| reference.key == SHELL_RECORDING_REF_KEY)
            .expect("recording reference");
        assert_eq!(
            recording
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("output_bytes"))
                .and_then(serde_json::Value::as_u64),
            Some(COMPLETE_BYTES)
        );
        let path = url::Url::parse(recording.storage_uri.as_deref().expect("recording URI"))
            .expect("recording URL")
            .to_file_path()
            .expect("recording path");
        let (summary, _) = recording::read_recording(&path).expect("valid recording");
        assert_eq!(summary.output_bytes, COMPLETE_BYTES);
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::too_many_lines)] // One invocation verifies the full recording artifact lifecycle.
    fn terminal_invocation_publishes_one_valid_authoritative_recording() {
        let environment = isolated_config_environment("recording-integration");
        let artifact_dir = tempfile::tempdir().expect("artifact dir");
        let events = Mutex::new(Vec::<Vec<u8>>::new());
        let emitter = ServiceEventEmitter::new(
            Some(capture_service_event),
            std::ptr::from_ref(&events).cast_mut().cast(),
        );
        let response = run_terminal_shell_command_with_environment(
            emitter,
            &bcode_plugin_sdk::ServiceCancellation::default(),
            bcode_plugin_sdk::TransientProgressLimits::default(),
            "test-recording",
            &ShellRunArguments {
                command: "printf 'recorded output\\n'".to_owned(),
                cwd: None,
                timeout_ms: Some(5_000),
                columns: Some(80),
                rows: Some(24),
                format_commands: None,
            },
            json!({}),
            TerminalRunPaths {
                session_cwd: None,
                artifact_dir: Some(artifact_dir.path()),
                input_bridge: None,
                primary_presentation: None,
            },
            &environment,
        );
        assert!(!response.is_error, "{}", response.output);
        let Some(ToolInvocationResult::Artifact { artifact }) = &response.result else {
            panic!("expected artifact");
        };
        let recordings = artifact
            .refs
            .iter()
            .filter(|reference| reference.key == SHELL_RECORDING_REF_KEY)
            .collect::<Vec<_>>();
        assert_eq!(recordings.len(), 1);
        assert_eq!(
            recordings[0].content_type.as_deref(),
            Some(SHELL_RECORDING_CONTENT_TYPE)
        );
        assert_eq!(
            recordings[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("format_version"))
                .and_then(serde_json::Value::as_u64),
            Some(3)
        );
        let uri = recordings[0].storage_uri.as_deref().expect("recording URI");
        let path = url::Url::parse(uri)
            .expect("recording URL")
            .to_file_path()
            .expect("recording path");
        let (summary, frames) = recording::read_recording(&path).expect("valid recording");
        assert_eq!(summary.columns, 80);
        assert_eq!(summary.rows, 24);
        assert!(summary.output_bytes >= 16);
        assert!(frames.iter().any(|frame| matches!(
            frame,
            recording::ShellRecordingFrame::Finish {
                exit_code: Some(0),
                timed_out: false,
                cancelled: false,
                ..
            }
        )));
        assert!(!path.with_extension("shell-recording.partial").exists());
        let artifact_updates = events
            .lock()
            .expect("events")
            .iter()
            .filter_map(|payload| {
                serde_json::from_slice::<bcode_tool::ToolPresentationUpdate>(payload).ok()
            })
            .filter_map(|update| update.artifact)
            .map(|artifact| {
                (
                    artifact.committed_bytes,
                    artifact.revision,
                    artifact.finalized,
                    artifact.storage_uri,
                )
            })
            .collect::<Vec<_>>();
        assert!(!artifact_updates.is_empty());
        assert!(
            artifact_updates
                .windows(2)
                .all(|window| { window[1].0 >= window[0].0 && window[1].1 > window[0].1 })
        );
        assert!(artifact_updates.last().expect("final update").2);
        assert_eq!(
            url::Url::parse(&artifact_updates.last().expect("final update").3)
                .expect("final update URL")
                .to_file_path()
                .expect("final update path"),
            path
        );
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::too_many_lines)] // One lifecycle matrix shares the full invocation/reopen path.
    fn terminal_recordings_preserve_timeout_cancellation_and_nonzero_status() {
        let environment = isolated_config_environment("recording-terminal-status");
        for (
            name,
            command,
            timeout_ms,
            cancel,
            expected_exit,
            expected_signal,
            timed_out,
            cancelled,
        ) in [
            (
                "nonzero",
                "exit 7",
                5_000,
                false,
                Some(7),
                None,
                false,
                false,
            ),
            (
                "signal",
                "kill -TERM $$",
                5_000,
                false,
                Some(1),
                Some("Terminated: 15"),
                false,
                false,
            ),
            (
                "timeout",
                "sleep 10",
                0,
                false,
                Some(1),
                Some("Hangup: 1"),
                true,
                false,
            ),
            (
                "cancel",
                "sleep 10",
                5_000,
                true,
                Some(1),
                Some("Hangup: 1"),
                false,
                true,
            ),
        ] {
            let artifact_dir = tempfile::tempdir().expect("artifact dir");
            let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
            if cancel {
                cancellation.cancel();
            }
            let response = run_terminal_shell_command_with_environment(
                ServiceEventEmitter::default(),
                &cancellation,
                bcode_plugin_sdk::TransientProgressLimits::default(),
                name,
                &ShellRunArguments {
                    command: command.to_owned(),
                    cwd: None,
                    timeout_ms: Some(timeout_ms),
                    columns: Some(80),
                    rows: Some(24),
                    format_commands: None,
                },
                json!({}),
                TerminalRunPaths {
                    session_cwd: None,
                    artifact_dir: Some(artifact_dir.path()),
                    input_bridge: None,
                    primary_presentation: None,
                },
                &environment,
            );
            let Some(ToolInvocationResult::Artifact { artifact }) = response.result else {
                panic!("{name}: expected artifact: {}", response.output);
            };
            let recording = artifact
                .refs
                .iter()
                .find(|reference| reference.key == SHELL_RECORDING_REF_KEY)
                .expect("recording reference");
            let path = url::Url::parse(
                recording
                    .storage_uri
                    .as_deref()
                    .expect("recording storage URI"),
            )
            .expect("recording URL")
            .to_file_path()
            .expect("recording path");
            let (_, frames) = recording::read_recording(&path).expect("valid recording");
            assert!(
                frames.iter().any(|frame| matches!(
                    frame,
                    recording::ShellRecordingFrame::Finish {
                        exit_code,
                        signal,
                        timed_out: actual_timed_out,
                        cancelled: actual_cancelled,
                        ..
                    } if *exit_code == expected_exit
                        && signal.as_deref() == expected_signal
                        && *actual_timed_out == timed_out
                        && *actual_cancelled == cancelled
                )),
                "{name}: {frames:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn terminal_mode_returns_semantic_terminal_result() {
        let environment = isolated_config_environment("terminal");
        let response = run_terminal_shell_command_with_environment(
            ServiceEventEmitter::default(),
            &bcode_plugin_sdk::ServiceCancellation::default(),
            bcode_plugin_sdk::TransientProgressLimits::default(),
            "test-terminal-semantic",
            &ShellRunArguments {
                command: "printf 'semantic terminal\\n'".to_string(),
                cwd: None,
                timeout_ms: Some(5_000),
                columns: Some(80),
                rows: Some(24),
                format_commands: None,
            },
            json!({}),
            TerminalRunPaths {
                session_cwd: None,
                artifact_dir: None,
                input_bridge: None,
                primary_presentation: None,
            },
            &environment,
        );

        assert!(!response.is_error, "{}", response.output);
        let ShellRunResult::Terminal {
            exit_code,
            timed_out,
            cancelled,
            output_tail,
            columns,
            rows,
            ..
        } = shell_result_from_artifact(&response).expect("expected shell artifact")
        else {
            panic!("expected semantic terminal shell result");
        };
        assert_eq!(exit_code, Some(0));
        assert!(!timed_out);
        assert!(!cancelled);
        assert!(output_tail.contains("semantic terminal"));
        assert_eq!(columns, 80);
        assert_eq!(rows, 24);
    }

    #[cfg(windows)]
    #[test]
    fn terminal_mode_executes_cmd_through_native_pty() {
        let environment = isolated_config_environment("windows-terminal");
        let response = run_terminal_shell_command_with_environment(
            ServiceEventEmitter::default(),
            &bcode_plugin_sdk::ServiceCancellation::default(),
            bcode_plugin_sdk::TransientProgressLimits::default(),
            "test-windows-terminal-semantic",
            &ShellRunArguments {
                command: "echo semantic windows terminal".to_string(),
                cwd: None,
                timeout_ms: Some(5_000),
                columns: Some(80),
                rows: Some(24),
                format_commands: None,
            },
            json!({}),
            TerminalRunPaths {
                session_cwd: None,
                artifact_dir: None,
                input_bridge: None,
                primary_presentation: None,
            },
            &environment,
        );

        assert!(!response.is_error, "{}", response.output);
        let ShellRunResult::Terminal {
            exit_code,
            timed_out,
            cancelled,
            output_tail,
            ..
        } = shell_result_from_artifact(&response).expect("expected shell artifact")
        else {
            panic!("expected semantic terminal shell result");
        };
        assert_eq!(exit_code, Some(0));
        assert!(!timed_out);
        assert!(!cancelled);
        assert!(output_tail.contains("semantic windows terminal"));
    }

    #[cfg(unix)]
    #[test]
    fn terminal_mode_preserves_ansi_output() {
        let response = run_terminal_shell_command(
            ServiceEventEmitter::default(),
            &bcode_plugin_sdk::ServiceCancellation::default(),
            bcode_plugin_sdk::TransientProgressLimits::default(),
            "test-terminal-ansi",
            &ShellRunArguments {
                command: "printf '\\033[31mred\\033[0m\\n'".to_string(),
                cwd: None,
                timeout_ms: Some(5_000),
                columns: Some(80),
                rows: Some(24),
                format_commands: None,
            },
            json!({}),
            TerminalRunPaths {
                session_cwd: None,
                artifact_dir: None,
                input_bridge: None,
                primary_presentation: None,
            },
        );

        assert!(!response.is_error, "{}", response.output);
        let ShellRunResult::Terminal { output_tail, .. } =
            shell_result_from_artifact(&response).expect("expected shell artifact")
        else {
            panic!("expected semantic terminal shell result");
        };
        assert!(output_tail.contains("\u{1b}[31mred\u{1b}[0m"));
    }

    #[test]
    fn terminal_output_encoding_returns_inline_tail() {
        let output = LimitedOutput {
            text: "hello".to_string(),
            original_bytes: 5,
            retained_bytes: 5,
            truncated: false,
        };
        let (_encoded, full_encoded, inline_output) = encode_terminal_output(
            "printf hello",
            None,
            &TerminalShellStatus {
                exit_code: 0,
                signal: None,
                success: true,
                timed_out: false,
                cancelled: false,
            },
            &output,
            80,
            24,
        )
        .expect("terminal output encodes");

        assert_eq!(inline_output.text, "hello");
        assert_eq!(inline_output.original_bytes, 5);
        assert_eq!(inline_output.retained_bytes, 5);
        assert!(!inline_output.truncated);
        assert!(full_encoded.contains("hello"));
    }

    #[test]
    fn terminal_result_tail_marks_truncation_and_byte_counts() {
        let output = LimitedOutput {
            text: format!("{}tail", "x".repeat(MAX_INLINE_TERMINAL_OUTPUT_BYTES + 128)),
            original_bytes: MAX_INLINE_TERMINAL_OUTPUT_BYTES + 132,
            retained_bytes: MAX_INLINE_TERMINAL_OUTPUT_BYTES + 132,
            truncated: false,
        };

        let limited = limit_terminal_inline_output(&output);

        assert!(limited.truncated);
        assert_eq!(limited.original_bytes, output.original_bytes);
        assert!(limited.retained_bytes <= MAX_INLINE_TERMINAL_OUTPUT_BYTES);
        assert!(limited.text.ends_with("tail"));
    }

    #[test]
    fn terminal_final_output_is_smaller_tail() {
        let output = LimitedOutput {
            text: format!("{}tail", "x".repeat(MAX_INLINE_TERMINAL_OUTPUT_BYTES + 128)),
            original_bytes: MAX_INLINE_TERMINAL_OUTPUT_BYTES + 132,
            retained_bytes: MAX_INLINE_TERMINAL_OUTPUT_BYTES + 132,
            truncated: false,
        };

        let limited = limit_terminal_inline_output(&output);

        assert!(limited.truncated);
        assert!(limited.retained_bytes <= MAX_INLINE_TERMINAL_OUTPUT_BYTES);
        assert!(limited.text.ends_with("tail"));
    }

    #[test]
    fn prelude_gate_suppresses_until_marker() {
        let mut filter = PreludeGate::new(vec!["__MARK__".to_string()]);

        assert!(filter.write(b"direnv: loading\n").is_empty());
        assert_eq!(filter.write(b"__MARK__\nhello\n"), b"hello\n");
        assert_eq!(filter.write(b"world\n"), b"world\n");
        assert!(filter.finish().is_empty());
    }

    #[test]
    fn prelude_gate_handles_split_marker() {
        let mut filter = PreludeGate::new(vec!["__MARK__".to_string()]);

        assert!(filter.write(b"noise\n__MA").is_empty());
        assert_eq!(filter.write(b"RK__\r\noutput"), b"output");
    }

    #[test]
    fn prelude_gate_preserves_output_without_marker() {
        let mut filter = PreludeGate::new(vec!["__MARK__".to_string()]);

        assert!(filter.write(b"direnv error\n").is_empty());
        assert_eq!(filter.finish(), b"direnv error\n");
    }

    #[test]
    fn prelude_gate_passes_through_when_disabled() {
        let mut filter = PreludeGate::new(Vec::new());

        assert_eq!(filter.write(b"hello"), b"hello");
        assert!(filter.finish().is_empty());
    }

    #[test]
    fn prelude_gate_uses_earliest_generic_marker() {
        let mut filter = PreludeGate::new(vec!["__SECOND__".to_string(), "__FIRST__".to_string()]);

        assert_eq!(
            filter.write(b"noise\n__FIRST__\noutput\n__SECOND__\n"),
            b"output\n__SECOND__\n"
        );
    }

    #[test]
    fn output_config_builds_enabled_prelude_markers() {
        let markers = prelude_markers_from_output_config(&ShellToolOutputConfig {
            prelude_gates: vec![
                bcode_config::ShellToolPreludeGateConfig {
                    name: "enabled".to_string(),
                    marker: "__READY__".to_string(),
                    enabled: true,
                    ..bcode_config::ShellToolPreludeGateConfig::default()
                },
                bcode_config::ShellToolPreludeGateConfig {
                    name: "disabled".to_string(),
                    marker: "__IGNORED__".to_string(),
                    enabled: false,
                    ..bcode_config::ShellToolPreludeGateConfig::default()
                },
            ],
            ..ShellToolOutputConfig::default()
        });

        assert_eq!(markers.live, vec!["__READY__".to_string()]);
        assert_eq!(markers.replay, vec!["__READY__".to_string()]);
        assert_eq!(markers.clean, vec!["__READY__".to_string()]);
    }

    #[test]
    #[ignore = "manual release benchmark"]
    fn benchmark_live_stream_recording_overhead() {
        const BYTES: usize = 4 * 1024 * 1024;
        const ROUNDS: usize = 9;
        let input = vec![b'x'; BYTES];
        let context = ShellVisualStreamContext {
            columns: 120,
            rows: 30,
            timeout_ms: DEFAULT_SHELL_TIMEOUT_MS,
            arguments: serde_json::Value::Null,
            primary_presentation: None,
            prelude_markers: PreludeGateMarkers::default(),
            progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
            cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
        };
        let mut baseline = Vec::with_capacity(ROUNDS);
        let mut recorded = Vec::with_capacity(ROUNDS);
        let dir = tempfile::tempdir().expect("temp dir");
        for round in 0..ROUNDS {
            let measure = |recording: Option<PathBuf>| {
                let started = Instant::now();
                let mut output = read_limited_streaming(
                    std::io::Cursor::new(&input),
                    ServiceEventEmitter::default(),
                    "benchmark-call",
                    &context,
                    TerminalStreamPaths {
                        clean: None,
                        raw: None,
                        replay: None,
                        recording,
                        recording_ready: None,
                    },
                )
                .expect("stream benchmark");
                let elapsed = started.elapsed().as_nanos();
                if let Some(writer) = output.recording_writer.take() {
                    writer
                        .finish(1, Some(0), None, false, false)
                        .expect("recording finalization");
                }
                elapsed
            };
            let recording = Some(dir.path().join(format!("recording-{round}.bcsr")));
            if round % 2 == 0 {
                baseline.push(measure(None));
                recorded.push(measure(recording));
            } else {
                recorded.push(measure(recording));
                baseline.push(measure(None));
            }
        }
        baseline.sort_unstable();
        recorded.sort_unstable();
        let baseline = baseline[ROUNDS / 2];
        let recorded = recorded[ROUNDS / 2];
        let overhead = recorded.saturating_sub(baseline).saturating_mul(10_000) / baseline;
        eprintln!(
            "shell live stream benchmark ({ROUNDS} median rounds x {BYTES} bytes): baseline={} ns/byte, recorded={} ns/byte, overhead={}.{:02}%",
            baseline / BYTES as u128,
            recorded / BYTES as u128,
            overhead / 100,
            overhead % 100,
        );
    }

    #[derive(Debug)]
    struct PendingTerminalChild {
        killed: bool,
    }

    impl portable_pty::ChildKiller for PendingTerminalChild {
        fn kill(&mut self) -> std::io::Result<()> {
            self.killed = true;
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(Self {
                killed: self.killed,
            })
        }
    }

    impl portable_pty::Child for PendingTerminalChild {
        fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
            Ok(self
                .killed
                .then(|| portable_pty::ExitStatus::with_signal("killed")))
        }

        fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
            Ok(portable_pty::ExitStatus::with_signal("killed"))
        }

        fn process_id(&self) -> Option<u32> {
            None
        }

        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            None
        }
    }

    #[test]
    fn terminal_wait_cancels_and_kills_promptly() {
        let mut child: Box<dyn portable_pty::Child + Send + Sync> =
            Box::new(PendingTerminalChild { killed: false });
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let cancellation = bcode_plugin_sdk::ServiceCancellation::new(cancelled);
        let started = Instant::now();
        let status = wait_for_terminal_shell_status(
            &mut child,
            &cancellation,
            Duration::from_secs(10),
            "cancel-test",
            ServiceEventEmitter::default(),
            None,
            None,
        )
        .expect("cancelled child status");

        assert!(status.cancelled);
        assert!(!status.timed_out);
        assert!(!status.success);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn terminal_wait_times_out_kills_and_reports_status_promptly() {
        let mut child: Box<dyn portable_pty::Child + Send + Sync> =
            Box::new(PendingTerminalChild { killed: false });
        let started = Instant::now();
        let status = wait_for_terminal_shell_status(
            &mut child,
            &bcode_plugin_sdk::ServiceCancellation::default(),
            Duration::ZERO,
            "timeout-test",
            ServiceEventEmitter::default(),
            None,
            None,
        )
        .expect("timed-out child status");

        assert!(status.timed_out);
        assert!(!status.cancelled);
        assert!(!status.success);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    struct FixedChunkReader {
        remaining_bytes: usize,
        chunk: Vec<u8>,
    }

    impl Read for FixedChunkReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.remaining_bytes == 0 {
                return Ok(0);
            }
            let len = self.chunk.len().min(buffer.len()).min(self.remaining_bytes);
            buffer[..len].copy_from_slice(&self.chunk[..len]);
            self.remaining_bytes = self.remaining_bytes.saturating_sub(len);
            Ok(len)
        }
    }

    #[derive(Debug)]
    struct PublicationWorkloadMetrics {
        publication_count: usize,
        average_committed_delta: u64,
        maximum_committed_delta: u64,
        maximum_interarrival_us: u64,
        ipc_bytes: usize,
    }

    fn publication_workload_metrics(events: &[CapturedServiceEvent]) -> PublicationWorkloadMetrics {
        let mut previous_bytes = 0_u64;
        let mut committed_deltas = Vec::new();
        let mut ipc_bytes = 0_usize;
        for event in events {
            ipc_bytes = ipc_bytes.saturating_add(event.payload.len());
            let update: bcode_tool::ToolPresentationUpdate =
                serde_json::from_slice(&event.payload).expect("artifact presentation update");
            assert_eq!(
                update.identity,
                bcode_tool::ToolPresentationIdentity::Primary
            );
            let artifact = update.artifact.expect("artifact revision");
            let delta = artifact.committed_bytes.saturating_sub(previous_bytes);
            previous_bytes = artifact.committed_bytes;
            if !artifact.finalized {
                committed_deltas.push(delta);
            }
        }
        PublicationWorkloadMetrics {
            publication_count: events.len(),
            average_committed_delta: committed_deltas
                .iter()
                .copied()
                .sum::<u64>()
                .checked_div(u64::try_from(committed_deltas.len()).unwrap_or(u64::MAX))
                .unwrap_or(0),
            maximum_committed_delta: committed_deltas.iter().copied().max().unwrap_or(0),
            maximum_interarrival_us: events
                .windows(2)
                .map(|window| {
                    u64::try_from(
                        window[1]
                            .observed_at
                            .saturating_duration_since(window[0].observed_at)
                            .as_micros(),
                    )
                    .unwrap_or(u64::MAX)
                })
                .max()
                .unwrap_or(0),
            ipc_bytes,
        }
    }

    #[derive(Debug)]
    struct ChunkWorkloadMetrics {
        raw_bytes: usize,
        recording_bytes: u64,
        raw_update_count: usize,
        publication_count: usize,
        average_committed_delta: u64,
        maximum_committed_delta: u64,
        maximum_interarrival_us: u64,
        ipc_bytes: usize,
        wall_time: Duration,
    }

    fn run_output_chunk_workload(output_bytes: usize, chunk_bytes: usize) -> ChunkWorkloadMetrics {
        let chunks = output_bytes.div_ceil(chunk_bytes);
        let started = Instant::now();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("many-chunks.bcsr");
        let events = Mutex::new(Vec::<CapturedServiceEvent>::new());
        let emitter = ServiceEventEmitter::new(
            Some(capture_timed_service_event),
            std::ptr::from_ref(&events).cast_mut().cast(),
        );
        let mut output = read_limited_streaming(
            FixedChunkReader {
                remaining_bytes: output_bytes,
                chunk: vec![b'x'; chunk_bytes],
            },
            emitter,
            "many-chunks",
            &ShellVisualStreamContext {
                columns: 80,
                rows: 24,
                timeout_ms: DEFAULT_SHELL_TIMEOUT_MS,
                arguments: serde_json::Value::Null,
                primary_presentation: None,
                prelude_markers: PreludeGateMarkers::default(),
                progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
                cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
            },
            TerminalStreamPaths {
                clean: None,
                raw: None,
                replay: None,
                recording: Some(path.clone()),
                recording_ready: None,
            },
        )
        .expect("stream chunks");
        output
            .recording_writer
            .take()
            .expect("recording writer")
            .finish(u64::MAX, Some(0), None, false, false)
            .expect("finish recording");

        let events = events.lock().expect("events");
        let publication = publication_workload_metrics(&events);
        drop(events);
        let recording_bytes = std::fs::metadata(&path).expect("recording metadata").len();
        let raw_bytes = output_bytes;
        assert!(publication.publication_count <= chunks.saturating_mul(2).saturating_add(2));
        let (_, frames) = recording::read_recording(&path).expect("recording");
        let exact_output_bytes = frames
            .iter()
            .filter_map(|frame| match frame {
                recording::ShellRecordingFrame::Output { bytes, .. } => Some(bytes.len()),
                _ => None,
            })
            .sum::<usize>();
        assert_eq!(exact_output_bytes, raw_bytes);
        ChunkWorkloadMetrics {
            raw_bytes,
            recording_bytes,
            raw_update_count: chunks,
            publication_count: publication.publication_count,
            average_committed_delta: publication.average_committed_delta,
            maximum_committed_delta: publication.maximum_committed_delta,
            maximum_interarrival_us: publication.maximum_interarrival_us,
            ipc_bytes: publication.ipc_bytes,
            wall_time: started.elapsed(),
        }
    }

    #[test]
    #[ignore = "manual deterministic performance baseline"]
    fn live_shell_output_chunk_baseline_report() {
        const OUTPUT_VOLUMES: [usize; 3] = [64 * 1024, 1024 * 1024, 8 * 1024 * 1024];
        const CHUNK_BYTES: [usize; 3] = [17, 4 * 1024, 16 * 1024];

        for output_bytes in OUTPUT_VOLUMES {
            for chunk_bytes in CHUNK_BYTES {
                let metrics = run_output_chunk_workload(output_bytes, chunk_bytes);
                println!(
                    "BCODE_PERF_CASE {}",
                    serde_json::json!({
                        "domain": "shell_output",
                        "output_bytes": metrics.raw_bytes,
                        "chunk_bytes": chunk_bytes,
                        "recording_bytes": metrics.recording_bytes,
                        "raw_updates": metrics.raw_update_count,
                        "published_updates": metrics.publication_count,
                        "average_committed_delta": metrics.average_committed_delta,
                        "maximum_committed_delta": metrics.maximum_committed_delta,
                        "maximum_interarrival_us": metrics.maximum_interarrival_us,
                        "ipc_bytes": metrics.ipc_bytes,
                        "wall_us": u64::try_from(metrics.wall_time.as_micros()).unwrap_or(u64::MAX),
                    })
                );
            }
        }
    }

    #[test]
    fn deterministic_output_chunk_matrix_preserves_exact_bytes_and_expected_updates() {
        const OUTPUT_VOLUMES: [usize; 3] = [64 * 1024, 1024 * 1024, 8 * 1024 * 1024];
        const CHUNK_BYTES: [usize; 3] = [17, 4 * 1024, 16 * 1024];

        for output_bytes in OUTPUT_VOLUMES {
            for chunk_bytes in CHUNK_BYTES {
                let chunks = output_bytes.div_ceil(chunk_bytes);
                let metrics = run_output_chunk_workload(output_bytes, chunk_bytes);
                assert_eq!(metrics.raw_bytes, output_bytes);
                assert_eq!(metrics.raw_update_count, chunks);
                let timer_publications = usize::try_from(
                    metrics
                        .wall_time
                        .as_millis()
                        .div_ceil(Duration::from_millis(16).as_millis()),
                )
                .unwrap_or(usize::MAX);
                let maximum_publications = usize::try_from(metrics.recording_bytes)
                    .unwrap_or(usize::MAX)
                    .div_ceil(64 * 1024)
                    .saturating_add(timer_publications)
                    .saturating_add(2);
                assert!(metrics.publication_count <= maximum_publications);
                assert!(metrics.recording_bytes >= u64::try_from(output_bytes).unwrap_or(u64::MAX));
                assert!(metrics.ipc_bytes > 0);
            }
        }
    }

    #[test]
    fn thousands_of_small_pty_chunks_emit_only_linear_artifact_notifications() {
        let metrics = run_output_chunk_workload(4_096 * 17, 17);
        assert!(
            metrics.recording_bytes
                <= u64::try_from(metrics.raw_bytes.saturating_mul(6)).unwrap_or(u64::MAX)
        );
        assert!(metrics.ipc_bytes <= metrics.raw_bytes.saturating_mul(64));
    }

    #[test]
    fn live_notification_transport_scales_linearly_with_recording_input() {
        let small = run_output_chunk_workload(512 * 17, 17);
        let large = run_output_chunk_workload(4_096 * 17, 17);
        let input_scale = large.raw_bytes / small.raw_bytes;

        assert_eq!(input_scale, 8);
        assert!(large.raw_update_count <= small.raw_update_count.saturating_mul(input_scale));
        assert!(large.publication_count <= small.publication_count.saturating_mul(input_scale));
        assert!(
            large.ipc_bytes
                <= small
                    .ipc_bytes
                    .saturating_mul(input_scale)
                    .saturating_mul(2)
        );
        assert!(
            large.recording_bytes
                <= small
                    .recording_bytes
                    .saturating_mul(u64::try_from(input_scale).expect("scale"))
                    .saturating_mul(2)
        );
    }

    #[test]
    fn recording_emits_only_bounded_artifact_notifications() {
        let bytes = b"first\rsecond\n\x1b[31mred\x1b[0m\n";
        let context = ShellVisualStreamContext {
            columns: 80,
            rows: 24,
            timeout_ms: DEFAULT_SHELL_TIMEOUT_MS,
            arguments: serde_json::Value::Null,
            primary_presentation: None,
            prelude_markers: PreludeGateMarkers::default(),
            progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
            cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
        };
        let baseline_events = Mutex::new(Vec::<Vec<u8>>::new());
        let baseline_emitter = ServiceEventEmitter::new(
            Some(capture_service_event),
            std::ptr::from_ref(&baseline_events).cast_mut().cast(),
        );
        read_limited_streaming(
            std::io::Cursor::new(bytes),
            baseline_emitter,
            "call",
            &context,
            TerminalStreamPaths {
                clean: None,
                raw: None,
                replay: None,
                recording: None,
                recording_ready: None,
            },
        )
        .expect("baseline stream");

        let dir = tempfile::tempdir().expect("temp dir");
        let recorded_events = Mutex::new(Vec::<Vec<u8>>::new());
        let recorded_emitter = ServiceEventEmitter::new(
            Some(capture_service_event),
            std::ptr::from_ref(&recorded_events).cast_mut().cast(),
        );
        let mut recorded_output = read_limited_streaming(
            std::io::Cursor::new(bytes),
            recorded_emitter,
            "call",
            &context,
            TerminalStreamPaths {
                clean: None,
                raw: None,
                replay: None,
                recording: Some(dir.path().join("recording.bcsr")),
                recording_ready: None,
            },
        )
        .expect("recorded stream");
        recorded_output
            .recording_writer
            .take()
            .expect("recording writer")
            .finish(1, Some(0), None, false, false)
            .expect("finish recording");

        assert!(baseline_events.lock().expect("baseline lock").is_empty());
        let recorded_events = recorded_events.lock().expect("recorded lock");
        assert!(!recorded_events.is_empty());
        assert!(recorded_events.iter().all(|payload| {
            serde_json::from_slice::<bcode_tool::ToolPresentationUpdate>(payload).is_ok_and(
                |update| {
                    update.identity == bcode_tool::ToolPresentationIdentity::Primary
                        && update.artifact.is_some()
                },
            )
        }));
        drop(recorded_events);
    }

    #[cfg(feature = "static-bundled")]
    #[test]
    fn authoritative_recording_replays_the_same_prelude_filtered_bytes_as_live() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("filtered.bcsr");
        let mut output = read_limited_streaming(
            std::io::Cursor::new(b"hidden prelude\n__MARK__\nvisible\n"),
            ServiceEventEmitter::default(),
            "call",
            &ShellVisualStreamContext {
                columns: 80,
                rows: 24,
                timeout_ms: DEFAULT_SHELL_TIMEOUT_MS,
                arguments: serde_json::Value::Null,
                primary_presentation: None,
                prelude_markers: PreludeGateMarkers {
                    live: vec!["__MARK__".to_owned()],
                    replay: vec!["__MARK__".to_owned()],
                    clean: vec!["__MARK__".to_owned()],
                },
                progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
                cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
            },
            TerminalStreamPaths {
                clean: None,
                raw: None,
                replay: None,
                recording: Some(path.clone()),
                recording_ready: None,
            },
        )
        .expect("stream output");
        output
            .recording_writer
            .take()
            .expect("recording writer")
            .finish(u64::MAX, Some(0), None, false, false)
            .expect("finish recording");
        let (summary, frames) = recording::read_recording(&path).expect("read recording");
        let replay = crate::shell_run_tui::decode_recording_replay(&summary, frames);

        assert_eq!(output.replay.text, "visible\n");
        assert_eq!(replay.output, "visible\n");
    }

    #[test]
    fn prelude_gate_config_can_keep_prelude_in_clean_output() {
        let output = read_limited_streaming(
            std::io::Cursor::new(b"prelude\n__MARK__\nhello\n"),
            ServiceEventEmitter::default(),
            "call",
            &ShellVisualStreamContext {
                columns: 80,
                rows: 24,
                timeout_ms: DEFAULT_SHELL_TIMEOUT_MS,
                arguments: serde_json::Value::Null,
                primary_presentation: None,
                prelude_markers: PreludeGateMarkers {
                    live: vec!["__MARK__".to_string()],
                    replay: vec!["__MARK__".to_string()],
                    clean: Vec::new(),
                },
                progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
                cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
            },
            TerminalStreamPaths {
                clean: None,
                raw: None,
                replay: None,
                recording: None,
                recording_ready: None,
            },
        )
        .expect("stream should read");

        assert_eq!(output.replay.text, "hello\n");
        assert_eq!(output.clean.text, "prelude\n__MARK__\nhello\n");
    }

    #[test]
    fn prelude_gate_config_can_keep_prelude_in_replay_output() {
        let output = read_limited_streaming(
            std::io::Cursor::new(b"prelude\n__MARK__\nhello\n"),
            ServiceEventEmitter::default(),
            "call",
            &ShellVisualStreamContext {
                columns: 80,
                rows: 24,
                timeout_ms: DEFAULT_SHELL_TIMEOUT_MS,
                arguments: serde_json::Value::Null,
                primary_presentation: None,
                prelude_markers: PreludeGateMarkers {
                    live: vec!["__MARK__".to_string()],
                    replay: Vec::new(),
                    clean: vec!["__MARK__".to_string()],
                },
                progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
                cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
            },
            TerminalStreamPaths {
                clean: None,
                raw: None,
                replay: None,
                recording: None,
                recording_ready: None,
            },
        )
        .expect("stream should read");

        assert_eq!(output.replay.text, "prelude\n__MARK__\nhello\n");
        assert_eq!(output.clean.text, "hello\n");
    }

    #[test]
    fn terminal_response_uses_replay_pty_artifact_when_direnv_prelude_was_suppressed() {
        let raw = LimitedOutput {
            text: "direnv: loading\n__BCODE_DIRENV_READY_call__\n\u{1b}[31mhello\u{1b}[0m\n"
                .to_string(),
            original_bytes: 61,
            retained_bytes: 61,
            truncated: false,
        };
        let replay = LimitedOutput {
            text: "\u{1b}[31mhello\u{1b}[0m\n".to_string(),
            original_bytes: 15,
            retained_bytes: 15,
            truncated: false,
        };
        let clean = LimitedOutput {
            text: "hello\n".to_string(),
            original_bytes: 6,
            retained_bytes: 6,
            truncated: false,
        };
        let response = terminal_shell_response(
            "call",
            TerminalShellResponseInput {
                arguments: &ShellRunArguments {
                    command: "echo hello".to_string(),
                    cwd: None,
                    timeout_ms: None,
                    columns: Some(80),
                    rows: Some(24),
                    format_commands: None,
                },
                cwd: None,
                status: TerminalShellStatus {
                    exit_code: 0,
                    signal: None,
                    success: true,
                    timed_out: false,
                    cancelled: false,
                },
                started: Instant::now(),
                stream_output: &TerminalStreamOutput {
                    raw,
                    replay,
                    clean,
                    raw_artifact_path: Some(PathBuf::from("/tmp/raw.txt")),
                    replay_artifact_path: Some(PathBuf::from("/tmp/replay.txt")),
                    clean_artifact_path: Some(PathBuf::from("/tmp/clean.txt")),
                    recording_path: None,
                    recording_writer: None,
                    prelude_suppressed: true,
                },
                columns: 80,
                rows: 24,
                format_commands: true,
                recording_ref: None,
            },
        )
        .expect("terminal response should encode");

        let ShellRunResult::Terminal { output_tail, .. } =
            shell_result_from_artifact(&response).expect("expected shell artifact")
        else {
            panic!("expected semantic terminal shell result");
        };
        assert_eq!(output_tail, "\u{1b}[31mhello\u{1b}[0m\n");
        assert!(!output_tail.contains("direnv:"));
        assert!(!output_tail.contains("__BCODE_DIRENV_READY_"));
        let Some(ToolInvocationResult::Artifact { artifact }) = response.result else {
            panic!("expected artifact result");
        };
        assert!(
            artifact
                .refs
                .iter()
                .any(|reference| reference.key == "clean_output")
        );
        let replay_ref = artifact
            .refs
            .iter()
            .find(|reference| reference.key == TERMINAL_PTY_STREAM_REF_KEY)
            .expect("replay pty ref should exist");
        assert_eq!(
            replay_ref.storage_uri.as_deref(),
            Some("file:///tmp/replay.txt")
        );
    }

    #[test]
    fn terminal_response_keeps_raw_artifact_when_direnv_marker_was_absent() {
        let raw = LimitedOutput {
            text: "direnv error\n".to_string(),
            original_bytes: 13,
            retained_bytes: 13,
            truncated: false,
        };
        let replay = raw.clone();
        let clean = raw.clone();
        let response = terminal_shell_response(
            "call",
            TerminalShellResponseInput {
                arguments: &ShellRunArguments {
                    command: "echo hello".to_string(),
                    cwd: None,
                    timeout_ms: None,
                    columns: Some(80),
                    rows: Some(24),
                    format_commands: None,
                },
                cwd: None,
                status: TerminalShellStatus {
                    exit_code: 1,
                    signal: None,
                    success: false,
                    timed_out: false,
                    cancelled: false,
                },
                started: Instant::now(),
                stream_output: &TerminalStreamOutput {
                    raw,
                    replay,
                    clean,
                    raw_artifact_path: Some(PathBuf::from("/tmp/raw.txt")),
                    replay_artifact_path: Some(PathBuf::from("/tmp/replay.txt")),
                    clean_artifact_path: Some(PathBuf::from("/tmp/clean.txt")),
                    recording_path: None,
                    recording_writer: None,
                    prelude_suppressed: false,
                },
                columns: 80,
                rows: 24,
                format_commands: true,
                recording_ref: None,
            },
        )
        .expect("terminal response should encode");

        let Some(ToolInvocationResult::Artifact { artifact }) = response.result else {
            panic!("expected artifact result");
        };
        assert!(
            artifact
                .refs
                .iter()
                .any(|reference| reference.key == TERMINAL_PTY_STREAM_REF_KEY)
        );
    }

    #[test]
    fn direnv_command_plan_uses_prelude_marker_by_default() {
        let plan = direnv_shell_command_plan(
            "echo hello",
            Path::new("/tmp"),
            ShellToolEnvConfig {
                mode: ShellToolEnvMode::Direnv,
                auto_fallback: ShellToolEnvAutoFallback::Error,
                hide_direnv_prelude: true,
            },
            "call-1",
        );

        let marker = plan.prelude_marker.expect("direnv marker should be set");
        assert_eq!(plan.program, "direnv");
        assert!(plan.args.iter().any(|arg| arg.contains(&marker)));
        assert!(plan.args.iter().any(|arg| arg.contains("echo hello")));
    }

    #[test]
    fn direnv_command_plan_can_disable_prelude_marker() {
        let plan = direnv_shell_command_plan(
            "echo hello",
            Path::new("/tmp"),
            ShellToolEnvConfig {
                mode: ShellToolEnvMode::Direnv,
                auto_fallback: ShellToolEnvAutoFallback::Error,
                hide_direnv_prelude: false,
            },
            "call-1",
        );

        assert!(plan.prelude_marker.is_none());
        assert!(plan.args.iter().any(|arg| arg == "echo hello"));
    }
}
