#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled Blims AI company simulator plugin.

use bcode_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::env;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use switchy_database::query::{FilterableQuery as _, SortDirection, where_eq};
use switchy_database::schema::{Column, DataType, create_table};
use switchy_database::{Database, DatabaseValue, Row};
use thiserror::Error;

/// Blims plugin service interface id.
pub const BLIMS_SERVICE_INTERFACE_ID: &str = "bcode.blims/v1";

/// Company status operation.
pub const OP_COMPANY_STATUS: &str = "company.status";

/// Company create operation.
pub const OP_COMPANY_CREATE: &str = "company.create";

/// Company load operation.
pub const OP_COMPANY_LOAD: &str = "company.load";

/// Company pause operation.
pub const OP_COMPANY_PAUSE: &str = "company.pause";

/// Company resume operation.
pub const OP_COMPANY_RESUME: &str = "company.resume";

/// Company shutdown operation.
pub const OP_COMPANY_SHUTDOWN: &str = "company.shutdown";

/// Agent list operation.
pub const OP_AGENT_LIST: &str = "agent.list";

/// Agent inspect operation.
pub const OP_AGENT_INSPECT: &str = "agent.inspect";

/// Agent permission get operation.
pub const OP_AGENT_GET_PERMISSION: &str = "agent.get_permission";

/// Agent permission set operation.
pub const OP_AGENT_SET_PERMISSION: &str = "agent.set_permission";

/// Agent permission escalation request operation.
pub const OP_AGENT_REQUEST_PERMISSION: &str = "agent.request_permission";

/// Agent conversation record operation.
pub const OP_AGENT_RECORD_CONVERSATION: &str = "agent.record_conversation";

/// Agent hire operation.
pub const OP_AGENT_HIRE: &str = "agent.hire";

/// Agent suspend operation.
pub const OP_AGENT_SUSPEND: &str = "agent.suspend";

/// Agent fire operation.
pub const OP_AGENT_FIRE: &str = "agent.fire";

/// Move one agent to a room operation.
pub const OP_AGENT_MOVE: &str = "agent.move";

/// Agent update contract operation.
pub const OP_AGENT_UPDATE_CONTRACT: &str = "agent.update_contract";

/// Initiative create operation.
pub const OP_INITIATIVE_CREATE: &str = "initiative.create";

/// Initiative list operation.
pub const OP_INITIATIVE_LIST: &str = "initiative.list";

/// Initiative inspect operation.
pub const OP_INITIATIVE_INSPECT: &str = "initiative.inspect";

/// Initiative set guidance operation.
pub const OP_INITIATIVE_SET_GUIDANCE: &str = "initiative.set_guidance";

/// Initiative pause operation.
pub const OP_INITIATIVE_PAUSE: &str = "initiative.pause";

/// Initiative resume operation.
pub const OP_INITIATIVE_RESUME: &str = "initiative.resume";

/// Guidance set operation.
pub const OP_GUIDANCE_SET: &str = "guidance.set";

/// Guidance list operation.
pub const OP_GUIDANCE_LIST: &str = "guidance.list";

/// Initiative planning prompt operation.
pub const OP_INITIATIVE_PLAN_PROMPT: &str = "initiative.plan_prompt";

/// Initiative plan import operation.
pub const OP_INITIATIVE_IMPORT_PLAN: &str = "initiative.import_plan";

/// Task list operation.
pub const OP_TASK_LIST: &str = "task.list";

/// Task inspect operation.
pub const OP_TASK_INSPECT: &str = "task.inspect";

/// Task work prompt operation.
pub const OP_TASK_WORK_PROMPT: &str = "task.work_prompt";

/// Artifact list operation.
pub const OP_ARTIFACT_LIST: &str = "artifact.list";

/// Artifact create operation.
pub const OP_ARTIFACT_CREATE: &str = "artifact.create";

/// Artifact inspect operation.
pub const OP_ARTIFACT_INSPECT: &str = "artifact.inspect";

/// Artifact approve operation.
pub const OP_ARTIFACT_APPROVE: &str = "artifact.approve";

/// Artifact reject operation.
pub const OP_ARTIFACT_REJECT: &str = "artifact.reject";

/// Artifact defer operation.
pub const OP_ARTIFACT_DEFER: &str = "artifact.defer";

/// Work proposal register operation.
pub const OP_PROPOSAL_REGISTER: &str = "proposal.register";

/// Work proposal list operation.
pub const OP_PROPOSAL_LIST: &str = "proposal.list";

/// Work proposal inspect operation.
pub const OP_PROPOSAL_INSPECT: &str = "proposal.inspect";

/// Work proposal mark-ready operation.
pub const OP_PROPOSAL_MARK_READY: &str = "proposal.mark_ready";

/// Work proposal approve operation.
pub const OP_PROPOSAL_APPROVE: &str = "proposal.approve";

/// Work proposal reject operation.
pub const OP_PROPOSAL_REJECT: &str = "proposal.reject";

/// Work proposal defer operation.
pub const OP_PROPOSAL_DEFER: &str = "proposal.defer";

/// Work proposal patch recording operation.
pub const OP_PROPOSAL_RECORD_PATCH: &str = "proposal.record_patch";

/// Agent talk prompt operation.
pub const OP_AGENT_TALK_PROMPT: &str = "agent.talk_prompt";

/// World snapshot operation.
pub const OP_WORLD_SNAPSHOT: &str = "world.snapshot";

/// Move player operation.
pub const OP_WORLD_MOVE_PLAYER: &str = "world.move_player";

/// Available world interactions operation.
pub const OP_WORLD_AVAILABLE_INTERACTIONS: &str = "world.available_interactions";

/// Advance lightweight world simulation operation.
pub const OP_WORLD_TICK: &str = "world.tick";

/// World template list operation.
pub const OP_WORLD_TEMPLATE_LIST: &str = "world.template_list";

/// World template select operation.
pub const OP_WORLD_SELECT_TEMPLATE: &str = "world.select_template";

/// Morning report operation.
pub const OP_REPORT_MORNING: &str = "report.morning";

/// Department report operation.
pub const OP_REPORT_DEPARTMENT: &str = "report.department";

/// Agent report operation.
pub const OP_REPORT_AGENT: &str = "report.agent";

/// Event list operation.
pub const OP_EVENT_LIST: &str = "event.list";

/// Event projection rebuild operation.
pub const OP_EVENT_REBUILD_PROJECTIONS: &str = "event.rebuild_projections";

/// Frontend-agnostic command submit operation.
pub const OP_COMMAND_SUBMIT: &str = "command.submit";

/// Operation list operation.
pub const OP_OPERATION_LIST: &str = "operation.list";

/// Operation claim-next operation.
pub const OP_OPERATION_CLAIM_NEXT: &str = "operation.claim_next";

/// Operation complete operation.
pub const OP_OPERATION_COMPLETE: &str = "operation.complete";

/// Operation fail operation.
pub const OP_OPERATION_FAIL: &str = "operation.fail";

/// Run a claimed operation operation.
pub const OP_OPERATION_RUN_CLAIMED: &str = "operation.run_claimed";

/// Scheduler tick operation.
pub const OP_SCHEDULER_TICK: &str = "scheduler.tick";

/// Dashboard projection get operation.
pub const OP_PROJECTION_DASHBOARD_GET: &str = "projection.dashboard.get";

/// World projection get operation.
pub const OP_PROJECTION_WORLD_GET: &str = "projection.world.get";

/// Pending AI work projection get operation.
pub const OP_PROJECTION_AI_WORK_GET: &str = "projection.ai_work.get";

/// Company activity projection get operation.
pub const OP_PROJECTION_ACTIVITY_GET: &str = "projection.activity.get";

/// CEO inbox projection get operation.
pub const OP_PROJECTION_CEO_INBOX_GET: &str = "projection.ceo_inbox.get";

/// Record task outcome operation.
pub const OP_TASK_RECORD_OUTCOME: &str = "task.record_outcome";

/// Blims protocol version for daemon/UI IPC payloads.
pub const BLIMS_PROTOCOL_VERSION: u16 = 1;

const MANIFEST: &str = include_str!("../bcode-plugin.toml");
const BLIMS_STATE_DIR_ENV: &str = "BCODE_BLIMS_STATE_DIR";
const DEFAULT_STATE_ROOT: &str = ".bcode/blims";
const DATABASE_FILE_NAME: &str = "blims.sqlite";
const SCHEMA_VERSION: i64 = 1;

/// Bundled Blims company simulator plugin.
#[derive(Default)]
pub struct BlimsPlugin;

impl RustPlugin for BlimsPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != BLIMS_SERVICE_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported Blims service interface",
            );
        }

        invoke_blims_service(&context.request)
    }
}

#[allow(clippy::too_many_lines)]
fn invoke_blims_service(request: &ServiceRequest) -> ServiceResponse {
    match request.operation.as_str() {
        OP_COMPANY_STATUS | OP_COMPANY_CREATE | OP_COMPANY_LOAD | OP_COMPANY_PAUSE
        | OP_COMPANY_RESUME | OP_COMPANY_SHUTDOWN => invoke_company_service(request),
        OP_AGENT_LIST
        | OP_AGENT_INSPECT
        | OP_AGENT_GET_PERMISSION
        | OP_AGENT_SET_PERMISSION
        | OP_AGENT_REQUEST_PERMISSION
        | OP_AGENT_RECORD_CONVERSATION
        | OP_AGENT_HIRE
        | OP_AGENT_SUSPEND
        | OP_AGENT_FIRE
        | OP_AGENT_MOVE
        | OP_AGENT_UPDATE_CONTRACT => invoke_agent_service(request),
        OP_INITIATIVE_CREATE => {
            service_initiative_create(request, &EventContext::from_request(request))
        }
        OP_INITIATIVE_LIST => service_initiative_list(request),
        OP_INITIATIVE_INSPECT => service_initiative_inspect(request),
        OP_INITIATIVE_SET_GUIDANCE => {
            service_initiative_set_guidance(request, &EventContext::from_request(request))
        }
        OP_INITIATIVE_PAUSE => {
            service_initiative_status(request, &EventContext::from_request(request), "paused")
        }
        OP_INITIATIVE_RESUME => {
            service_initiative_status(request, &EventContext::from_request(request), "active")
        }
        OP_GUIDANCE_SET => service_guidance_set(request, &EventContext::from_request(request)),
        OP_GUIDANCE_LIST => service_guidance_list(request),
        OP_INITIATIVE_PLAN_PROMPT => service_initiative_plan_prompt(request),
        OP_INITIATIVE_IMPORT_PLAN => {
            service_initiative_import_plan(request, &EventContext::from_request(request))
        }
        OP_TASK_LIST => service_task_list(request),
        OP_TASK_INSPECT => service_task_inspect(request),
        OP_TASK_WORK_PROMPT => service_task_work_prompt(request),
        OP_TASK_RECORD_OUTCOME => {
            service_task_record_outcome(request, &EventContext::from_request(request))
        }
        OP_ARTIFACT_LIST => service_artifact_list(request),
        OP_ARTIFACT_CREATE => {
            service_artifact_create(request, &EventContext::from_request(request))
        }
        OP_ARTIFACT_INSPECT => service_artifact_inspect(request),
        OP_ARTIFACT_APPROVE => {
            service_artifact_status(request, &EventContext::from_request(request), "approved")
        }
        OP_ARTIFACT_REJECT => {
            service_artifact_status(request, &EventContext::from_request(request), "rejected")
        }
        OP_ARTIFACT_DEFER => {
            service_artifact_status(request, &EventContext::from_request(request), "deferred")
        }
        OP_PROPOSAL_REGISTER => {
            service_proposal_register(request, &EventContext::from_request(request))
        }
        OP_PROPOSAL_LIST => service_proposal_list(request),
        OP_PROPOSAL_INSPECT => service_proposal_inspect(request),
        OP_PROPOSAL_MARK_READY => {
            service_proposal_mark_ready(request, &EventContext::from_request(request))
        }
        OP_PROPOSAL_APPROVE => {
            service_proposal_status(request, &EventContext::from_request(request), "approved")
        }
        OP_PROPOSAL_REJECT => {
            service_proposal_status(request, &EventContext::from_request(request), "rejected")
        }
        OP_PROPOSAL_DEFER => {
            service_proposal_status(request, &EventContext::from_request(request), "deferred")
        }
        OP_PROPOSAL_RECORD_PATCH => {
            service_proposal_record_patch(request, &EventContext::from_request(request))
        }
        OP_AGENT_TALK_PROMPT => service_agent_talk_prompt(request),
        OP_WORLD_SNAPSHOT => service_world_snapshot(request),
        OP_WORLD_MOVE_PLAYER => {
            service_world_move_player(request, &EventContext::from_request(request))
        }
        OP_WORLD_AVAILABLE_INTERACTIONS => service_world_available_interactions(request),
        OP_WORLD_TICK => service_world_tick(request, &EventContext::from_request(request)),
        OP_WORLD_TEMPLATE_LIST => service_world_template_list(),
        OP_WORLD_SELECT_TEMPLATE => {
            service_world_select_template(request, &EventContext::from_request(request))
        }
        OP_REPORT_MORNING => service_morning_report(request, &EventContext::from_request(request)),
        OP_REPORT_DEPARTMENT => service_department_report(request),
        OP_REPORT_AGENT => service_agent_report(request),
        OP_EVENT_LIST => service_event_list(request),
        OP_EVENT_REBUILD_PROJECTIONS => service_event_rebuild_projections(request),
        OP_COMMAND_SUBMIT => service_command_submit(request),
        OP_OPERATION_LIST => service_operation_list(request),
        OP_OPERATION_CLAIM_NEXT => service_operation_claim_next(request),
        OP_OPERATION_COMPLETE => service_operation_complete(request),
        OP_OPERATION_FAIL => service_operation_fail(request),
        OP_OPERATION_RUN_CLAIMED => service_operation_run_claimed(request),
        OP_SCHEDULER_TICK => service_scheduler_tick(request),
        OP_PROJECTION_DASHBOARD_GET => service_projection_dashboard_get(request),
        OP_PROJECTION_WORLD_GET => service_projection_world_get(request),
        OP_PROJECTION_AI_WORK_GET => service_projection_ai_work_get(request),
        OP_PROJECTION_ACTIVITY_GET => service_projection_activity_get(request),
        OP_PROJECTION_CEO_INBOX_GET => service_projection_ceo_inbox_get(request),
        _ => ServiceResponse::error("unsupported_operation", "unsupported Blims operation"),
    }
}

fn invoke_company_service(request: &ServiceRequest) -> ServiceResponse {
    match request.operation.as_str() {
        OP_COMPANY_STATUS => service_company_status(request),
        OP_COMPANY_CREATE | OP_COMPANY_LOAD => service_company_create(request),
        OP_COMPANY_PAUSE => {
            service_company_lifecycle(request, &EventContext::from_request(request), "paused")
        }
        OP_COMPANY_RESUME => {
            service_company_lifecycle(request, &EventContext::from_request(request), "running")
        }
        OP_COMPANY_SHUTDOWN => {
            service_company_lifecycle(request, &EventContext::from_request(request), "shutdown")
        }
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported Blims company operation",
        ),
    }
}

fn invoke_agent_service(request: &ServiceRequest) -> ServiceResponse {
    match request.operation.as_str() {
        OP_AGENT_LIST => service_agent_list(request),
        OP_AGENT_INSPECT => service_agent_inspect(request),
        OP_AGENT_GET_PERMISSION => service_agent_get_permission(request),
        OP_AGENT_SET_PERMISSION => {
            service_agent_set_permission(request, &EventContext::from_request(request))
        }
        OP_AGENT_REQUEST_PERMISSION => {
            service_agent_request_permission(request, &EventContext::from_request(request))
        }
        OP_AGENT_RECORD_CONVERSATION => {
            service_agent_record_conversation(request, &EventContext::from_request(request))
        }
        OP_AGENT_HIRE => service_agent_hire(request, &EventContext::from_request(request)),
        OP_AGENT_SUSPEND => {
            service_agent_employment(request, &EventContext::from_request(request), "suspended")
        }
        OP_AGENT_FIRE => {
            service_agent_employment(request, &EventContext::from_request(request), "fired")
        }
        OP_AGENT_MOVE => service_agent_move(request, &EventContext::from_request(request)),
        OP_AGENT_UPDATE_CONTRACT => {
            service_agent_update_contract(request, &EventContext::from_request(request))
        }
        _ => ServiceResponse::error("unsupported_operation", "unsupported Blims agent operation"),
    }
}

/// Request to list prepared AI work items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiWorkListRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Maximum number of work items to return.
    #[serde(default = "default_ai_work_limit")]
    pub limit: u64,
}

/// Request to list company activity items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityListRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Maximum number of activity items to return.
    #[serde(default = "default_activity_limit")]
    pub limit: u64,
}

/// Request to list CEO inbox items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CeoInboxRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Maximum number of inbox items to return.
    #[serde(default = "default_inbox_limit")]
    pub limit: u64,
}

/// Request carrying the workspace root for repo-local Blims state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EventContext {
    correlation: String,
    causation: String,
    expected_latest: Option<i64>,
}

impl EventContext {
    fn from_request(request: &ServiceRequest) -> Self {
        Self {
            correlation: format!("service:{}", request.operation),
            causation: format!("service:{}", request.operation),
            expected_latest: None,
        }
    }

    fn merge_world_tick_request(&self, request: &WorldTickRequest) -> Self {
        let tick_id = request
            .tick_id
            .clone()
            .unwrap_or_else(|| "frontend-tick".to_string());
        Self {
            correlation: request
                .correlation_id
                .clone()
                .unwrap_or_else(|| format!("world.tick:{tick_id}")),
            causation: request
                .causation_id
                .clone()
                .unwrap_or_else(|| self.causation.clone()),
            expected_latest: request.expected_latest_event_id.or(self.expected_latest),
        }
    }

    fn merge_workspace_request(&self, request: &WorkspaceRequest) -> Self {
        Self {
            correlation: request
                .correlation_id
                .clone()
                .unwrap_or_else(|| self.correlation.clone()),
            causation: request
                .causation_id
                .clone()
                .unwrap_or_else(|| self.causation.clone()),
            expected_latest: request.expected_latest_event_id.or(self.expected_latest),
        }
    }

    fn merge_agent_move_request(&self, request: &AgentMoveRequest) -> Self {
        Self {
            correlation: request
                .correlation_id
                .clone()
                .unwrap_or_else(|| self.correlation.clone()),
            causation: request
                .causation_id
                .clone()
                .unwrap_or_else(|| self.causation.clone()),
            expected_latest: request.expected_latest_event_id.or(self.expected_latest),
        }
    }
}

/// Typed Blims IPC request envelope for future daemon/frontends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlimsProtocolRequest<T> {
    /// Protocol version.
    pub protocol_version: u16,
    /// Service operation name.
    pub operation: String,
    /// Typed request payload.
    pub payload: T,
}

/// Typed Blims IPC response envelope for future daemon/frontends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlimsProtocolResponse<T> {
    /// Protocol version.
    pub protocol_version: u16,
    /// Typed response payload.
    pub payload: T,
}

/// Request to inspect or change an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Agent id.
    pub agent_id: String,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Request to move a Blims agent to a room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMoveRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Agent id.
    pub agent_id: String,
    /// Destination room id.
    pub room_id: String,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Request to update a Blims agent's mapped Bcode permissions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPermissionUpdateRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Agent id.
    pub agent_id: String,
    /// Mapped Bcode agent id.
    pub bcode_agent_id: String,
    /// Bash permission policy.
    pub bash: String,
    /// Read permission policy.
    pub read: String,
    /// Write permission policy.
    pub write: String,
    /// Edit permission policy.
    pub edit: String,
    /// External directory permission policy.
    pub external_directory: String,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Request to ask CEO for a permission escalation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionEscalationRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Agent id.
    pub agent_id: String,
    /// Permission category.
    pub category: String,
    /// Requested policy such as allow/ask/deny.
    pub requested_policy: String,
    /// Escalation reason.
    pub reason: String,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Request to record a conversation session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationRecordRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Conversation id.
    pub conversation_id: String,
    /// Agent id.
    pub agent_id: String,
    /// Bcode session id.
    pub session_id: String,
    /// Conversation summary.
    pub summary: String,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Request to hire a new agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHireRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// New agent id.
    pub agent_id: String,
    /// Agent display name.
    pub name: String,
    /// Agent role/title.
    pub role: String,
    /// Starting room id.
    #[serde(default = "default_agent_room")]
    pub room_id: String,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Request to update an agent contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentContractUpdateRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Agent id.
    pub agent_id: String,
    /// Responsibilities text.
    pub responsibilities: String,
    /// Restrictions text.
    pub restrictions: String,
    /// Escalation text.
    pub escalation: String,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Request for a department report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepartmentReportRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Department id.
    pub department_id: String,
}

/// Request for an agent report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentReportRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Agent id.
    pub agent_id: String,
}

/// Request to create a company initiative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitiativeCreateRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Initiative title.
    pub title: String,
    /// Optional initiative description.
    #[serde(default)]
    pub description: Option<String>,
    /// Optional initiative priority. Lower numbers sort first.
    #[serde(default)]
    pub priority: Option<i64>,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Request to add initiative-specific CEO guidance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitiativeGuidanceRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Initiative id receiving the guidance.
    pub initiative_id: String,
    /// Guidance text.
    pub guidance: String,
    /// Guidance strength label.
    #[serde(default = "default_guidance_strength")]
    pub strength: String,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Request to add CEO guidance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuidanceSetRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Guidance text.
    pub guidance: String,
    /// Guidance strength label.
    #[serde(default = "default_guidance_strength")]
    pub strength: String,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Request to build an AI planning prompt for an initiative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitiativePlanPromptRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Initiative id to plan.
    pub initiative_id: String,
}

/// Request to list persisted Blims events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventListRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Maximum number of events to return.
    #[serde(default = "default_event_limit")]
    pub limit: u64,
}

/// Frontend-neutral operation priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlimsOperationPriority {
    /// CEO-visible interactions that should not wait behind simulation work.
    Interactive,
    /// Foreground work related to currently visible views.
    Foreground,
    /// Autonomous company simulation work.
    Background,
    /// Projection rebuilds and housekeeping.
    Maintenance,
}

impl BlimsOperationPriority {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Foreground => "foreground",
            Self::Background => "background",
            Self::Maintenance => "maintenance",
        }
    }
}

impl std::fmt::Display for BlimsOperationPriority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Durable operation status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlimsOperationStatus {
    /// Command was accepted but not yet started by a worker.
    Queued,
    /// Operation is actively being processed.
    Running,
    /// Operation completed and projections/events were updated.
    Completed,
    /// Operation failed and recorded an error.
    Failed,
    /// Operation was abandoned by the caller or scheduler.
    Cancelled,
}

impl BlimsOperationStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for BlimsOperationStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Frontend-neutral Blims command payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlimsCommand {
    /// Refresh durable CEO dashboard projection data.
    RefreshDashboard,
    /// Move the CEO/player avatar to a room.
    MovePlayer {
        /// Destination room id.
        room_id: String,
    },
    /// Advance lightweight company/world simulation.
    TickWorld {
        /// Caller tick id for deterministic event correlation.
        #[serde(default)]
        tick_id: Option<String>,
        /// Optional wall-clock milliseconds supplied by the frontend/daemon.
        #[serde(default)]
        now_ms: Option<i64>,
    },
    SelectWorldTemplate {
        /// Starter template id.
        template_id: String,
    },
    SetProposalStatus {
        /// Proposal id.
        proposal_id: String,
        /// New proposal status.
        status: String,
    },
    SetArtifactStatus {
        /// Artifact id.
        artifact_id: String,
        /// New artifact status.
        status: String,
    },
    OpenAgentConversation {
        /// Agent id.
        agent_id: String,
        /// Blims conversation id.
        conversation_id: String,
        /// Bcode session id.
        session_id: String,
        /// Conversation summary.
        summary: String,
    },
    RecordConversationMessage {
        /// Blims conversation id.
        conversation_id: String,
        /// Agent id.
        agent_id: String,
        /// Speaker id or role.
        speaker: String,
        /// Message text.
        message: String,
    },
    ScheduleCompanyTick {
        /// Caller tick id for deterministic event correlation.
        #[serde(default)]
        tick_id: Option<String>,
        /// Optional wall-clock milliseconds supplied by the daemon.
        #[serde(default)]
        now_ms: Option<i64>,
    },
    ScheduleAgentPlanning {
        /// Agent id.
        agent_id: String,
    },
    ScheduleTaskWork {
        /// Task id.
        task_id: String,
    },
    /// Record that a frontend opened a dashboard view.
    OpenDashboardView,
    /// Record that a frontend closed a dashboard view.
    CloseDashboardView,
}

impl BlimsCommand {
    const fn operation_kind(&self) -> &'static str {
        match self {
            Self::RefreshDashboard => "dashboard.refresh",
            Self::MovePlayer { .. } => "world.move_player",
            Self::TickWorld { .. } => "world.tick",
            Self::SelectWorldTemplate { .. } => "world.select_template",
            Self::SetProposalStatus { .. } => "proposal.status_set",
            Self::SetArtifactStatus { .. } => "artifact.status_set",
            Self::OpenAgentConversation { .. } => "conversation.open",
            Self::RecordConversationMessage { .. } => "conversation.message_record",
            Self::ScheduleCompanyTick { .. } => "company.tick.scheduled",
            Self::ScheduleAgentPlanning { .. } => "agent.planning.scheduled",
            Self::ScheduleTaskWork { .. } => "task.work.scheduled",
            Self::OpenDashboardView => "dashboard.open_view",
            Self::CloseDashboardView => "dashboard.close_view",
        }
    }

    const fn priority(&self) -> BlimsOperationPriority {
        match self {
            Self::RefreshDashboard
            | Self::MovePlayer { .. }
            | Self::SelectWorldTemplate { .. }
            | Self::SetProposalStatus { .. }
            | Self::SetArtifactStatus { .. }
            | Self::OpenAgentConversation { .. }
            | Self::RecordConversationMessage { .. }
            | Self::OpenDashboardView
            | Self::CloseDashboardView => BlimsOperationPriority::Interactive,
            Self::TickWorld { .. }
            | Self::ScheduleCompanyTick { .. }
            | Self::ScheduleAgentPlanning { .. }
            | Self::ScheduleTaskWork { .. } => BlimsOperationPriority::Background,
        }
    }
}

/// Request to submit a frontend-neutral Blims command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSubmitRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Unique command id supplied by the caller.
    pub command_id: String,
    /// Actor issuing the command, such as `ceo` or an agent id.
    pub actor: String,
    /// Optional frontend id, such as `tui` or `web`.
    #[serde(default)]
    pub frontend_id: Option<String>,
    /// Optional Bcode session id associated with the command.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
    /// Typed command payload.
    pub command: BlimsCommand,
}

impl CommandSubmitRequest {
    fn event_context(&self) -> EventContext {
        EventContext {
            correlation: self.command_id.clone(),
            causation: self
                .session_id
                .clone()
                .unwrap_or_else(|| self.command_id.clone()),
            expected_latest: self.expected_latest_event_id,
        }
    }
}

/// Command submission acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSubmitResponse {
    /// Submitted command id.
    pub command_id: String,
    /// Durable operation id created for the command.
    pub operation_id: String,
    /// Current operation status.
    pub status: BlimsOperationStatus,
    /// Operation priority lane.
    pub priority: BlimsOperationPriority,
}

/// Request to list durable operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationListRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Maximum number of operations to return.
    #[serde(default = "default_operation_limit")]
    pub limit: u64,
}

/// Durable operation summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlimsOperationSummary {
    /// Durable operation id.
    pub id: String,
    /// Command id that created the operation.
    pub command_id: String,
    /// Actor that submitted the command.
    pub actor: String,
    /// Optional frontend id.
    pub frontend_id: String,
    /// Operation kind.
    pub kind: String,
    /// Priority lane.
    pub priority: String,
    /// Current operation status.
    pub status: String,
    /// Optional result event id.
    pub result_event_id: Option<i64>,
    /// Optional error text.
    pub error: String,
}

/// Request to claim the next queued operation for a worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationClaimNextRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Worker id claiming the operation.
    pub worker_id: String,
    /// Lease duration in milliseconds.
    #[serde(default = "default_operation_lease_ms")]
    pub lease_ms: i64,
}

/// Request to complete an operation by id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationCompleteRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Durable operation id.
    pub operation_id: String,
}

/// Request to fail an operation by id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationFailRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Durable operation id.
    pub operation_id: String,
    /// Error text.
    pub error: String,
}

/// Request to run a claimed operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationRunClaimedRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Durable operation id.
    pub operation_id: String,
}

/// Request to let the scheduler claim and run a bounded batch of work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulerTickRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Worker id.
    pub worker_id: String,
    /// Maximum operations to claim and run.
    #[serde(default = "default_scheduler_tick_limit")]
    pub limit: u64,
    /// Lease duration in milliseconds.
    #[serde(default = "default_operation_lease_ms")]
    pub lease_ms: i64,
}

/// Scheduler tick response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulerTickReport {
    /// Operations claimed by this tick.
    pub claimed: Vec<BlimsOperationSummary>,
    /// Operations completed by this tick.
    pub completed: Vec<BlimsOperationSummary>,
    /// Operations failed by this tick.
    pub failed: Vec<BlimsOperationSummary>,
}

/// Prepared autonomous AI work request for execution adapters/frontends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedAiWorkItem {
    /// Stable prepared work id.
    pub id: String,
    /// Related operation id.
    pub operation_id: String,
    /// Work kind.
    pub kind: String,
    /// Suggested agent id.
    pub agent_id: String,
    /// Optional task id.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Prompt text to send through Bcode AI/session orchestration.
    pub prompt: String,
}

/// Friendly company activity item for frontends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityItem {
    /// Activity id.
    pub id: String,
    /// Source event id.
    pub event_id: i64,
    /// Activity kind.
    pub kind: String,
    /// Short title.
    pub title: String,
    /// Human-friendly body.
    pub body: String,
    /// Optional actor/agent id.
    pub actor_id: String,
    /// Optional room id.
    pub room_id: String,
    /// Severity label.
    pub severity: String,
    /// Suggested action hint.
    pub action_hint: String,
}

/// User-facing CEO inbox item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CeoInboxItem {
    /// Inbox item id.
    pub id: String,
    /// Inbox kind.
    pub kind: String,
    /// Title.
    pub title: String,
    /// Summary.
    pub summary: String,
    /// Priority. Lower is more urgent.
    pub priority: i64,
    /// Optional actor/agent id.
    pub actor_id: String,
    /// User-friendly action label.
    pub action_label: String,
    /// Suggested command/action token.
    pub action_command: String,
}

/// Dashboard read model shared by all frontends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardProjection {
    /// Initiatives visible in CEO dashboard.
    pub initiatives: Vec<InitiativeSummary>,
    /// Tasks visible in CEO dashboard.
    pub tasks: Vec<TaskSummary>,
    /// Proposals awaiting or recording CEO attention.
    pub proposals: Vec<WorkProposalSummary>,
    /// Non-code artifacts visible in CEO dashboard.
    pub artifacts: Vec<ArtifactSummary>,
    /// Active and historic CEO guidance rows.
    pub guidance: Vec<GuidanceSummary>,
    /// Latest event id at projection read time.
    pub latest_event_id: i64,
    /// Latest dashboard refresh operation id if known.
    #[serde(default)]
    pub refreshed_by_operation_id: Option<String>,
}

/// Result from rebuilding current-state projections from events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionRebuildReport {
    /// Number of events replayed.
    pub events_replayed: usize,
    /// Number of projected initiatives.
    pub initiatives_projected: usize,
    /// Number of projected guidance rows.
    pub guidance_projected: usize,
    /// Number of projected artifacts.
    pub artifacts_projected: usize,
    /// Number of projected work proposals.
    pub proposals_projected: usize,
    /// Number of projected tasks.
    pub tasks_projected: usize,
    /// Number of projected agents.
    pub agents_projected: usize,
    /// Number of projected departments.
    pub departments_projected: usize,
    /// Number of projected teams.
    pub teams_projected: usize,
    /// Number of projected rooms.
    pub rooms_projected: usize,
    /// Current projected company lifecycle status.
    pub lifecycle_status: String,
}

/// Request to move the CEO/player avatar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldMovePlayerRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Destination room id.
    pub room_id: String,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Request to advance lightweight world simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldTickRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Caller tick id for deterministic event correlation.
    #[serde(default)]
    pub tick_id: Option<String>,
    /// Optional wall-clock milliseconds supplied by the frontend.
    #[serde(default)]
    pub now_ms: Option<i64>,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Request to switch starter office template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldTemplateSelectRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Starter template id.
    pub template_id: String,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Request to build an AI talk prompt for an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTalkPromptRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Blims agent id.
    pub agent_id: String,
}

/// Request to inspect a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskInspectRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Task id.
    pub task_id: String,
}

/// Request to record a completed task outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskOutcomeRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Task id.
    pub task_id: String,
    /// Whether the task outcome was successful.
    pub success: bool,
    /// Short result summary.
    pub summary: String,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Request to create a non-code Blims artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactCreateRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Parent initiative id.
    pub initiative_id: String,
    /// Artifact kind.
    pub kind: String,
    /// Artifact title.
    pub title: String,
    /// Artifact payload JSON or text.
    pub payload_json: String,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Request to inspect an artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactInspectRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Artifact id.
    pub artifact_id: String,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Prompt for starting real task work through Bcode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskWorkPrompt {
    /// Task id.
    pub task_id: String,
    /// Suggested agent id.
    pub agent_id: String,
    /// Prompt text.
    pub prompt: String,
}

/// Request to register a task work proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalRegisterRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Task id.
    pub task_id: String,
    /// Session id.
    pub session_id: String,
    /// Worktree path.
    pub worktree_path: PathBuf,
    /// Branch name.
    pub branch: String,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Request to inspect or update a proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Proposal id.
    pub proposal_id: String,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Request to record a proposal patch artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalRecordPatchRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Proposal id.
    pub proposal_id: String,
    /// Patch text.
    pub patch: String,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Work proposal summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkProposalSummary {
    /// Proposal id.
    pub id: String,
    /// Task id.
    pub task_id: String,
    /// Initiative id.
    pub initiative_id: String,
    /// Agent id.
    pub agent_id: String,
    /// Bcode session id.
    pub session_id: String,
    /// Worktree path.
    pub worktree_path: String,
    /// Branch name.
    pub branch: String,
    /// Proposal status.
    pub status: String,
    /// Proposal summary.
    pub summary: String,
    /// Validation notes.
    pub validation_notes: String,
}

/// Request to inspect an initiative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitiativeInspectRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Initiative id.
    pub initiative_id: String,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// Request to import an AI-generated plan for an initiative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitiativeImportPlanRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Initiative id receiving the plan.
    pub initiative_id: String,
    /// AI-generated plan payload.
    pub plan: AiInitiativePlan,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
    /// Optional expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
}

/// AI-generated initiative plan contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiInitiativePlan {
    /// Short plan summary.
    pub summary: String,
    /// Acceptance criteria proposed by AI.
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    /// Proposed tasks.
    #[serde(default)]
    pub tasks: Vec<AiTaskProposal>,
    /// Risks identified by AI.
    #[serde(default)]
    pub risks: Vec<String>,
    /// Questions for the CEO.
    #[serde(default)]
    pub questions: Vec<String>,
    /// Cozy/fun game opportunities proposed by AI.
    #[serde(default)]
    pub cozy_game_opportunities: Vec<String>,
}

/// AI-generated task proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiTaskProposal {
    /// Task title.
    pub title: String,
    /// Task description.
    #[serde(default)]
    pub description: String,
    /// Suggested Blims agent owner.
    #[serde(default)]
    pub suggested_agent_id: Option<String>,
    /// AI rationale for this task.
    #[serde(default)]
    pub rationale: String,
    /// Task priority. Lower numbers sort first.
    #[serde(default = "default_task_priority")]
    pub priority: i64,
}

/// Planning prompt returned for Bcode AI/session orchestration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitiativePlanningPrompt {
    /// Initiative id.
    pub initiative_id: String,
    /// Prompt text to send to an AI planning session.
    pub prompt: String,
}

/// Agent talk prompt returned for Bcode AI/session orchestration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTalkPrompt {
    /// Agent id.
    pub agent_id: String,
    /// Conversation id to associate with a Bcode session.
    pub conversation_id: String,
    /// Prompt text to send to an AI conversation session.
    pub prompt: String,
}

/// Persisted project summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSummary {
    /// Project id.
    pub id: String,
    /// Parent initiative id.
    pub initiative_id: String,
    /// Project title.
    pub title: String,
    /// Project status.
    pub status: String,
    /// Project rationale.
    pub rationale: String,
}

/// Persisted task summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    /// Task id.
    pub id: String,
    /// Parent initiative id.
    pub initiative_id: String,
    /// Task title.
    pub title: String,
    /// Task description.
    pub description: String,
    /// Task status.
    pub status: String,
    /// Assigned agent id, if any.
    pub assigned_agent_id: String,
    /// Task rationale.
    pub rationale: String,
    /// Current blocker text, if any.
    pub blocker: String,
    /// Task priority. Lower numbers sort first.
    pub priority: i64,
}

/// Persisted artifact summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactSummary {
    /// Artifact id.
    pub id: String,
    /// Parent initiative id.
    pub initiative_id: String,
    /// Artifact kind.
    pub kind: String,
    /// Artifact title.
    pub title: String,
    /// Artifact status.
    pub status: String,
}

/// Persisted Blims event summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlimsEventSummary {
    /// Monotonic database event id.
    pub id: i64,
    /// Event schema version.
    pub event_version: i64,
    /// Company id.
    pub company_id: String,
    /// Event kind.
    pub kind: String,
    /// Human-readable event summary.
    pub summary: String,
    /// Typed event payload JSON.
    pub payload_json: String,
    /// Correlation id linking related events.
    pub correlation_id: String,
    /// Causation id pointing at the triggering event/command.
    pub causation_id: String,
}

/// Persisted artifact detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDetail {
    /// Artifact id.
    pub id: String,
    /// Parent initiative id.
    pub initiative_id: String,
    /// Artifact kind.
    pub kind: String,
    /// Artifact title.
    pub title: String,
    /// Artifact status.
    pub status: String,
    /// Artifact payload JSON.
    pub payload_json: String,
}

/// Current Blims company lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompanyLifecycleState {
    /// No repo-local Blims company has been created yet.
    NotStarted,
    /// Repo-local Blims company state exists and background work can run.
    Created,
    /// Company work is temporarily paused.
    Paused,
    /// Company was cleanly shut down and can be resumed from state.
    Shutdown,
}

/// Blims company status summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompanyStatus {
    /// Current company lifecycle state.
    pub state: CompanyLifecycleState,
    /// Human-readable status summary.
    pub message: String,
    /// Whether a Blims daemon is currently connected.
    pub daemon_connected: bool,
    /// Resolved Blims state root.
    pub state_root: PathBuf,
    /// Resolved Blims `SQLite` database path.
    pub database_path: PathBuf,
    /// Persisted lifecycle status for daemon/frontends.
    pub lifecycle_status: String,
    /// Local daemon namespace id for this repo/state root/build.
    pub daemon_namespace: String,
    /// Local daemon metadata path.
    pub daemon_metadata_path: PathBuf,
    /// Protocol/build compatibility string.
    pub compatibility: String,
}

/// Starter office template summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldTemplateSummary {
    /// Stable template id.
    pub id: String,
    /// Template display name.
    pub name: String,
    /// Cozy theme summary.
    pub description: String,
    /// Room count.
    pub rooms: usize,
    /// Main productivity flavor.
    pub flavor: String,
}

/// World interaction available to the player.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldInteraction {
    /// Stable interaction id.
    pub id: String,
    /// Display label.
    pub label: String,
    /// Suggested CLI command.
    pub command: String,
    /// Interaction flavor source, such as agent, room, object, or theme.
    pub source: String,
}

/// Available interactions for the current player room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableInteractions {
    /// Current room id.
    pub room_id: String,
    /// Available interactions.
    pub interactions: Vec<WorldInteraction>,
}

/// Blims world room snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomSnapshot {
    /// Stable room identifier.
    pub id: String,
    /// Room display name.
    pub name: String,
    /// Room purpose or productivity modifier.
    pub purpose: String,
    /// Gameplay room kind, such as library/lab/studio/review.
    pub room_kind: String,
    /// Productivity modifier contributed by the room.
    pub productivity_modifier: i64,
    /// X coordinate in the office map.
    pub x: i64,
    /// Y coordinate in the office map.
    pub y: i64,
    /// Visual symbol for terminal frontends.
    pub symbol: String,
    /// Suggested terminal color name.
    pub color: String,
}

/// Snapshot of the currently visible Blims world.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldSnapshot {
    /// Starter office theme name.
    pub theme: String,
    /// Starter office template id.
    pub template_id: String,
    /// Office map width in terminal cells.
    pub width: i64,
    /// Office map height in terminal cells.
    pub height: i64,
    /// Player avatar display name.
    pub player_name: String,
    /// Starter rooms currently available in the office.
    pub rooms: Vec<RoomSnapshot>,
    /// Starter agents currently visible in the office.
    pub agents: Vec<AgentSnapshot>,
}

/// Minimal visible agent state for the initial Blims office.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSnapshot {
    /// Stable agent identifier.
    pub id: String,
    /// Agent display name.
    pub name: String,
    /// Current loose role or job title.
    pub role: String,
    /// Short current status.
    pub status: String,
    /// Current world room identifier.
    pub room_id: String,
}

/// CEO morning report summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MorningReport {
    /// Report title.
    pub title: String,
    /// Report bullet items.
    pub bullets: Vec<String>,
}

/// Focused department report summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepartmentReport {
    /// Report title.
    pub title: String,
    /// Department id.
    pub department_id: String,
    /// Report bullet items.
    pub bullets: Vec<String>,
}

/// Focused agent report summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentReport {
    /// Report title.
    pub title: String,
    /// Agent id.
    pub agent_id: String,
    /// Report bullet items.
    pub bullets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompanyRecord {
    name: String,
    mission: String,
    culture: String,
    lifecycle_status: String,
}

impl CompanyRecord {
    fn culture_priority_bias(&self) -> &'static str {
        let culture = self.culture.to_lowercase();
        if culture.contains("cozy") || culture.contains("safe") {
            "Prefer small, reversible, well-explained work; avoid high-risk leaps unless guidance is urgent."
        } else if culture.contains("fast") || culture.contains("bold") {
            "Prefer fast experiments and visible momentum; escalate risks instead of stalling silently."
        } else {
            "Balance usefulness, safety, autonomy, and CEO guidance when choosing priorities."
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentRecord {
    id: String,
    name: String,
    role: String,
    department_id: String,
    team_id: String,
    status: String,
    room_id: String,
}

impl AgentRecord {
    fn snapshot(self) -> AgentSnapshot {
        AgentSnapshot {
            id: self.id,
            name: self.name,
            role: self.role,
            status: self.status,
            room_id: self.room_id,
        }
    }
}

impl From<AgentSnapshot> for AgentRecord {
    fn from(snapshot: AgentSnapshot) -> Self {
        Self {
            id: snapshot.id,
            name: snapshot.name,
            role: snapshot.role,
            department_id: String::new(),
            team_id: String::new(),
            status: snapshot.status,
            room_id: snapshot.room_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AgentStatsRecord {
    agent_id: String,
    traits: String,
    skills: String,
    energy: i64,
    morale: i64,
    focus: i64,
    confidence: i64,
    speed_modifier: i64,
    quality_modifier: i64,
    risk_modifier: i64,
    creativity_modifier: i64,
    persistence_modifier: i64,
    collaboration_modifier: i64,
}

impl AgentStatsRecord {
    fn report_line(&self) -> String {
        format!(
            "Energy {} · morale {} · focus {} · confidence {}",
            self.energy, self.morale, self.focus, self.confidence
        )
    }

    fn mechanics_line(&self) -> String {
        format!(
            "Mechanics: speed {:+}, quality {:+}, risk {:+}, creativity {:+}, persistence {:+}, collaboration {:+}",
            self.speed_modifier,
            self.quality_modifier,
            self.risk_modifier,
            self.creativity_modifier,
            self.persistence_modifier,
            self.collaboration_modifier
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PermissionEscalationRecord {
    id: String,
    agent_id: String,
    category: String,
    requested_policy: String,
    reason: String,
    status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ConversationRecord {
    id: String,
    agent_id: String,
    session_id: String,
    status: String,
    summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReportRecord {
    id: String,
    report_type: String,
    title: String,
    summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AgentPermissionRecord {
    agent_id: String,
    bcode_agent_id: String,
    bash: String,
    read: String,
    write: String,
    edit: String,
    external_directory: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorktreeRecord {
    id: String,
    task_id: String,
    agent_id: String,
    session_id: String,
    path: String,
    branch: String,
    status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ContractViolationRecord {
    id: String,
    agent_id: String,
    severity: String,
    summary: String,
    action: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AgentRelationshipRecord {
    agent_id: String,
    other_agent_id: String,
    affinity: i64,
    trust: i64,
    notes: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AgentMemoryRecord {
    id: String,
    agent_id: String,
    kind: String,
    summary: String,
    importance: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorldObjectRecord {
    id: String,
    room_id: String,
    name: String,
    kind: String,
    symbol: String,
    color: String,
    interaction: String,
    productivity_modifier: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RoomRecord {
    id: String,
    name: String,
    purpose: String,
    room_kind: String,
    productivity_modifier: i64,
    x: i64,
    y: i64,
    symbol: String,
    color: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DepartmentRecord {
    id: String,
    name: String,
    purpose: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TeamRecord {
    id: String,
    department_id: String,
    name: String,
    purpose: String,
}

impl From<RoomSnapshot> for RoomRecord {
    fn from(snapshot: RoomSnapshot) -> Self {
        Self {
            id: snapshot.id,
            name: snapshot.name,
            purpose: snapshot.purpose,
            room_kind: snapshot.room_kind,
            productivity_modifier: snapshot.productivity_modifier,
            x: snapshot.x,
            y: snapshot.y,
            symbol: snapshot.symbol,
            color: snapshot.color,
        }
    }
}

/// Persisted initiative summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitiativeSummary {
    /// Initiative id.
    pub id: String,
    /// Initiative title.
    pub title: String,
    /// Initiative description.
    pub description: String,
    /// Initiative status.
    pub status: String,
    /// Initiative priority. Lower numbers sort first.
    pub priority: i64,
}

/// Persisted CEO guidance summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuidanceSummary {
    /// Guidance id.
    pub id: String,
    /// Guidance text.
    pub guidance: String,
    /// Guidance strength label.
    pub strength: String,
    /// Whether this guidance is active.
    pub active: bool,
}

fn default_guidance_strength() -> String {
    "strong".to_string()
}

const fn default_task_priority() -> i64 {
    100
}

const fn default_event_limit() -> u64 {
    100
}

const fn default_operation_limit() -> u64 {
    100
}

const fn default_ai_work_limit() -> u64 {
    50
}

const fn default_activity_limit() -> u64 {
    12
}

const fn default_inbox_limit() -> u64 {
    12
}

const fn default_operation_lease_ms() -> i64 {
    30_000
}

const fn default_scheduler_tick_limit() -> u64 {
    1
}

fn default_agent_room() -> String {
    "ceo-nook".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlimsCommandEnvelope<T> {
    /// Unique command id supplied by caller or daemon.
    pub command_id: String,
    /// Actor issuing the command, such as `ceo` or an agent id.
    pub actor: String,
    /// Optional Bcode session id associated with the command.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Expected latest event id for optimistic concurrency.
    #[serde(default)]
    pub expected_latest_event_id: Option<i64>,
    /// Typed command payload.
    pub payload: T,
}

impl<T> BlimsCommandEnvelope<T> {
    fn event_context(&self) -> EventContext {
        EventContext {
            correlation: self.command_id.clone(),
            causation: self
                .session_id
                .clone()
                .unwrap_or_else(|| self.command_id.clone()),
            expected_latest: self.expected_latest_event_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BlimsEventPayload {
    CompanyLifecycleSet {
        lifecycle_status: String,
    },
    CommandSubmitted {
        command_id: String,
        actor: String,
        frontend_id: Option<String>,
        kind: String,
        priority: String,
    },
    OperationStatusSet {
        operation_id: String,
        status: String,
        error: Option<String>,
    },
    DashboardProjectionRefreshed {
        operation_id: String,
    },
    StarterOfficeSelected {
        template_id: String,
        theme: String,
        player_room_id: String,
    },
    InitiativeCreated {
        initiative: InitiativeSummary,
    },
    InitiativeStatusSet {
        initiative_id: String,
        status: String,
    },
    GuidanceSet {
        guidance: GuidanceSummary,
    },
    InitiativeGuidanceSet {
        initiative_id: String,
        guidance: GuidanceSummary,
    },
    ProposalRegistered {
        proposal: WorkProposalSummary,
    },
    ProjectCreated {
        project: ProjectSummary,
    },
    WorktreeRecorded {
        worktree: WorktreeRecord,
    },
    ProposalStatusSet {
        proposal_id: String,
        status: String,
    },
    ArtifactCreated {
        artifact: ArtifactDetail,
    },
    ArtifactStatusSet {
        artifact_id: String,
        status: String,
    },
    TaskCreated {
        task: TaskSummary,
    },
    TaskOutcomeRecorded {
        task_id: String,
        agent_id: String,
        success: bool,
        summary: String,
        stats: AgentStatsRecord,
    },
    ReportGenerated {
        report: ReportRecord,
    },
    ConversationRecorded {
        conversation: ConversationRecord,
    },
    ConversationMessageRecorded {
        conversation_id: String,
        agent_id: String,
        speaker: String,
        message: String,
    },
    AgentPlanningCycleRecorded {
        agent_id: String,
        rationale: String,
    },
    AiWorkPrepared {
        work: PreparedAiWorkItem,
    },
    TaskWorkScheduled {
        task_id: String,
        agent_id: String,
        rationale: String,
    },
    AgentHired {
        agent: AgentSnapshot,
    },
    AgentMoved {
        agent_id: String,
        room_id: String,
    },
    AgentStatusSet {
        agent_id: String,
        status: String,
    },
    AgentStatsSet {
        stats: AgentStatsRecord,
    },
    AgentRelationshipSet {
        relationship: AgentRelationshipRecord,
    },
    AgentMemoryRecorded {
        memory: AgentMemoryRecord,
    },
    AgentPermissionSet {
        permission: AgentPermissionRecord,
    },
    PermissionEscalationRequested {
        request: PermissionEscalationRecord,
    },
    ContractViolationRecorded {
        violation: ContractViolationRecord,
    },
    AgentContractUpdated {
        agent_id: String,
        responsibilities: String,
        restrictions: String,
        escalation: String,
    },
    PlayerMoved {
        room_id: String,
    },
    DepartmentCreated {
        id: String,
        name: String,
        purpose: String,
    },
    TeamCreated {
        id: String,
        department_id: String,
        name: String,
        purpose: String,
    },
    WorldRoomCreated {
        room: RoomSnapshot,
    },
    WorldObjectPlaced {
        object: WorldObjectRecord,
    },
    InitiativePlanImported {
        initiative_id: String,
        task_count: usize,
    },
}

/// Errors returned by Blims state initialization.
#[derive(Debug, Error)]
pub enum BlimsStateError {
    /// State directory creation failed.
    #[error("failed to create Blims state directory {path}: {source}")]
    CreateStateDirectory {
        /// Directory that could not be created.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// `SQLite` initialization failed.
    #[error("failed to initialize Blims SQLite database {path}: {source}")]
    InitDatabase {
        /// Database path that could not be opened.
        path: PathBuf,
        /// Underlying database initialization error.
        source: switchy_database_connection::InitDbError,
    },
    /// Schema initialization failed.
    #[error("failed to initialize Blims database schema: {0}")]
    Schema(#[from] switchy_database::DatabaseError),
    /// JSON serialization failed.
    #[error("failed to encode Blims JSON payload: {0}")]
    Json(#[from] serde_json::Error),
    /// Event replay failed.
    #[error("failed to replay Blims event {event_id} ({kind}): {source}")]
    EventReplay {
        /// Event id.
        event_id: i64,
        /// Event kind.
        kind: String,
        /// Underlying payload decoding error.
        source: serde_json::Error,
    },
    /// State initialization worker panicked.
    #[error("Blims state initialization worker panicked: {0}")]
    WorkerPanicked(String),
    /// Command envelope was invalid.
    #[error("invalid Blims command envelope: {0}")]
    InvalidCommandEnvelope(String),
    /// Required state was missing.
    #[error("Blims company state has not been created at {0}")]
    StateMissing(PathBuf),
    /// Persisted state row was missing an expected column.
    #[error("Blims state row is missing column {0}")]
    MissingColumn(&'static str),
    /// A request field was invalid.
    #[error("invalid Blims request: {0}")]
    InvalidRequest(String),
    /// Event stream version did not match optimistic concurrency expectation.
    #[error("Blims event stream conflict: expected latest event id {expected}, actual {actual}")]
    EventConflict {
        /// Expected latest event id.
        expected: i64,
        /// Actual latest event id.
        actual: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatePaths {
    state_root: PathBuf,
    database_path: PathBuf,
}

#[must_use]
fn state_paths(working_directory: &Path) -> StatePaths {
    let state_root = env::var_os(BLIMS_STATE_DIR_ENV)
        .map_or_else(|| working_directory.join(DEFAULT_STATE_ROOT), PathBuf::from);
    let database_path = state_root.join(DATABASE_FILE_NAME);

    StatePaths {
        state_root,
        database_path,
    }
}

fn panic_to_blims_error(payload: Box<dyn Any + Send>) -> BlimsStateError {
    let message = payload.downcast::<String>().map_or_else(
        |payload| {
            payload.downcast::<&str>().map_or_else(
                |_| "unknown panic payload".to_string(),
                |message| (*message).to_string(),
            )
        },
        |message| *message,
    );
    BlimsStateError::WorkerPanicked(message)
}

fn service_company_status(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    json_response(&company_status(&request.working_directory))
}

fn service_company_create(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };

    match create_company_state(&request.working_directory) {
        Ok(status) => json_response(&status),
        Err(error) => ServiceResponse::error("state_initialization_failed", error.to_string()),
    }
}

fn service_company_lifecycle(
    request: &ServiceRequest,
    event_context: &EventContext,
    lifecycle_status: &str,
) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<WorkspaceRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };

    let event_context = event_context.merge_workspace_request(&request);
    match set_company_lifecycle(&request.working_directory, &event_context, lifecycle_status) {
        Ok(status) => json_response(&status),
        Err(error) => ServiceResponse::error("company_lifecycle_failed", error.to_string()),
    }
}

fn service_agent_list(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };

    match load_company_data(&request.working_directory) {
        Ok(data) => json_response(
            &data
                .agents
                .into_iter()
                .map(AgentRecord::snapshot)
                .collect::<Vec<_>>(),
        ),
        Err(error) => ServiceResponse::error("state_read_failed", error.to_string()),
    }
}

fn service_agent_inspect(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<AgentRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match inspect_agent(&request) {
        Ok(agent) => json_response(&agent),
        Err(error) => ServiceResponse::error("agent_inspect_failed", error.to_string()),
    }
}

fn service_agent_get_permission(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<AgentRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match inspect_agent_permission(&request) {
        Ok(permission) => json_response(&permission),
        Err(error) => ServiceResponse::error("agent_permission_read_failed", error.to_string()),
    }
}

fn service_agent_set_permission(
    request: &ServiceRequest,
    event_context: &EventContext,
) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<AgentPermissionUpdateRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match set_agent_permission(&request, &event_context) {
        Ok(permission) => json_response(&permission),
        Err(error) => ServiceResponse::error("agent_permission_update_failed", error.to_string()),
    }
}

fn service_agent_request_permission(
    request: &ServiceRequest,
    event_context: &EventContext,
) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<PermissionEscalationRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match request_permission_escalation(&request, &event_context) {
        Ok(escalation) => json_response(&escalation),
        Err(error) => ServiceResponse::error("permission_escalation_failed", error.to_string()),
    }
}

fn service_agent_record_conversation(
    request: &ServiceRequest,
    event_context: &EventContext,
) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<ConversationRecordRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match record_conversation(&request, &event_context) {
        Ok(conversation) => json_response(&conversation),
        Err(error) => ServiceResponse::error("conversation_record_failed", error.to_string()),
    }
}

fn service_agent_hire(request: &ServiceRequest, event_context: &EventContext) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<AgentHireRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match hire_agent(&request, &event_context) {
        Ok(agent) => json_response(&agent),
        Err(error) => ServiceResponse::error("agent_hire_failed", error.to_string()),
    }
}

fn service_agent_employment(
    request: &ServiceRequest,
    event_context: &EventContext,
    status: &str,
) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<AgentRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match set_agent_status(&request, &event_context, status) {
        Ok(agent) => json_response(&agent),
        Err(error) => ServiceResponse::error("agent_employment_failed", error.to_string()),
    }
}

fn service_agent_move(request: &ServiceRequest, event_context: &EventContext) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<AgentMoveRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match move_agent(&request, &event_context) {
        Ok(agent) => json_response(&agent),
        Err(error) => ServiceResponse::error("agent_move_failed", error.to_string()),
    }
}

fn service_agent_update_contract(
    request: &ServiceRequest,
    event_context: &EventContext,
) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<AgentContractUpdateRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match update_agent_contract(&request, &event_context) {
        Ok(contract) => json_response(&contract),
        Err(error) => ServiceResponse::error("agent_contract_update_failed", error.to_string()),
    }
}

fn service_initiative_create(
    request: &ServiceRequest,
    event_context: &EventContext,
) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<InitiativeCreateRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match create_initiative(&request, &event_context) {
        Ok(initiative) => json_response(&initiative),
        Err(error) => ServiceResponse::error("initiative_create_failed", error.to_string()),
    }
}

fn service_initiative_list(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match list_initiatives(&request.working_directory) {
        Ok(initiatives) => json_response(&initiatives),
        Err(error) => ServiceResponse::error("initiative_list_failed", error.to_string()),
    }
}

fn service_initiative_inspect(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<InitiativeInspectRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match inspect_initiative(&request) {
        Ok(initiative) => json_response(&initiative),
        Err(error) => ServiceResponse::error("initiative_inspect_failed", error.to_string()),
    }
}

fn service_initiative_set_guidance(
    request: &ServiceRequest,
    event_context: &EventContext,
) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<InitiativeGuidanceRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match set_initiative_guidance(&request, &event_context) {
        Ok(guidance) => json_response(&guidance),
        Err(error) => ServiceResponse::error("initiative_guidance_failed", error.to_string()),
    }
}

fn service_initiative_status(
    request: &ServiceRequest,
    event_context: &EventContext,
    status: &str,
) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<InitiativeInspectRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match set_initiative_status(&request, &event_context, status) {
        Ok(initiative) => json_response(&initiative),
        Err(error) => ServiceResponse::error("initiative_status_failed", error.to_string()),
    }
}

fn service_guidance_set(request: &ServiceRequest, event_context: &EventContext) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<GuidanceSetRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match set_guidance(&request, &event_context) {
        Ok(guidance) => json_response(&guidance),
        Err(error) => ServiceResponse::error("guidance_set_failed", error.to_string()),
    }
}

fn service_guidance_list(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match list_guidance(&request.working_directory) {
        Ok(guidance) => json_response(&guidance),
        Err(error) => ServiceResponse::error("guidance_list_failed", error.to_string()),
    }
}

fn service_initiative_plan_prompt(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<InitiativePlanPromptRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match build_initiative_plan_prompt(&request) {
        Ok(prompt) => json_response(&prompt),
        Err(error) => ServiceResponse::error("initiative_prompt_failed", error.to_string()),
    }
}

fn service_initiative_import_plan(
    request: &ServiceRequest,
    event_context: &EventContext,
) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<InitiativeImportPlanRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match import_initiative_plan(&request, &event_context) {
        Ok(plan) => json_response(&plan),
        Err(error) => ServiceResponse::error("initiative_import_failed", error.to_string()),
    }
}

fn service_task_list(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match list_tasks(&request.working_directory) {
        Ok(tasks) => json_response(&tasks),
        Err(error) => ServiceResponse::error("task_list_failed", error.to_string()),
    }
}

fn service_task_inspect(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<TaskInspectRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match inspect_task(&request) {
        Ok(task) => json_response(&task),
        Err(error) => ServiceResponse::error("task_inspect_failed", error.to_string()),
    }
}

fn service_task_work_prompt(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<TaskInspectRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match build_task_work_prompt(&request) {
        Ok(prompt) => json_response(&prompt),
        Err(error) => ServiceResponse::error("task_work_prompt_failed", error.to_string()),
    }
}

fn service_task_record_outcome(
    request: &ServiceRequest,
    event_context: &EventContext,
) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<TaskOutcomeRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match record_task_outcome(&request, &event_context) {
        Ok(stats) => json_response(&stats),
        Err(error) => ServiceResponse::error("task_record_outcome_failed", error.to_string()),
    }
}

fn service_artifact_list(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match list_artifacts(&request.working_directory) {
        Ok(artifacts) => json_response(&artifacts),
        Err(error) => ServiceResponse::error("artifact_list_failed", error.to_string()),
    }
}

fn service_artifact_create(
    request: &ServiceRequest,
    event_context: &EventContext,
) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<ArtifactCreateRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match create_artifact(&request, &event_context) {
        Ok(artifact) => json_response(&artifact),
        Err(error) => ServiceResponse::error("artifact_create_failed", error.to_string()),
    }
}

fn service_artifact_inspect(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<ArtifactInspectRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match inspect_artifact(&request) {
        Ok(artifact) => json_response(&artifact),
        Err(error) => ServiceResponse::error("artifact_inspect_failed", error.to_string()),
    }
}

fn service_artifact_status(
    request: &ServiceRequest,
    event_context: &EventContext,
    status: &str,
) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<ArtifactInspectRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match set_artifact_status(&request, &event_context, status) {
        Ok(artifact) => json_response(&artifact),
        Err(error) => ServiceResponse::error("artifact_status_failed", error.to_string()),
    }
}

fn service_proposal_register(
    request: &ServiceRequest,
    event_context: &EventContext,
) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<ProposalRegisterRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match register_proposal(&request, &event_context) {
        Ok(proposal) => json_response(&proposal),
        Err(error) => ServiceResponse::error("proposal_register_failed", error.to_string()),
    }
}

fn service_proposal_list(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match list_proposals(&request.working_directory) {
        Ok(proposals) => json_response(&proposals),
        Err(error) => ServiceResponse::error("proposal_list_failed", error.to_string()),
    }
}

fn service_proposal_inspect(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<ProposalRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match inspect_proposal(&request) {
        Ok(proposal) => json_response(&proposal),
        Err(error) => ServiceResponse::error("proposal_inspect_failed", error.to_string()),
    }
}

fn service_proposal_mark_ready(
    request: &ServiceRequest,
    event_context: &EventContext,
) -> ServiceResponse {
    service_proposal_status(request, event_context, "ready_for_review")
}

fn service_proposal_status(
    request: &ServiceRequest,
    event_context: &EventContext,
    status: &str,
) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<ProposalRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match set_proposal_status(&request, &event_context, status) {
        Ok(proposal) => json_response(&proposal),
        Err(error) => ServiceResponse::error("proposal_status_failed", error.to_string()),
    }
}

fn service_proposal_record_patch(
    request: &ServiceRequest,
    event_context: &EventContext,
) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<ProposalRecordPatchRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match record_proposal_patch(&request, &event_context) {
        Ok(artifact) => json_response(&artifact),
        Err(error) => ServiceResponse::error("proposal_record_patch_failed", error.to_string()),
    }
}

fn service_agent_talk_prompt(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<AgentTalkPromptRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match build_agent_talk_prompt(&request) {
        Ok(prompt) => json_response(&prompt),
        Err(error) => ServiceResponse::error("agent_talk_prompt_failed", error.to_string()),
    }
}

fn service_world_snapshot(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };

    world_snapshot(&request.working_directory).map_or_else(
        |_| json_response(&fallback_world_snapshot()),
        |snapshot| json_response(&snapshot),
    )
}

fn service_world_move_player(
    request: &ServiceRequest,
    event_context: &EventContext,
) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<WorldMovePlayerRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match move_player(&request, &event_context) {
        Ok(snapshot) => json_response(&snapshot),
        Err(error) => ServiceResponse::error("world_move_player_failed", error.to_string()),
    }
}

fn service_world_available_interactions(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match available_interactions(&request.working_directory) {
        Ok(interactions) => json_response(&interactions),
        Err(error) => ServiceResponse::error("world_interactions_failed", error.to_string()),
    }
}

fn service_world_tick(request: &ServiceRequest, event_context: &EventContext) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<WorldTickRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match tick_world(&request, &event_context) {
        Ok(snapshot) => json_response(&snapshot),
        Err(error) => ServiceResponse::error("world_tick_failed", error.to_string()),
    }
}

fn service_world_template_list() -> ServiceResponse {
    json_response(&starter_world_templates())
}

fn service_world_select_template(
    request: &ServiceRequest,
    event_context: &EventContext,
) -> ServiceResponse {
    let (request, event_context) =
        match parse_service_command::<WorldTemplateSelectRequest>(request, event_context) {
            Ok(parsed) => parsed,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
    match select_world_template(&request, &event_context) {
        Ok(snapshot) => json_response(&snapshot),
        Err(error) => ServiceResponse::error("world_select_template_failed", error.to_string()),
    }
}

fn service_morning_report(
    request: &ServiceRequest,
    event_context: &EventContext,
) -> ServiceResponse {
    let Ok(request) = request.payload_json::<WorkspaceRequest>() else {
        return json_response(&fallback_morning_report());
    };

    generate_morning_report(&request, event_context).map_or_else(
        |_| json_response(&fallback_morning_report()),
        |report| json_response(&report),
    )
}

fn service_department_report(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<DepartmentReportRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    load_company_data(&request.working_directory).map_or_else(
        |error| ServiceResponse::error("state_read_failed", error.to_string()),
        |data| match department_report(&data, &request.department_id) {
            Ok(report) => json_response(&report),
            Err(error) => ServiceResponse::error("department_report_failed", error.to_string()),
        },
    )
}

fn service_agent_report(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<AgentReportRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    load_company_data(&request.working_directory).map_or_else(
        |error| ServiceResponse::error("state_read_failed", error.to_string()),
        |data| match agent_report(&data, &request.agent_id) {
            Ok(report) => json_response(&report),
            Err(error) => ServiceResponse::error("agent_report_failed", error.to_string()),
        },
    )
}

fn service_event_list(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<EventListRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match list_events(&request) {
        Ok(events) => json_response(&events),
        Err(error) => ServiceResponse::error("event_list_failed", error.to_string()),
    }
}

fn service_event_rebuild_projections(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match rebuild_projections(&request.working_directory) {
        Ok(report) => json_response(&report),
        Err(error) => ServiceResponse::error("event_projection_rebuild_failed", error.to_string()),
    }
}

fn service_command_submit(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<CommandSubmitRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match submit_command(&request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("command_submit_failed", error.to_string()),
    }
}

fn service_operation_list(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<OperationListRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match list_operations(&request) {
        Ok(operations) => json_response(&operations),
        Err(error) => ServiceResponse::error("operation_list_failed", error.to_string()),
    }
}

fn service_operation_claim_next(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<OperationClaimNextRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match claim_next_operation(&request) {
        Ok(operation) => json_response(&operation),
        Err(error) => ServiceResponse::error("operation_claim_next_failed", error.to_string()),
    }
}

fn service_operation_complete(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<OperationCompleteRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match complete_claimed_operation(&request) {
        Ok(operation) => json_response(&operation),
        Err(error) => ServiceResponse::error("operation_complete_failed", error.to_string()),
    }
}

fn service_operation_fail(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<OperationFailRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match fail_claimed_operation(&request) {
        Ok(operation) => json_response(&operation),
        Err(error) => ServiceResponse::error("operation_fail_failed", error.to_string()),
    }
}

fn service_operation_run_claimed(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<OperationRunClaimedRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match run_claimed_operation(&request) {
        Ok(operation) => json_response(&operation),
        Err(error) => ServiceResponse::error("operation_run_claimed_failed", error.to_string()),
    }
}

fn service_scheduler_tick(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<SchedulerTickRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match scheduler_tick(&request) {
        Ok(report) => json_response(&report),
        Err(error) => ServiceResponse::error("scheduler_tick_failed", error.to_string()),
    }
}

fn service_projection_dashboard_get(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match dashboard_projection(&request.working_directory) {
        Ok(projection) => json_response(&projection),
        Err(error) => ServiceResponse::error("dashboard_projection_failed", error.to_string()),
    }
}

fn service_projection_world_get(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match world_snapshot(&request.working_directory) {
        Ok(projection) => json_response(&projection),
        Err(error) => ServiceResponse::error("world_projection_failed", error.to_string()),
    }
}

fn service_projection_ai_work_get(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<AiWorkListRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match list_prepared_ai_work(&request) {
        Ok(work) => json_response(&work),
        Err(error) => ServiceResponse::error("ai_work_projection_failed", error.to_string()),
    }
}

fn service_projection_activity_get(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<ActivityListRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match list_activity_items(&request) {
        Ok(items) => json_response(&items),
        Err(error) => ServiceResponse::error("activity_projection_failed", error.to_string()),
    }
}

fn service_projection_ceo_inbox_get(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<CeoInboxRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match list_ceo_inbox_items(&request) {
        Ok(items) => json_response(&items),
        Err(error) => ServiceResponse::error("ceo_inbox_projection_failed", error.to_string()),
    }
}

fn company_status(working_directory: &Path) -> CompanyStatus {
    let paths = state_paths(working_directory);
    let lifecycle_status = paths.database_path.exists().then(|| {
        load_company_data(working_directory).map_or_else(
            |_| "running".to_string(),
            |data| data.company.lifecycle_status,
        )
    });
    let state = match lifecycle_status.as_deref() {
        None => CompanyLifecycleState::NotStarted,
        Some("paused") => CompanyLifecycleState::Paused,
        Some("shutdown") => CompanyLifecycleState::Shutdown,
        Some(_) => CompanyLifecycleState::Created,
    };
    let message = company_status_message(&state);
    let daemon_namespace = daemon_namespace(working_directory, &paths);
    let daemon_metadata_path = paths.state_root.join("daemon.json");

    CompanyStatus {
        state,
        message,
        daemon_connected: daemon_metadata_path.exists(),
        state_root: paths.state_root,
        database_path: paths.database_path,
        lifecycle_status: lifecycle_status.unwrap_or_else(|| "not_started".to_string()),
        daemon_namespace,
        daemon_metadata_path,
        compatibility: format!("blims-protocol-v{BLIMS_PROTOCOL_VERSION}"),
    }
}

fn company_status_message(state: &CompanyLifecycleState) -> String {
    match state {
        CompanyLifecycleState::Created => {
            "Blims company state exists. The office is ready to wake.".to_string()
        }
        CompanyLifecycleState::Paused => {
            "Blims company work is paused. Resume when the office should continue.".to_string()
        }
        CompanyLifecycleState::Shutdown => {
            "Blims company is shut down but fully resurrectable from repo-local state.".to_string()
        }
        CompanyLifecycleState::NotStarted => {
            "Blims is bundled and ready. Create a repo-local company to wake the office."
                .to_string()
        }
    }
}

fn daemon_namespace(working_directory: &Path, paths: &StatePaths) -> String {
    format!(
        "blims-{}-{}-v{}",
        stable_slug(&working_directory.display().to_string()),
        stable_slug(&paths.state_root.display().to_string()),
        BLIMS_PROTOCOL_VERSION
    )
}

fn create_company_state(working_directory: &Path) -> Result<CompanyStatus, BlimsStateError> {
    let paths = state_paths(working_directory);
    std::fs::create_dir_all(&paths.state_root).map_err(|source| {
        BlimsStateError::CreateStateDirectory {
            path: paths.state_root.clone(),
            source,
        }
    })?;

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread Tokio runtime should build");
        runtime.block_on(async {
            let database = switchy_database_connection::init(Some(&paths.database_path), None)
                .await
                .map_err(|source| BlimsStateError::InitDatabase {
                    path: paths.database_path.clone(),
                    source,
                })?;
            initialize_schema(database.as_ref()).await?;
            write_daemon_metadata(&paths).map_err(|source| {
                BlimsStateError::CreateStateDirectory {
                    path: paths.state_root.clone(),
                    source,
                }
            })?;
            Ok::<(), BlimsStateError>(())
        })
    })
    .join()
    .map_err(panic_to_blims_error)??;

    Ok(company_status(working_directory))
}

fn write_daemon_metadata(paths: &StatePaths) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(&paths.state_root)?;
    std::fs::write(
        paths.state_root.join("daemon.json"),
        format!(
            "{{\n  \"transport\": \"bmux-ipc-local-placeholder\",\n  \"protocol_version\": {BLIMS_PROTOCOL_VERSION},\n  \"database\": \"{}\",\n  \"background\": true\n}}\n",
            paths.database_path.display()
        ),
    )
}

fn set_company_lifecycle(
    working_directory: &Path,
    event_context: &EventContext,
    lifecycle_status: &str,
) -> Result<CompanyStatus, BlimsStateError> {
    let lifecycle_status = lifecycle_status.to_string();
    let event_context = event_context.clone();
    with_database(working_directory, move |database| {
        Box::pin(async move {
            append_event(
                database,
                &event_context,
                "company.lifecycle_set",
                format!("Blims company lifecycle set to {lifecycle_status}."),
                &BlimsEventPayload::CompanyLifecycleSet {
                    lifecycle_status: lifecycle_status.clone(),
                },
            )
            .await?;
            Ok::<(), BlimsStateError>(())
        })
    })?;
    Ok(company_status(working_directory))
}

async fn initialize_schema(database: &dyn Database) -> Result<(), switchy_database::DatabaseError> {
    create_core_tables(database).await?;
    seed_core_rows(database).await
}

async fn create_core_tables(
    database: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    create_base_tables(database).await?;
    create_operation_tables(database).await?;
    create_org_tables(database).await?;
    create_world_tables(database).await?;
    create_work_tables(database).await
}

async fn create_base_tables(
    database: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    create_table("blims_schema_version")
        .if_not_exists(true)
        .column(int_column("version"))
        .column(now_column("applied_at"))
        .execute(database)
        .await?;
    create_table("companies")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("name"))
        .column(text_column("mission"))
        .column(text_column("culture"))
        .column(text_column("lifecycle_status"))
        .column(now_column("created_at"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("blims_config")
        .if_not_exists(true)
        .column(text_column("key"))
        .column(text_column("value"))
        .primary_key("key")
        .execute(database)
        .await?;
    create_table("events")
        .if_not_exists(true)
        .column(auto_id_column("id"))
        .column(text_column("company_id"))
        .column(text_column("kind"))
        .column(text_column("summary"))
        .column(text_column("payload_json"))
        .column(text_column("correlation_id"))
        .column(text_column("causation_id"))
        .column(int_column("event_version"))
        .column(now_column("created_at"))
        .primary_key("id")
        .execute(database)
        .await
}

async fn create_operation_tables(
    database: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    create_table("operations")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("command_id"))
        .column(text_column("actor"))
        .column(text_column("frontend_id"))
        .column(text_column("kind"))
        .column(text_column("priority"))
        .column(text_column("status"))
        .column(int_column("result_event_id"))
        .column(text_column("error"))
        .column(now_column("created_at"))
        .column(now_column("updated_at"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("operation_leases")
        .if_not_exists(true)
        .column(text_column("operation_id"))
        .column(text_column("worker_id"))
        .column(int_column("lease_expires_at_ms"))
        .column(int_column("heartbeat_at_ms"))
        .primary_key("operation_id")
        .execute(database)
        .await?;
    create_table("operation_attempts")
        .if_not_exists(true)
        .column(auto_id_column("id"))
        .column(text_column("operation_id"))
        .column(text_column("worker_id"))
        .column(text_column("status"))
        .column(text_column("error"))
        .column(int_column("started_at_ms"))
        .column(int_column("finished_at_ms"))
        .primary_key("id")
        .execute(database)
        .await
}

async fn create_org_tables(database: &dyn Database) -> Result<(), switchy_database::DatabaseError> {
    create_table("departments")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("company_id"))
        .column(text_column("name"))
        .column(text_column("purpose"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("teams")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("department_id"))
        .column(text_column("name"))
        .column(text_column("purpose"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("agents")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("name"))
        .column(text_column("role"))
        .column(text_column("department_id"))
        .column(text_column("team_id"))
        .column(text_column("status"))
        .column(text_column("room_id"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_agent_policy_tables(database).await?;
    create_agent_context_tables(database).await
}

async fn create_agent_policy_tables(
    database: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    create_table("agent_contracts")
        .if_not_exists(true)
        .column(text_column("agent_id"))
        .column(text_column("responsibilities"))
        .column(text_column("restrictions"))
        .column(text_column("escalation"))
        .column(text_column("reporting_expectations"))
        .column(text_column("disciplinary_policy"))
        .primary_key("agent_id")
        .execute(database)
        .await?;
    create_table("agent_permissions")
        .if_not_exists(true)
        .column(text_column("agent_id"))
        .column(text_column("bcode_agent_id"))
        .column(text_column("bash"))
        .column(text_column("read"))
        .column(text_column("write"))
        .column(text_column("edit"))
        .column(text_column("external_directory"))
        .primary_key("agent_id")
        .execute(database)
        .await?;
    create_table("permission_escalations")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("agent_id"))
        .column(text_column("category"))
        .column(text_column("requested_policy"))
        .column(text_column("reason"))
        .column(text_column("status"))
        .column(now_column("created_at"))
        .primary_key("id")
        .execute(database)
        .await?;
    Ok(())
}

async fn create_agent_context_tables(
    database: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    create_table("agent_stats")
        .if_not_exists(true)
        .column(text_column("agent_id"))
        .column(text_column("traits"))
        .column(text_column("skills"))
        .column(int_column("energy"))
        .column(int_column("morale"))
        .column(int_column("focus"))
        .column(int_column("confidence"))
        .column(int_column("speed_modifier"))
        .column(int_column("quality_modifier"))
        .column(int_column("risk_modifier"))
        .column(int_column("creativity_modifier"))
        .column(int_column("persistence_modifier"))
        .column(int_column("collaboration_modifier"))
        .primary_key("agent_id")
        .execute(database)
        .await?;
    create_table("agent_relationships")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("agent_id"))
        .column(text_column("other_agent_id"))
        .column(int_column("affinity"))
        .column(int_column("trust"))
        .column(text_column("notes"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("memories")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("agent_id"))
        .column(text_column("kind"))
        .column(text_column("summary"))
        .column(int_column("importance"))
        .column(now_column("created_at"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("contract_violations")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("agent_id"))
        .column(text_column("severity"))
        .column(text_column("summary"))
        .column(text_column("action"))
        .column(now_column("created_at"))
        .primary_key("id")
        .execute(database)
        .await
}

async fn create_world_tables(
    database: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    create_table("worlds")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("company_id"))
        .column(text_column("theme"))
        .column(text_column("player_room_id"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("world_rooms")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("world_id"))
        .column(text_column("name"))
        .column(text_column("purpose"))
        .column(text_column("room_kind"))
        .column(int_column("productivity_modifier"))
        .column(int_column("x"))
        .column(int_column("y"))
        .column(text_column("symbol"))
        .column(text_column("color"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("world_objects")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("room_id"))
        .column(text_column("name"))
        .column(text_column("kind"))
        .column(text_column("symbol"))
        .column(text_column("color"))
        .column(text_column("interaction"))
        .column(int_column("productivity_modifier"))
        .primary_key("id")
        .execute(database)
        .await
}

async fn create_work_tables(
    database: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    create_table("executive_guidance")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("company_id"))
        .column(text_column("guidance"))
        .column(text_column("strength"))
        .column(bool_column("active"))
        .column(now_column("created_at"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("initiatives")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("company_id"))
        .column(text_column("title"))
        .column(text_column("description"))
        .column(text_column("status"))
        .column(int_column("priority"))
        .column(text_column("created_by"))
        .column(text_column("guidance"))
        .column(now_column("created_at"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("projects")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("initiative_id"))
        .column(text_column("title"))
        .column(text_column("status"))
        .column(text_column("rationale"))
        .column(now_column("created_at"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_task_and_artifact_tables(database).await
}

async fn create_task_and_artifact_tables(
    database: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    create_table("tasks")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("initiative_id"))
        .column(text_column("title"))
        .column(text_column("description"))
        .column(text_column("status"))
        .column(text_column("assigned_agent_id"))
        .column(text_column("rationale"))
        .column(text_column("blocker"))
        .column(int_column("priority"))
        .column(now_column("created_at"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("artifacts")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("initiative_id"))
        .column(text_column("kind"))
        .column(text_column("title"))
        .column(text_column("status"))
        .column(text_column("payload_json"))
        .column(now_column("created_at"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("work_proposals")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("task_id"))
        .column(text_column("initiative_id"))
        .column(text_column("agent_id"))
        .column(text_column("session_id"))
        .column(text_column("worktree_path"))
        .column(text_column("branch"))
        .column(text_column("status"))
        .column(text_column("summary"))
        .column(text_column("validation_notes"))
        .column(now_column("created_at"))
        .column(now_column("updated_at"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("worktree_records")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("task_id"))
        .column(text_column("agent_id"))
        .column(text_column("session_id"))
        .column(text_column("path"))
        .column(text_column("branch"))
        .column(text_column("status"))
        .column(now_column("created_at"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("reports")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("report_type"))
        .column(text_column("title"))
        .column(text_column("summary"))
        .column(now_column("created_at"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("conversations")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("agent_id"))
        .column(text_column("session_id"))
        .column(text_column("status"))
        .column(text_column("summary"))
        .column(now_column("created_at"))
        .primary_key("id")
        .execute(database)
        .await
}

fn text_column(name: &str) -> Column {
    Column {
        name: name.to_string(),
        nullable: false,
        auto_increment: false,
        data_type: DataType::Text,
        default: None,
    }
}

fn int_column(name: &str) -> Column {
    Column {
        name: name.to_string(),
        nullable: false,
        auto_increment: false,
        data_type: DataType::BigInt,
        default: None,
    }
}

fn bool_column(name: &str) -> Column {
    Column {
        name: name.to_string(),
        nullable: false,
        auto_increment: false,
        data_type: DataType::Bool,
        default: Some(DatabaseValue::Bool(true)),
    }
}

fn auto_id_column(name: &str) -> Column {
    Column {
        name: name.to_string(),
        nullable: false,
        auto_increment: true,
        data_type: DataType::BigInt,
        default: None,
    }
}

fn now_column(name: &str) -> Column {
    Column {
        name: name.to_string(),
        nullable: false,
        auto_increment: false,
        data_type: DataType::DateTime,
        default: Some(DatabaseValue::Now),
    }
}

async fn seed_core_rows(database: &dyn Database) -> Result<(), switchy_database::DatabaseError> {
    database
        .exec_raw(&format!(
            "INSERT INTO blims_schema_version (version) SELECT {SCHEMA_VERSION} \
             WHERE NOT EXISTS (SELECT 1 FROM blims_schema_version WHERE version = {SCHEMA_VERSION})"
        ))
        .await?;
    database
        .exec_raw(
            "INSERT INTO companies (id, name, mission, culture, lifecycle_status) \
             SELECT 'default', 'Blims', \
             'Build a cozy autonomous AI company inside Bcode.', \
             'cozy, fun, dynamic, productive', \
             'running' \
             WHERE NOT EXISTS (SELECT 1 FROM companies WHERE id = 'default')",
        )
        .await?;
    seed_default_config(database).await?;
    seed_default_world(database).await?;
    seed_company_created_event(database).await?;
    seed_starter_org_events(database).await?;
    seed_starter_world_events(database).await?;
    rebuild_projections_from_database(database)
        .await
        .map_err(|error| switchy_database::DatabaseError::QueryFailed(error.to_string()))?;
    Ok(())
}

async fn seed_default_world(
    database: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    let existing = database
        .select("worlds")
        .columns(&["id"])
        .filter(Box::new(where_eq("id", "default")))
        .limit(1)
        .execute_first(database)
        .await?;
    if existing.is_some() {
        return Ok(());
    }
    database
        .insert("worlds")
        .value("id", "default")
        .value("company_id", "default")
        .value("theme", "Cozy Startup Loft")
        .value("player_room_id", "ceo-nook")
        .execute(database)
        .await?;
    Ok(())
}

async fn seed_default_config(
    database: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    let defaults = [
        ("state_root", DEFAULT_STATE_ROOT),
        ("autonomy.default_level", "guided"),
        ("agent_roster.default", "mira,jules,pip"),
        ("starter_office", "cozy-startup-loft"),
        (
            "permissions.default",
            "read=allow,bash=ask,write=ask,edit=ask,external_directory=deny",
        ),
        (
            "contracts.default",
            "report blockers/risk/validation; log disagreement rationale",
        ),
        ("daemon.background", "enabled"),
        (
            "pause_behavior",
            "pause autonomous work, keep state queryable",
        ),
        (
            "shutdown_behavior",
            "persist state and stop background work",
        ),
        ("report_cadence", "on-entry,on-request"),
    ];
    for (key, value) in defaults {
        database
            .exec_raw(&format!(
                "INSERT INTO blims_config (key, value) SELECT '{key}', '{value}' WHERE NOT EXISTS (SELECT 1 FROM blims_config WHERE key = '{key}')"
            ))
            .await?;
    }
    Ok(())
}

async fn seed_company_created_event(
    database: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    database
        .exec_raw(
            "INSERT INTO events (company_id, kind, summary, payload_json, correlation_id, causation_id, event_version) \
             SELECT 'default', 'company.created', 'Blims company state initialized.', '{\"type\":\"company_lifecycle_set\",\"lifecycle_status\":\"running\"}', 'bootstrap', 'bootstrap', 1 \
             WHERE NOT EXISTS (SELECT 1 FROM events WHERE kind = 'company.created')",
        )
        .await
}

async fn append_bootstrap_event_once(
    database: &dyn Database,
    kind: &str,
    summary: &str,
    payload: &BlimsEventPayload,
) -> Result<(), switchy_database::DatabaseError> {
    let payload_json = serde_json::to_string(payload).expect("bootstrap event should encode");
    let existing = database
        .select("events")
        .columns(&["id"])
        .filter(Box::new(where_eq("kind", kind)))
        .filter(Box::new(where_eq("payload_json", payload_json.clone())))
        .limit(1)
        .execute_first(database)
        .await?;
    if existing.is_some() {
        return Ok(());
    }
    database
        .insert("events")
        .value("company_id", "default")
        .value("kind", kind)
        .value("summary", summary)
        .value("payload_json", payload_json)
        .value("correlation_id", "bootstrap")
        .value("causation_id", "bootstrap")
        .value("event_version", 1_i64)
        .execute(database)
        .await?;
    Ok(())
}

async fn seed_starter_org_events(
    database: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    for (id, name, purpose) in starter_departments() {
        let payload = BlimsEventPayload::DepartmentCreated {
            id: id.to_string(),
            name: name.to_string(),
            purpose: purpose.to_string(),
        };
        append_bootstrap_event_once(
            database,
            "department.created",
            &format!("Starter department created: {name}"),
            &payload,
        )
        .await?;
    }
    for (id, department_id, name, purpose) in starter_teams() {
        let payload = BlimsEventPayload::TeamCreated {
            id: id.to_string(),
            department_id: department_id.to_string(),
            name: name.to_string(),
            purpose: purpose.to_string(),
        };
        append_bootstrap_event_once(
            database,
            "team.created",
            &format!("Starter team created: {name}"),
            &payload,
        )
        .await?;
    }
    Ok(())
}

async fn seed_starter_world_events(
    database: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    let template = cozy_startup_loft_template();
    let payload = BlimsEventPayload::StarterOfficeSelected {
        template_id: template.id,
        theme: template.name,
        player_room_id: template.player_room_id,
    };
    append_bootstrap_event_once(
        database,
        "world.starter_office_selected",
        "Starter office selected: Cozy Startup Loft",
        &payload,
    )
    .await?;
    for room in fallback_world_snapshot().rooms {
        let payload = BlimsEventPayload::WorldRoomCreated { room: room.clone() };
        append_bootstrap_event_once(
            database,
            "world.room_created",
            &format!("Starter room created: {}", room.name),
            &payload,
        )
        .await?;
    }
    for object in starter_world_objects() {
        let payload = BlimsEventPayload::WorldObjectPlaced {
            object: object.clone(),
        };
        append_bootstrap_event_once(
            database,
            "world.object_placed",
            &format!("Starter object placed: {}", object.name),
            &payload,
        )
        .await?;
    }
    for agent in fallback_world_snapshot().agents {
        let payload = BlimsEventPayload::AgentHired {
            agent: agent.clone(),
        };
        append_bootstrap_event_once(
            database,
            "agent.hired",
            &format!("Starter agent hired: {}", agent.name),
            &payload,
        )
        .await?;
        let stats = starter_agent_stats(&agent.id);
        let payload = BlimsEventPayload::AgentStatsSet { stats };
        append_bootstrap_event_once(
            database,
            "agent.stats_set",
            &format!("Starter agent stats set: {}", agent.name),
            &payload,
        )
        .await?;
        for memory in starter_agent_memories(&agent.id) {
            let payload = BlimsEventPayload::AgentMemoryRecorded { memory };
            append_bootstrap_event_once(
                database,
                "agent.memory_recorded",
                &format!("Starter memory recorded: {}", agent.name),
                &payload,
            )
            .await?;
        }
        let permission = starter_agent_permission(&agent.id, &agent.role);
        let payload = BlimsEventPayload::AgentPermissionSet { permission };
        append_bootstrap_event_once(
            database,
            "agent.permission_set",
            &format!("Starter permission mapped: {}", agent.name),
            &payload,
        )
        .await?;
    }
    for relationship in starter_agent_relationships() {
        let payload = BlimsEventPayload::AgentRelationshipSet { relationship };
        append_bootstrap_event_once(
            database,
            "agent.relationship_set",
            "Starter agent relationship set",
            &payload,
        )
        .await?;
    }
    Ok(())
}

const fn starter_departments() -> [(&'static str, &'static str, &'static str); 3] {
    [
        (
            "product",
            "Product",
            "strategy, initiatives, and CEO reporting",
        ),
        (
            "engineering",
            "Engineering",
            "implementation, worktrees, tests, and review",
        ),
        (
            "creative",
            "Creative",
            "branding, docs tone, cozy design, and playful polish",
        ),
    ]
}

const fn starter_teams() -> [(&'static str, &'static str, &'static str, &'static str); 3] {
    [
        (
            "product-leads",
            "product",
            "Product Leads",
            "turn CEO guidance into company direction",
        ),
        (
            "engineering-pod",
            "engineering",
            "Engineering Pod",
            "ship safe code in worktrees",
        ),
        (
            "creative-studio",
            "creative",
            "Creative Studio",
            "make Blims delightful and memorable",
        ),
    ]
}

fn starter_agents() -> Vec<AgentRecord> {
    vec![
        AgentRecord {
            id: "mira".to_string(),
            name: "Mira".to_string(),
            role: "Product Lead".to_string(),
            department_id: "product".to_string(),
            team_id: "product-leads".to_string(),
            status: "waiting by the whiteboard".to_string(),
            room_id: "whiteboard".to_string(),
        },
        AgentRecord {
            id: "jules".to_string(),
            name: "Jules".to_string(),
            role: "Engineer".to_string(),
            department_id: "engineering".to_string(),
            team_id: "engineering-pod".to_string(),
            status: "setting up a workbench".to_string(),
            room_id: "engineering".to_string(),
        },
        AgentRecord {
            id: "pip".to_string(),
            name: "Pip".to_string(),
            role: "Creative Generalist".to_string(),
            department_id: "creative".to_string(),
            team_id: "creative-studio".to_string(),
            status: "sketching cozy office ideas".to_string(),
            room_id: "creative".to_string(),
        },
    ]
}

fn starter_agent_stats(agent_id: &str) -> AgentStatsRecord {
    match agent_id {
        "mira" => AgentStatsRecord {
            agent_id: agent_id.to_string(),
            traits: "curious, diplomatic, product-minded".to_string(),
            skills: "planning, prioritization, CEO synthesis".to_string(),
            energy: 82,
            morale: 88,
            focus: 74,
            confidence: 80,
            speed_modifier: 6,
            quality_modifier: 8,
            risk_modifier: -4,
            creativity_modifier: 9,
            persistence_modifier: 8,
            collaboration_modifier: 10,
        },
        "jules" => AgentStatsRecord {
            agent_id: agent_id.to_string(),
            traits: "careful, pragmatic, test-loving".to_string(),
            skills: "Rust, worktrees, validation, debugging".to_string(),
            energy: 78,
            morale: 80,
            focus: 90,
            confidence: 76,
            speed_modifier: 4,
            quality_modifier: 12,
            risk_modifier: -8,
            creativity_modifier: 4,
            persistence_modifier: 9,
            collaboration_modifier: 5,
        },
        _ => AgentStatsRecord {
            agent_id: agent_id.to_string(),
            traits: "playful, visual, encouraging".to_string(),
            skills: "copywriting, design, cozy polish, docs".to_string(),
            energy: 86,
            morale: 92,
            focus: 70,
            confidence: 84,
            speed_modifier: 7,
            quality_modifier: 6,
            risk_modifier: -2,
            creativity_modifier: 13,
            persistence_modifier: 7,
            collaboration_modifier: 9,
        },
    }
}

fn starter_agent_permission(agent_id: &str, role: &str) -> AgentPermissionRecord {
    let is_engineer = role.to_lowercase().contains("engineer");
    AgentPermissionRecord {
        agent_id: agent_id.to_string(),
        bcode_agent_id: format!("blims-{agent_id}"),
        bash: if is_engineer { "ask" } else { "deny" }.to_string(),
        read: "allow".to_string(),
        write: if is_engineer { "ask" } else { "deny" }.to_string(),
        edit: if is_engineer { "ask" } else { "deny" }.to_string(),
        external_directory: "deny".to_string(),
    }
}

fn starter_agent_relationships() -> Vec<AgentRelationshipRecord> {
    vec![
        AgentRelationshipRecord {
            agent_id: "mira".to_string(),
            other_agent_id: "jules".to_string(),
            affinity: 78,
            trust: 84,
            notes: "Mira trusts Jules to keep plans grounded in shippable slices.".to_string(),
        },
        AgentRelationshipRecord {
            agent_id: "jules".to_string(),
            other_agent_id: "pip".to_string(),
            affinity: 74,
            trust: 70,
            notes: "Jules appreciates Pip's polish when scope stays crisp.".to_string(),
        },
        AgentRelationshipRecord {
            agent_id: "pip".to_string(),
            other_agent_id: "mira".to_string(),
            affinity: 86,
            trust: 80,
            notes: "Pip looks to Mira for product direction before making things sparkly."
                .to_string(),
        },
    ]
}

fn starter_agent_memories(agent_id: &str) -> Vec<AgentMemoryRecord> {
    vec![AgentMemoryRecord {
        id: format!("{agent_id}-origin"),
        agent_id: agent_id.to_string(),
        kind: "origin".to_string(),
        summary: "Joined Blims at founding to make the company feel alive and useful.".to_string(),
        importance: 80,
    }]
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StarterWorldTemplate {
    id: String,
    name: String,
    description: String,
    flavor: String,
    player_room_id: String,
    width: i64,
    height: i64,
    rooms: Vec<RoomSnapshot>,
    objects: Vec<WorldObjectRecord>,
}

fn starter_world_templates() -> Vec<WorldTemplateSummary> {
    all_starter_world_templates()
        .into_iter()
        .map(|template| WorldTemplateSummary {
            id: template.id,
            name: template.name,
            description: template.description,
            rooms: template.rooms.len(),
            flavor: template.flavor,
        })
        .collect()
}

fn all_starter_world_templates() -> Vec<StarterWorldTemplate> {
    vec![
        cozy_startup_loft_template(),
        hacker_garage_template(),
        guild_hall_template(),
    ]
}

fn find_starter_world_template(template_id: &str) -> Option<StarterWorldTemplate> {
    all_starter_world_templates()
        .into_iter()
        .find(|template| template.id == template_id)
}

fn cozy_startup_loft_template() -> StarterWorldTemplate {
    StarterWorldTemplate {
        id: "cozy-startup-loft".to_string(),
        name: "Cozy Startup Loft".to_string(),
        description: "Warm default office for balanced planning, coding, creativity, and review."
            .to_string(),
        flavor: "balanced cozy productivity".to_string(),
        player_room_id: "ceo-nook".to_string(),
        width: 40,
        height: 14,
        rooms: cozy_startup_loft_rooms(),
        objects: cozy_startup_loft_objects(),
    }
}

fn fallback_world_snapshot() -> WorldSnapshot {
    let template = cozy_startup_loft_template();
    WorldSnapshot {
        theme: template.name,
        template_id: template.id,
        width: template.width,
        height: template.height,
        player_name: "CEO".to_string(),
        rooms: template.rooms,
        agents: starter_agents()
            .into_iter()
            .map(AgentRecord::snapshot)
            .collect(),
    }
}

fn cozy_startup_loft_rooms() -> Vec<RoomSnapshot> {
    vec![
        RoomSnapshot {
            id: "ceo-nook".to_string(),
            name: "CEO Nook".to_string(),
            purpose: "orientation, morning reports, and company controls".to_string(),
            room_kind: "meeting_room".to_string(),
            productivity_modifier: 3,
            x: 2,
            y: 1,
            symbol: "⌂".to_string(),
            color: "yellow".to_string(),
        },
        RoomSnapshot {
            id: "whiteboard".to_string(),
            name: "Whiteboard".to_string(),
            purpose: "initiatives, priorities, and planning".to_string(),
            room_kind: "meeting_room".to_string(),
            productivity_modifier: 6,
            x: 14,
            y: 1,
            symbol: "▦".to_string(),
            color: "bright-white".to_string(),
        },
        RoomSnapshot {
            id: "engineering".to_string(),
            name: "Engineering Desks".to_string(),
            purpose: "implementation focus and worktree coding".to_string(),
            room_kind: "lab".to_string(),
            productivity_modifier: 10,
            x: 2,
            y: 8,
            symbol: "⚙".to_string(),
            color: "cyan".to_string(),
        },
        RoomSnapshot {
            id: "creative".to_string(),
            name: "Creative Corner".to_string(),
            purpose: "branding, docs, and design ideas".to_string(),
            room_kind: "design_studio".to_string(),
            productivity_modifier: 8,
            x: 16,
            y: 8,
            symbol: "✦".to_string(),
            color: "magenta".to_string(),
        },
        RoomSnapshot {
            id: "review".to_string(),
            name: "Review Wall".to_string(),
            purpose: "approvals, proposals, and artifact review".to_string(),
            room_kind: "review_room".to_string(),
            productivity_modifier: 7,
            x: 28,
            y: 4,
            symbol: "✓".to_string(),
            color: "green".to_string(),
        },
    ]
}

fn hacker_garage_template() -> StarterWorldTemplate {
    StarterWorldTemplate {
        id: "hacker-garage".to_string(),
        name: "Hacker Garage".to_string(),
        description: "A neon tinkering garage tuned for fast experiments and scrappy builds."
            .to_string(),
        flavor: "fast experiments and debugging".to_string(),
        player_room_id: "garage-bay".to_string(),
        width: 44,
        height: 14,
        rooms: vec![
            RoomSnapshot {
                id: "garage-bay".to_string(),
                name: "Garage Bay".to_string(),
                purpose: "CEO standups beside the rolling door".to_string(),
                room_kind: "meeting_room".to_string(),
                productivity_modifier: 2,
                x: 2,
                y: 2,
                symbol: "▤".to_string(),
                color: "yellow".to_string(),
            },
            RoomSnapshot {
                id: "debug-bench".to_string(),
                name: "Debug Bench".to_string(),
                purpose: "instrumentation, bug hunts, and validation loops".to_string(),
                room_kind: "lab".to_string(),
                productivity_modifier: 12,
                x: 14,
                y: 2,
                symbol: "⚡".to_string(),
                color: "cyan".to_string(),
            },
            RoomSnapshot {
                id: "parts-wall".to_string(),
                name: "Parts Wall".to_string(),
                purpose: "research snippets, reusable ideas, and tool notes".to_string(),
                room_kind: "library".to_string(),
                productivity_modifier: 6,
                x: 28,
                y: 2,
                symbol: "▣".to_string(),
                color: "blue".to_string(),
            },
            RoomSnapshot {
                id: "shipping-lane".to_string(),
                name: "Shipping Lane".to_string(),
                purpose: "review, package, and hand off completed work".to_string(),
                room_kind: "review_room".to_string(),
                productivity_modifier: 8,
                x: 8,
                y: 9,
                symbol: "➜".to_string(),
                color: "green".to_string(),
            },
        ],
        objects: vec![
            WorldObjectRecord {
                id: "garage-build-board".to_string(),
                room_id: "garage-bay".to_string(),
                name: "Build Board".to_string(),
                kind: "planning_hotspot".to_string(),
                symbol: "▦".to_string(),
                color: "bright-white".to_string(),
                interaction: "Prioritize experiments and active builds".to_string(),
                productivity_modifier: 6,
            },
            WorldObjectRecord {
                id: "debug-oscilloscope".to_string(),
                room_id: "debug-bench".to_string(),
                name: "Debug Oscilloscope".to_string(),
                kind: "engineering_station".to_string(),
                symbol: "⌁".to_string(),
                color: "cyan".to_string(),
                interaction: "Inspect failing checks and validation traces".to_string(),
                productivity_modifier: 12,
            },
            WorldObjectRecord {
                id: "shipping-crate".to_string(),
                room_id: "shipping-lane".to_string(),
                name: "Shipping Crate".to_string(),
                kind: "approval_hotspot".to_string(),
                symbol: "□".to_string(),
                color: "green".to_string(),
                interaction: "Review ready patches before shipping".to_string(),
                productivity_modifier: 8,
            },
        ],
    }
}

fn guild_hall_template() -> StarterWorldTemplate {
    StarterWorldTemplate {
        id: "guild-hall".to_string(),
        name: "Guild Hall".to_string(),
        description:
            "A collaborative hall for departments, rituals, long-running quests, and craft review."
                .to_string(),
        flavor: "collaboration and deep craft".to_string(),
        player_room_id: "throne-table".to_string(),
        width: 46,
        height: 16,
        rooms: vec![
            RoomSnapshot {
                id: "throne-table".to_string(),
                name: "Round CEO Table".to_string(),
                purpose: "company direction, counsel, and morning reports".to_string(),
                room_kind: "meeting_room".to_string(),
                productivity_modifier: 5,
                x: 3,
                y: 2,
                symbol: "◉".to_string(),
                color: "yellow".to_string(),
            },
            RoomSnapshot {
                id: "quest-board".to_string(),
                name: "Quest Board".to_string(),
                purpose: "initiatives, tasks, blockers, and party assignments".to_string(),
                room_kind: "meeting_room".to_string(),
                productivity_modifier: 8,
                x: 17,
                y: 2,
                symbol: "※".to_string(),
                color: "bright-white".to_string(),
            },
            RoomSnapshot {
                id: "scribe-library".to_string(),
                name: "Scribe Library".to_string(),
                purpose: "docs, research, memory, and non-code artifacts".to_string(),
                room_kind: "library".to_string(),
                productivity_modifier: 10,
                x: 31,
                y: 2,
                symbol: "☰".to_string(),
                color: "blue".to_string(),
            },
            RoomSnapshot {
                id: "forge".to_string(),
                name: "Code Forge".to_string(),
                purpose: "implementation work with careful validation".to_string(),
                room_kind: "lab".to_string(),
                productivity_modifier: 11,
                x: 9,
                y: 10,
                symbol: "⚒".to_string(),
                color: "cyan".to_string(),
            },
            RoomSnapshot {
                id: "council-review".to_string(),
                name: "Council Review".to_string(),
                purpose: "approvals, tradeoffs, and contract review".to_string(),
                room_kind: "review_room".to_string(),
                productivity_modifier: 9,
                x: 27,
                y: 10,
                symbol: "✓".to_string(),
                color: "green".to_string(),
            },
        ],
        objects: vec![
            WorldObjectRecord {
                id: "guild-quest-board".to_string(),
                room_id: "quest-board".to_string(),
                name: "Quest Board".to_string(),
                kind: "planning_hotspot".to_string(),
                symbol: "※".to_string(),
                color: "bright-white".to_string(),
                interaction: "Review quests, priorities, and blockers".to_string(),
                productivity_modifier: 9,
            },
            WorldObjectRecord {
                id: "scribe-desk".to_string(),
                room_id: "scribe-library".to_string(),
                name: "Scribe Desk".to_string(),
                kind: "creative_station".to_string(),
                symbol: "✎".to_string(),
                color: "blue".to_string(),
                interaction: "Draft docs, briefs, and research summaries".to_string(),
                productivity_modifier: 10,
            },
            WorldObjectRecord {
                id: "forge-anvil".to_string(),
                room_id: "forge".to_string(),
                name: "Validation Anvil".to_string(),
                kind: "engineering_station".to_string(),
                symbol: "⚒".to_string(),
                color: "cyan".to_string(),
                interaction: "Run implementation and validation loops".to_string(),
                productivity_modifier: 11,
            },
        ],
    }
}

fn cozy_startup_loft_objects() -> Vec<WorldObjectRecord> {
    vec![
        WorldObjectRecord {
            id: "whiteboard-roadmap".to_string(),
            room_id: "whiteboard".to_string(),
            name: "Roadmap Whiteboard".to_string(),
            kind: "planning_hotspot".to_string(),
            symbol: "▦".to_string(),
            color: "bright-white".to_string(),
            interaction: "Review initiatives and CEO guidance".to_string(),
            productivity_modifier: 8,
        },
        WorldObjectRecord {
            id: "engineering-workbench".to_string(),
            room_id: "engineering".to_string(),
            name: "Worktree Workbench".to_string(),
            kind: "engineering_station".to_string(),
            symbol: "⚙".to_string(),
            color: "cyan".to_string(),
            interaction: "Review tasks and start implementation work".to_string(),
            productivity_modifier: 10,
        },
        WorldObjectRecord {
            id: "creative-rug".to_string(),
            room_id: "creative".to_string(),
            name: "Idea Rug".to_string(),
            kind: "creative_station".to_string(),
            symbol: "✦".to_string(),
            color: "magenta".to_string(),
            interaction: "Brainstorm delightful artifacts and docs".to_string(),
            productivity_modifier: 7,
        },
        WorldObjectRecord {
            id: "review-wall".to_string(),
            room_id: "review".to_string(),
            name: "Review Wall".to_string(),
            kind: "approval_hotspot".to_string(),
            symbol: "✓".to_string(),
            color: "green".to_string(),
            interaction: "Review proposals, patches, and artifacts".to_string(),
            productivity_modifier: 6,
        },
    ]
}

fn starter_world_objects() -> Vec<WorldObjectRecord> {
    cozy_startup_loft_objects()
}

fn fallback_morning_report() -> MorningReport {
    MorningReport {
        title: "Blims morning report".to_string(),
        bullets: vec![
            "The Blims plugin is now available as a bundled service stub.".to_string(),
            "Repo-local SQLite state initialization is available through company.create."
                .to_string(),
            "Starter office direction: Cozy Startup Loft, Hacker Garage, and Guild Hall."
                .to_string(),
        ],
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompanyData {
    company: CompanyRecord,
    world_theme: String,
    player_room_id: String,
    rooms: Vec<RoomRecord>,
    agents: Vec<AgentRecord>,
    departments: Vec<DepartmentRecord>,
    teams: Vec<TeamRecord>,
    initiatives: Vec<InitiativeSummary>,
    projects: Vec<ProjectSummary>,
    guidance: Vec<GuidanceSummary>,
    proposals: Vec<WorkProposalSummary>,
    tasks: Vec<TaskSummary>,
    artifacts: Vec<ArtifactSummary>,
    contract_violations: Vec<ContractViolationRecord>,
    agent_stats: Vec<AgentStatsRecord>,
    relationships: Vec<AgentRelationshipRecord>,
    memories: Vec<AgentMemoryRecord>,
    permissions: Vec<AgentPermissionRecord>,
    permission_escalations: Vec<PermissionEscalationRecord>,
    world_objects: Vec<WorldObjectRecord>,
}

fn with_database<T>(
    working_directory: &Path,
    operation: impl for<'a> FnOnce(
        &'a dyn Database,
    )
        -> Pin<Box<dyn Future<Output = Result<T, BlimsStateError>> + 'a>>
    + Send
    + 'static,
) -> Result<T, BlimsStateError>
where
    T: Send + 'static,
{
    let paths = state_paths(working_directory);
    if !paths.database_path.exists() {
        return Err(BlimsStateError::StateMissing(paths.database_path));
    }

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread Tokio runtime should build");
        runtime.block_on(async {
            let database = switchy_database_connection::init(Some(&paths.database_path), None)
                .await
                .map_err(|source| BlimsStateError::InitDatabase {
                    path: paths.database_path.clone(),
                    source,
                })?;
            initialize_schema(database.as_ref()).await?;
            operation(database.as_ref()).await
        })
    })
    .join()
    .map_err(panic_to_blims_error)?
}

async fn append_event(
    database: &dyn Database,
    event_context: &EventContext,
    kind: &str,
    summary: String,
    payload: &BlimsEventPayload,
) -> Result<(), BlimsStateError> {
    let payload_json = serde_json::to_string(payload)?;
    check_expected_latest_event_id(database, event_context.expected_latest).await?;
    database
        .insert("events")
        .value("company_id", "default")
        .value("kind", kind)
        .value("summary", summary)
        .value("payload_json", payload_json)
        .value("correlation_id", event_context.correlation.clone())
        .value("causation_id", event_context.causation.clone())
        .value("event_version", 1_i64)
        .execute(database)
        .await?;
    apply_event_projection(database, payload).await
}

async fn check_expected_latest_event_id(
    database: &dyn Database,
    expected_latest_event_id: Option<i64>,
) -> Result<(), BlimsStateError> {
    let Some(expected) = expected_latest_event_id else {
        return Ok(());
    };
    let actual = latest_event_id(database).await?;
    if actual == expected {
        Ok(())
    } else {
        Err(BlimsStateError::EventConflict { expected, actual })
    }
}

async fn latest_event_id(database: &dyn Database) -> Result<i64, switchy_database::DatabaseError> {
    Ok(database
        .select("events")
        .columns(&["id"])
        .sort("id", SortDirection::Desc)
        .limit(1)
        .execute_first(database)
        .await?
        .as_ref()
        .and_then(|row| row.get("id"))
        .and_then(|value| value.as_i64())
        .unwrap_or_default())
}

async fn apply_event_projection(
    database: &dyn Database,
    payload: &BlimsEventPayload,
) -> Result<(), BlimsStateError> {
    if apply_work_event_projection(database, payload).await? {
        return Ok(());
    }
    apply_org_world_event_projection(database, payload).await
}

#[allow(clippy::too_many_lines)]
async fn apply_work_event_projection(
    database: &dyn Database,
    payload: &BlimsEventPayload,
) -> Result<bool, BlimsStateError> {
    match payload {
        BlimsEventPayload::CompanyLifecycleSet { lifecycle_status } => {
            database
                .update("companies")
                .value("lifecycle_status", lifecycle_status.clone())
                .filter(Box::new(where_eq("id", "default")))
                .execute(database)
                .await?;
        }
        BlimsEventPayload::CommandSubmitted {
            command_id,
            actor,
            frontend_id,
            kind,
            priority,
        } => {
            let operation_id = format!("op-{command_id}");
            upsert_operation(
                database,
                &OperationProjectionRecord {
                    id: &operation_id,
                    command_id,
                    actor,
                    frontend_id: frontend_id.as_deref().unwrap_or_default(),
                    kind,
                    priority,
                    status: BlimsOperationStatus::Queued.as_str(),
                    result_event_id: None,
                    error: "",
                },
            )
            .await?;
        }
        BlimsEventPayload::OperationStatusSet {
            operation_id,
            status,
            error,
        } => {
            database
                .update("operations")
                .value("status", status.clone())
                .value("error", error.clone().unwrap_or_default())
                .filter(Box::new(where_eq("id", operation_id.clone())))
                .execute(database)
                .await?;
        }
        BlimsEventPayload::DashboardProjectionRefreshed { operation_id } => {
            database
                .update("operations")
                .value("status", BlimsOperationStatus::Completed.as_str())
                .value("error", "")
                .filter(Box::new(where_eq("id", operation_id.clone())))
                .execute(database)
                .await?;
        }
        BlimsEventPayload::InitiativeCreated { initiative } => {
            replace_one_initiative_projection(database, initiative).await?;
        }
        BlimsEventPayload::ProjectCreated { project } => {
            replace_one_project_projection(database, project).await?;
        }
        BlimsEventPayload::InitiativeStatusSet {
            initiative_id,
            status,
        } => {
            database
                .update("initiatives")
                .value("status", status.clone())
                .filter(Box::new(where_eq("id", initiative_id.clone())))
                .execute(database)
                .await?;
        }
        BlimsEventPayload::GuidanceSet { guidance } => {
            replace_one_guidance_projection(database, guidance).await?;
        }
        BlimsEventPayload::InitiativeGuidanceSet {
            initiative_id,
            guidance,
        } => {
            replace_one_guidance_projection(database, guidance).await?;
            database
                .update("initiatives")
                .value("guidance", guidance.guidance.clone())
                .filter(Box::new(where_eq("id", initiative_id.clone())))
                .execute(database)
                .await?;
        }
        BlimsEventPayload::ProposalRegistered { proposal } => {
            replace_one_proposal_projection(database, proposal).await?;
        }
        BlimsEventPayload::WorktreeRecorded { worktree } => {
            replace_one_worktree_projection(database, worktree).await?;
        }
        BlimsEventPayload::ProposalStatusSet {
            proposal_id,
            status,
        } => {
            database
                .update("work_proposals")
                .value("status", status.clone())
                .filter(Box::new(where_eq("id", proposal_id.clone())))
                .execute(database)
                .await?;
        }
        BlimsEventPayload::ArtifactCreated { artifact } => {
            replace_one_artifact_projection(database, artifact, "draft").await?;
        }
        BlimsEventPayload::ArtifactStatusSet {
            artifact_id,
            status,
        } => {
            database
                .update("artifacts")
                .value("status", status.clone())
                .filter(Box::new(where_eq("id", artifact_id.clone())))
                .execute(database)
                .await?;
        }
        BlimsEventPayload::TaskCreated { task } => {
            replace_one_task_projection(database, task).await?;
        }
        BlimsEventPayload::TaskOutcomeRecorded { stats, .. } => {
            replace_one_agent_stats_projection(database, stats).await?;
        }
        BlimsEventPayload::ReportGenerated { report } => {
            replace_one_report_projection(database, report).await?;
        }
        BlimsEventPayload::ConversationRecorded { conversation } => {
            replace_one_conversation_projection(database, conversation).await?;
        }
        BlimsEventPayload::ConversationMessageRecorded {
            conversation_id,
            agent_id,
            ..
        } => {
            let conversation = ConversationRecord {
                id: conversation_id.clone(),
                agent_id: agent_id.clone(),
                session_id: String::new(),
                status: "open".to_string(),
                summary: "Conversation message recorded.".to_string(),
            };
            replace_one_conversation_projection(database, &conversation).await?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

#[allow(clippy::too_many_lines)]
async fn apply_org_world_event_projection(
    database: &dyn Database,
    payload: &BlimsEventPayload,
) -> Result<(), BlimsStateError> {
    match payload {
        BlimsEventPayload::AgentHired { agent } => {
            replace_one_agent_projection(database, &agent.clone().into()).await?;
        }
        BlimsEventPayload::AgentMoved { agent_id, room_id } => {
            database
                .update("agents")
                .value("room_id", room_id.clone())
                .filter(Box::new(where_eq("id", agent_id.clone())))
                .execute(database)
                .await?;
        }
        BlimsEventPayload::AgentStatsSet { stats } => {
            replace_one_agent_stats_projection(database, stats).await?;
        }
        BlimsEventPayload::AgentRelationshipSet { relationship } => {
            replace_one_agent_relationship_projection(database, relationship).await?;
        }
        BlimsEventPayload::AgentMemoryRecorded { memory } => {
            replace_one_agent_memory_projection(database, memory).await?;
        }
        BlimsEventPayload::AgentPermissionSet { permission } => {
            replace_one_agent_permission_projection(database, permission).await?;
        }
        BlimsEventPayload::PermissionEscalationRequested { request } => {
            replace_one_permission_escalation_projection(database, request).await?;
        }
        BlimsEventPayload::ContractViolationRecorded { violation } => {
            replace_one_contract_violation_projection(database, violation).await?;
        }
        BlimsEventPayload::AgentStatusSet { agent_id, status } => {
            database
                .update("agents")
                .value("status", status.clone())
                .filter(Box::new(where_eq("id", agent_id.clone())))
                .execute(database)
                .await?;
        }
        BlimsEventPayload::AgentContractUpdated {
            agent_id,
            responsibilities,
            restrictions,
            escalation,
        } => {
            replace_one_agent_contract_projection(
                database,
                agent_id,
                responsibilities,
                restrictions,
                escalation,
            )
            .await?;
        }
        BlimsEventPayload::DepartmentCreated { id, name, purpose } => {
            replace_one_department_projection(
                database,
                &DepartmentRecord {
                    id: id.clone(),
                    name: name.clone(),
                    purpose: purpose.clone(),
                },
            )
            .await?;
        }
        BlimsEventPayload::TeamCreated {
            id,
            department_id,
            name,
            purpose,
        } => {
            replace_one_team_projection(
                database,
                &TeamRecord {
                    id: id.clone(),
                    department_id: department_id.clone(),
                    name: name.clone(),
                    purpose: purpose.clone(),
                },
            )
            .await?;
        }
        BlimsEventPayload::WorldRoomCreated { room } => {
            replace_one_world_room_projection(database, &room.clone().into()).await?;
        }
        BlimsEventPayload::WorldObjectPlaced { object } => {
            replace_one_world_object_projection(database, object).await?;
        }
        BlimsEventPayload::PlayerMoved { room_id } => {
            database
                .update("worlds")
                .value("player_room_id", room_id.clone())
                .filter(Box::new(where_eq("id", "default")))
                .execute(database)
                .await?;
        }
        BlimsEventPayload::StarterOfficeSelected {
            theme,
            player_room_id,
            ..
        } => {
            database
                .update("worlds")
                .value("theme", theme.clone())
                .value("player_room_id", player_room_id.clone())
                .filter(Box::new(where_eq("id", "default")))
                .execute(database)
                .await?;
        }
        BlimsEventPayload::InitiativePlanImported { .. }
        | BlimsEventPayload::ConversationMessageRecorded { .. }
        | BlimsEventPayload::CommandSubmitted { .. }
        | BlimsEventPayload::OperationStatusSet { .. }
        | BlimsEventPayload::DashboardProjectionRefreshed { .. }
        | BlimsEventPayload::CompanyLifecycleSet { .. }
        | BlimsEventPayload::InitiativeCreated { .. }
        | BlimsEventPayload::ProjectCreated { .. }
        | BlimsEventPayload::InitiativeStatusSet { .. }
        | BlimsEventPayload::GuidanceSet { .. }
        | BlimsEventPayload::InitiativeGuidanceSet { .. }
        | BlimsEventPayload::ProposalRegistered { .. }
        | BlimsEventPayload::WorktreeRecorded { .. }
        | BlimsEventPayload::ProposalStatusSet { .. }
        | BlimsEventPayload::ArtifactCreated { .. }
        | BlimsEventPayload::ArtifactStatusSet { .. }
        | BlimsEventPayload::TaskCreated { .. }
        | BlimsEventPayload::TaskOutcomeRecorded { .. }
        | BlimsEventPayload::AgentPlanningCycleRecorded { .. }
        | BlimsEventPayload::AiWorkPrepared { .. }
        | BlimsEventPayload::TaskWorkScheduled { .. }
        | BlimsEventPayload::ReportGenerated { .. }
        | BlimsEventPayload::ConversationRecorded { .. } => {}
    }
    Ok(())
}

fn load_company_data(working_directory: &Path) -> Result<CompanyData, BlimsStateError> {
    with_database(working_directory, |database| {
        Box::pin(load_company_data_from_database(database))
    })
}

fn inspect_agent(request: &AgentRequest) -> Result<AgentSnapshot, BlimsStateError> {
    let agent_id = request.agent_id.clone();
    load_company_data(&request.working_directory).and_then(|data| {
        data.agents
            .into_iter()
            .find(|agent| agent.id == agent_id)
            .map(AgentRecord::snapshot)
            .ok_or_else(|| BlimsStateError::InvalidRequest(format!("unknown agent: {agent_id}")))
    })
}

fn inspect_agent_permission(
    request: &AgentRequest,
) -> Result<AgentPermissionRecord, BlimsStateError> {
    let agent_id = request.agent_id.clone();
    load_company_data(&request.working_directory).and_then(|data| {
        data.permissions
            .into_iter()
            .find(|permission| permission.agent_id == agent_id)
            .ok_or_else(|| {
                BlimsStateError::InvalidRequest(format!("unknown agent permission: {agent_id}"))
            })
    })
}

fn set_agent_permission(
    request: &AgentPermissionUpdateRequest,
    event_context: &EventContext,
) -> Result<AgentPermissionRecord, BlimsStateError> {
    validate_permission_policy("bash", &request.bash)?;
    validate_permission_policy("read", &request.read)?;
    validate_permission_policy("write", &request.write)?;
    validate_permission_policy("edit", &request.edit)?;
    validate_permission_policy("external_directory", &request.external_directory)?;
    let permission = AgentPermissionRecord {
        agent_id: request.agent_id.clone(),
        bcode_agent_id: request.bcode_agent_id.clone(),
        bash: request.bash.clone(),
        read: request.read.clone(),
        write: request.write.clone(),
        edit: request.edit.clone(),
        external_directory: request.external_directory.clone(),
    };
    let event_context = event_context.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            append_event(
                database,
                &event_context,
                "agent.permission_set",
                format!("Agent permission updated: {}", permission.agent_id),
                &BlimsEventPayload::AgentPermissionSet {
                    permission: permission.clone(),
                },
            )
            .await?;
            Ok::<_, BlimsStateError>(permission)
        })
    })
}

fn validate_permission_policy(category: &str, policy: &str) -> Result<(), BlimsStateError> {
    if matches!(policy, "allow" | "ask" | "deny") {
        Ok(())
    } else {
        Err(BlimsStateError::InvalidRequest(format!(
            "{category} policy must be allow, ask, or deny"
        )))
    }
}

fn request_permission_escalation(
    request: &PermissionEscalationRequest,
    event_context: &EventContext,
) -> Result<PermissionEscalationRecord, BlimsStateError> {
    let escalation = PermissionEscalationRecord {
        id: format!(
            "permission-{}-{}",
            request.agent_id,
            stable_slug(&request.category)
        ),
        agent_id: request.agent_id.clone(),
        category: request.category.clone(),
        requested_policy: request.requested_policy.clone(),
        reason: request.reason.clone(),
        status: "pending".to_string(),
    };
    let event_context = event_context.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            append_event(
                database,
                &event_context,
                "permission.escalation_requested",
                format!("Permission escalation requested: {}", escalation.id),
                &BlimsEventPayload::PermissionEscalationRequested {
                    request: escalation.clone(),
                },
            )
            .await?;
            Ok::<_, BlimsStateError>(escalation)
        })
    })
}

fn record_conversation(
    request: &ConversationRecordRequest,
    event_context: &EventContext,
) -> Result<ConversationRecord, BlimsStateError> {
    let conversation_id = request.conversation_id.clone();
    let agent_id = request.agent_id.clone();
    let session_id = request.session_id.clone();
    let summary = request.summary.clone();
    let event_context = event_context.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            record_conversation_in_database(
                database,
                &conversation_id,
                &agent_id,
                &session_id,
                &summary,
                &event_context,
            )
            .await
        })
    })
}

async fn record_conversation_in_database(
    database: &dyn Database,
    conversation_id: &str,
    agent_id: &str,
    session_id: &str,
    summary: &str,
    event_context: &EventContext,
) -> Result<ConversationRecord, BlimsStateError> {
    let conversation = ConversationRecord {
        id: conversation_id.to_string(),
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        status: "open".to_string(),
        summary: summary.to_string(),
    };
    append_event(
        database,
        event_context,
        "conversation.recorded",
        format!("Conversation recorded: {}", conversation.id),
        &BlimsEventPayload::ConversationRecorded {
            conversation: conversation.clone(),
        },
    )
    .await?;
    Ok(conversation)
}

async fn record_conversation_message_in_database(
    database: &dyn Database,
    conversation_id: &str,
    agent_id: &str,
    speaker: &str,
    message: &str,
    event_context: &EventContext,
) -> Result<(), BlimsStateError> {
    if message.trim().is_empty() {
        return Err(BlimsStateError::InvalidRequest(
            "conversation message cannot be empty".to_string(),
        ));
    }
    append_event(
        database,
        event_context,
        "conversation.message_recorded",
        format!("Conversation message recorded for {agent_id}."),
        &BlimsEventPayload::ConversationMessageRecorded {
            conversation_id: conversation_id.to_string(),
            agent_id: agent_id.to_string(),
            speaker: speaker.to_string(),
            message: message.to_string(),
        },
    )
    .await
}

fn hire_agent(
    request: &AgentHireRequest,
    event_context: &EventContext,
) -> Result<AgentSnapshot, BlimsStateError> {
    let agent = AgentSnapshot {
        id: stable_slug(&request.agent_id),
        name: request.name.trim().to_string(),
        role: request.role.trim().to_string(),
        status: "active".to_string(),
        room_id: request.room_id.clone(),
    };
    if agent.id.is_empty() || agent.name.is_empty() || agent.role.is_empty() {
        return Err(BlimsStateError::InvalidRequest(
            "agent id, name, and role are required".to_string(),
        ));
    }
    let event_context = event_context.clone();
    let working_directory = request.working_directory.clone();
    let response_agent = agent.clone();
    with_database(&working_directory, move |database| {
        Box::pin(async move {
            append_event(
                database,
                &event_context,
                "agent.hired",
                format!("Agent hired: {}", agent.name),
                &BlimsEventPayload::AgentHired { agent },
            )
            .await?;
            Ok::<_, BlimsStateError>(response_agent)
        })
    })
}

fn set_agent_status(
    request: &AgentRequest,
    event_context: &EventContext,
    status: &str,
) -> Result<AgentSnapshot, BlimsStateError> {
    let agent = inspect_agent(request)?;
    let agent_id = request.agent_id.clone();
    let status = status.to_string();
    let event_context = event_context.clone();
    let working_directory = request.working_directory.clone();
    with_database(&working_directory, move |database| {
        Box::pin(async move {
            append_event(
                database,
                &event_context,
                "agent.status_set",
                format!("Agent {agent_id} status set to {status}."),
                &BlimsEventPayload::AgentStatusSet {
                    agent_id,
                    status: status.clone(),
                },
            )
            .await?;
            Ok::<_, BlimsStateError>(AgentSnapshot { status, ..agent })
        })
    })
}

fn move_agent(
    request: &AgentMoveRequest,
    event_context: &EventContext,
) -> Result<AgentSnapshot, BlimsStateError> {
    let agent = inspect_agent(&AgentRequest {
        working_directory: request.working_directory.clone(),
        agent_id: request.agent_id.clone(),
        correlation_id: None,
        causation_id: None,
        expected_latest_event_id: None,
    })?;
    let room_id = request.room_id.trim().to_string();
    if room_id.is_empty() {
        return Err(BlimsStateError::InvalidRequest(
            "room_id cannot be empty".to_string(),
        ));
    }
    let agent_id = request.agent_id.clone();
    let working_directory = request.working_directory.clone();
    let event_context = event_context.merge_agent_move_request(request);
    with_database(&working_directory, move |database| {
        Box::pin(async move {
            let room_exists = database
                .select("world_rooms")
                .columns(&["id"])
                .filter(Box::new(where_eq("id", room_id.clone())))
                .limit(1)
                .execute_first(database)
                .await?
                .is_some();
            if !room_exists {
                return Err(BlimsStateError::InvalidRequest(format!(
                    "unknown room: {room_id}"
                )));
            }
            append_event(
                database,
                &event_context,
                "agent.moved",
                format!("Agent {agent_id} walked to {room_id}."),
                &BlimsEventPayload::AgentMoved {
                    agent_id,
                    room_id: room_id.clone(),
                },
            )
            .await?;
            Ok::<_, BlimsStateError>(AgentSnapshot { room_id, ..agent })
        })
    })
}

fn update_agent_contract(
    request: &AgentContractUpdateRequest,
    event_context: &EventContext,
) -> Result<AgentContractUpdateRequest, BlimsStateError> {
    inspect_agent(&AgentRequest {
        working_directory: request.working_directory.clone(),
        agent_id: request.agent_id.clone(),
        correlation_id: None,
        causation_id: None,
        expected_latest_event_id: None,
    })?;
    let request = request.clone();
    let working_directory = request.working_directory.clone();
    let event_context = event_context.clone();
    with_database(&working_directory, move |database| {
        Box::pin(async move {
            append_event(
                database,
                &event_context,
                "agent.contract_updated",
                format!("Agent {} contract updated.", request.agent_id),
                &BlimsEventPayload::AgentContractUpdated {
                    agent_id: request.agent_id.clone(),
                    responsibilities: request.responsibilities.clone(),
                    restrictions: request.restrictions.clone(),
                    escalation: request.escalation.clone(),
                },
            )
            .await?;
            Ok::<_, BlimsStateError>(request)
        })
    })
}

fn world_snapshot(working_directory: &Path) -> Result<WorldSnapshot, BlimsStateError> {
    load_company_data(working_directory).map(world_snapshot_from_data)
}

fn template_id_for_theme(theme: &str) -> String {
    all_starter_world_templates()
        .into_iter()
        .find(|template| template.name == theme)
        .map_or_else(|| stable_slug(theme), |template| template.id)
}

fn world_snapshot_from_data(data: CompanyData) -> WorldSnapshot {
    let template_id = template_id_for_theme(&data.world_theme);
    let (width, height) = find_starter_world_template(&template_id)
        .map_or((40, 14), |template| (template.width, template.height));
    WorldSnapshot {
        theme: data.world_theme,
        template_id,
        width,
        height,
        player_name: "CEO".to_string(),
        rooms: data
            .rooms
            .into_iter()
            .map(|room| RoomSnapshot {
                id: room.id,
                name: room.name,
                purpose: room.purpose,
                room_kind: room.room_kind,
                productivity_modifier: room.productivity_modifier,
                x: room.x,
                y: room.y,
                symbol: room.symbol,
                color: room.color,
            })
            .collect(),
        agents: data
            .agents
            .into_iter()
            .map(|agent| AgentSnapshot {
                id: agent.id,
                name: agent.name,
                role: agent.role,
                status: agent.status,
                room_id: agent.room_id,
            })
            .collect(),
    }
}

fn move_player(
    request: &WorldMovePlayerRequest,
    event_context: &EventContext,
) -> Result<WorldSnapshot, BlimsStateError> {
    let room_id = request.room_id.trim().to_string();
    let event_context = event_context.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            move_player_in_database(database, &room_id, &event_context).await?;
            let data = load_company_data_from_database(database).await?;
            Ok(world_snapshot_from_data(data))
        })
    })
}

async fn move_player_in_database(
    database: &dyn Database,
    room_id: &str,
    event_context: &EventContext,
) -> Result<(), BlimsStateError> {
    let room_id = room_id.trim().to_string();
    if room_id.is_empty() {
        return Err(BlimsStateError::InvalidRequest(
            "room_id cannot be empty".to_string(),
        ));
    }
    let room_exists = database
        .select("world_rooms")
        .columns(&["id"])
        .filter(Box::new(where_eq("id", room_id.clone())))
        .limit(1)
        .execute_first(database)
        .await?
        .is_some();
    if !room_exists {
        return Err(BlimsStateError::InvalidRequest(format!(
            "unknown room: {room_id}"
        )));
    }
    append_event(
        database,
        event_context,
        "player.moved",
        format!("CEO moved to room {room_id}."),
        &BlimsEventPayload::PlayerMoved { room_id },
    )
    .await
}

fn tick_world(
    request: &WorldTickRequest,
    event_context: &EventContext,
) -> Result<WorldSnapshot, BlimsStateError> {
    let event_context = event_context.merge_world_tick_request(request);
    let tick_id = request.tick_id.clone();
    let now_ms = request.now_ms;
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            tick_world_in_database(database, tick_id.as_deref(), now_ms, &event_context).await?;
            let data = load_company_data_from_database(database).await?;
            Ok(world_snapshot_from_data(data))
        })
    })
}

async fn tick_world_in_database(
    database: &dyn Database,
    tick_id: Option<&str>,
    now_ms: Option<i64>,
    event_context: &EventContext,
) -> Result<(), BlimsStateError> {
    let tick_seed = now_ms
        .unwrap_or_default()
        .saturating_add(stable_i64(tick_id.unwrap_or("tick")));
    let data = load_company_data_from_database(database).await?;
    if data.company.lifecycle_status != "running" || data.rooms.is_empty() || data.agents.is_empty()
    {
        return Ok(());
    }
    for (index, agent) in data.agents.iter().enumerate() {
        let decision = tick_decision(tick_seed, index, agent, &data.rooms);
        if let Some(room_id) = decision.room_id
            && room_id != agent.room_id
        {
            append_event(
                database,
                event_context,
                "agent.moved",
                format!("{} walked to {room_id}.", agent.name),
                &BlimsEventPayload::AgentMoved {
                    agent_id: agent.id.clone(),
                    room_id,
                },
            )
            .await?;
        }
        if decision.status != agent.status {
            append_event(
                database,
                event_context,
                "agent.status_set",
                format!("{} is now {}.", agent.name, decision.status),
                &BlimsEventPayload::AgentStatusSet {
                    agent_id: agent.id.clone(),
                    status: decision.status,
                },
            )
            .await?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentTickDecision {
    room_id: Option<String>,
    status: String,
}

fn tick_decision(
    tick_seed: i64,
    index: usize,
    agent: &AgentRecord,
    rooms: &[RoomRecord],
) -> AgentTickDecision {
    let seed = tick_seed
        .saturating_add(stable_i64(&agent.id))
        .saturating_add(i64::try_from(index).unwrap_or_default() * 17);
    let room_id = preferred_agent_room(agent, rooms, seed).and_then(|room| {
        if room.id == agent.room_id && rooms.len() > 1 {
            fallback_agent_room(agent, rooms, seed).map(|fallback| fallback.id.clone())
        } else {
            Some(room.id.clone())
        }
    });
    AgentTickDecision {
        room_id,
        status: agent_activity(agent, seed).to_string(),
    }
}

fn preferred_agent_room<'a>(
    agent: &AgentRecord,
    rooms: &'a [RoomRecord],
    seed: i64,
) -> Option<&'a RoomRecord> {
    let role = agent.role.to_lowercase();
    let preferred_kind = if role.contains("engineer") || role.contains("developer") {
        "engineering"
    } else if role.contains("design") || role.contains("creative") {
        "creative"
    } else if role.contains("review") || role.contains("qa") {
        "review"
    } else if role.contains("strategy") || role.contains("lead") {
        "planning"
    } else {
        ""
    };
    rooms
        .iter()
        .find(|room| !preferred_kind.is_empty() && room.room_kind == preferred_kind)
        .or_else(|| {
            let len = i64::try_from(rooms.len()).ok()?;
            rooms.get(usize::try_from(seed.rem_euclid(len)).ok()?)
        })
}

fn fallback_agent_room<'a>(
    agent: &AgentRecord,
    rooms: &'a [RoomRecord],
    seed: i64,
) -> Option<&'a RoomRecord> {
    let choices = rooms
        .iter()
        .filter(|room| room.id != agent.room_id)
        .collect::<Vec<_>>();
    let len = i64::try_from(choices.len()).ok()?;
    choices
        .get(usize::try_from(seed.rem_euclid(len)).ok()?)
        .copied()
}

fn agent_activity(agent: &AgentRecord, seed: i64) -> &'static str {
    let role = agent.role.to_lowercase();
    let activities: &[&str] = if role.contains("engineer") || role.contains("developer") {
        &[
            "pairing with Bcode",
            "debugging a tiny gremlin",
            "scouting the repo",
            "writing implementation notes",
        ]
    } else if role.contains("design") || role.contains("creative") {
        &[
            "sketching an initiative pitch",
            "tuning the office vibe",
            "making the UI feel cozy",
            "storyboarding agent rituals",
        ]
    } else if role.contains("review") || role.contains("qa") {
        &[
            "reviewing open proposals",
            "checking validation notes",
            "looking for risky edges",
            "curating the approval queue",
        ]
    } else {
        &[
            "waiting for CEO guidance",
            "organizing company memory",
            "walking with purpose",
            "prioritizing the next initiative",
        ]
    };
    let len = i64::try_from(activities.len()).unwrap_or(1);
    let index = usize::try_from(seed.rem_euclid(len)).unwrap_or_default();
    activities[index]
}

fn stable_i64(value: &str) -> i64 {
    value
        .bytes()
        .fold(0_i64, |acc, byte| {
            acc.saturating_mul(31).saturating_add(i64::from(byte))
        })
        .abs()
}

fn available_interactions(
    working_directory: &Path,
) -> Result<AvailableInteractions, BlimsStateError> {
    let data = load_company_data(working_directory)?;
    Ok(available_interactions_from_data(&data))
}

fn select_world_template(
    request: &WorldTemplateSelectRequest,
    event_context: &EventContext,
) -> Result<WorldSnapshot, BlimsStateError> {
    let template_id = request.template_id.trim().to_string();
    let event_context = event_context.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            select_world_template_in_database(database, &template_id, &event_context).await?;
            let data = load_company_data_from_database(database).await?;
            Ok(world_snapshot_from_data(data))
        })
    })
}

async fn select_world_template_in_database(
    database: &dyn Database,
    template_id: &str,
    event_context: &EventContext,
) -> Result<(), BlimsStateError> {
    let template_id = template_id.trim().to_string();
    let template = find_starter_world_template(&template_id).ok_or_else(|| {
        BlimsStateError::InvalidRequest(format!("unknown template: {template_id}"))
    })?;
    replace_world_room_projections(
        database,
        &template
            .rooms
            .iter()
            .cloned()
            .map(RoomRecord::from)
            .collect::<Vec<_>>(),
    )
    .await?;
    replace_world_object_projections(database, &template.objects).await?;
    append_event(
        database,
        event_context,
        "world.starter_office_selected",
        format!("Starter office selected: {}", template.name),
        &BlimsEventPayload::StarterOfficeSelected {
            template_id: template.id,
            theme: template.name,
            player_room_id: template.player_room_id,
        },
    )
    .await
}

fn available_interactions_from_data(data: &CompanyData) -> AvailableInteractions {
    let room_id = data.player_room_id.clone();
    let mut interactions = vec![WorldInteraction {
        id: "look".to_string(),
        label: "Look around".to_string(),
        command: "look".to_string(),
        source: "room".to_string(),
    }];
    for agent in data.agents.iter().filter(|agent| agent.room_id == room_id) {
        interactions.push(WorldInteraction {
            id: format!("talk-{}", agent.id),
            label: format!("Talk to {}", agent.name),
            command: format!("ai {}", agent.id),
            source: "agent".to_string(),
        });
    }
    if room_id == "whiteboard" {
        interactions.push(WorldInteraction {
            id: "initiatives".to_string(),
            label: "Review initiatives".to_string(),
            command: "initiatives".to_string(),
            source: "whiteboard".to_string(),
        });
    }
    if room_id == "engineering" {
        interactions.push(WorldInteraction {
            id: "tasks".to_string(),
            label: "Review tasks".to_string(),
            command: "tasks".to_string(),
            source: "engineering".to_string(),
        });
    }
    if room_id == "review" {
        interactions.push(WorldInteraction {
            id: "proposals".to_string(),
            label: "Review proposals and artifacts".to_string(),
            command: "proposals".to_string(),
            source: "review".to_string(),
        });
    }
    if let Some(room) = data.rooms.iter().find(|room| room.id == room_id) {
        interactions.push(WorldInteraction {
            id: format!("room-{}", room.id),
            label: format!(
                "Room effect: {} (+{} productivity)",
                room.room_kind, room.productivity_modifier
            ),
            command: "look".to_string(),
            source: "room".to_string(),
        });
    }
    for object in data
        .world_objects
        .iter()
        .filter(|object| object.room_id == room_id)
    {
        interactions.push(WorldInteraction {
            id: format!("object-{}", object.id),
            label: format!(
                "{} {} — {} (+{} productivity)",
                object.symbol, object.name, object.interaction, object.productivity_modifier
            ),
            command: object.interaction.clone(),
            source: object.kind.clone(),
        });
    }
    AvailableInteractions {
        room_id,
        interactions,
    }
}

fn create_initiative(
    request: &InitiativeCreateRequest,
    event_context: &EventContext,
) -> Result<InitiativeSummary, BlimsStateError> {
    let title = request.title.trim().to_string();
    if title.is_empty() {
        return Err(BlimsStateError::InvalidRequest(
            "initiative title cannot be empty".to_string(),
        ));
    }
    let id = stable_slug(&title);
    let description = request.description.clone().unwrap_or_default();
    let priority = request.priority.unwrap_or(100);
    let initiative = InitiativeSummary {
        id,
        title: title.clone(),
        description,
        status: "active".to_string(),
        priority,
    };
    let event_context = event_context.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            append_event(
                database,
                &event_context,
                "initiative.created",
                format!("Initiative created: {title}"),
                &BlimsEventPayload::InitiativeCreated {
                    initiative: initiative.clone(),
                },
            )
            .await?;
            Ok::<_, BlimsStateError>(initiative)
        })
    })
}

fn list_initiatives(working_directory: &Path) -> Result<Vec<InitiativeSummary>, BlimsStateError> {
    with_database(working_directory, |database| {
        Box::pin(async move {
            database
                .select("initiatives")
                .columns(&["id", "title", "description", "status", "priority"])
                .sort("priority", SortDirection::Asc)
                .execute(database)
                .await?
                .iter()
                .map(initiative_summary)
                .collect()
        })
    })
}

fn inspect_initiative(
    request: &InitiativeInspectRequest,
) -> Result<InitiativeSummary, BlimsStateError> {
    let initiative_id = request.initiative_id.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move { load_initiative(database, &initiative_id).await })
    })
}

fn set_initiative_status(
    request: &InitiativeInspectRequest,
    event_context: &EventContext,
    status: &str,
) -> Result<InitiativeSummary, BlimsStateError> {
    let initiative_id = request.initiative_id.clone();
    let status = status.to_string();
    let event_context = event_context.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            let initiative = load_initiative(database, &initiative_id).await?;
            append_event(
                database,
                &event_context,
                "initiative.status_set",
                format!("Initiative {initiative_id} status set to {status}."),
                &BlimsEventPayload::InitiativeStatusSet {
                    initiative_id,
                    status: status.clone(),
                },
            )
            .await?;
            Ok(InitiativeSummary {
                status,
                ..initiative
            })
        })
    })
}

fn set_initiative_guidance(
    request: &InitiativeGuidanceRequest,
    event_context: &EventContext,
) -> Result<GuidanceSummary, BlimsStateError> {
    let initiative_request = InitiativeInspectRequest {
        working_directory: request.working_directory.clone(),
        initiative_id: request.initiative_id.clone(),
        correlation_id: None,
        causation_id: None,
        expected_latest_event_id: None,
    };
    inspect_initiative(&initiative_request)?;
    let guidance = request.guidance.trim().to_string();
    if guidance.is_empty() {
        return Err(BlimsStateError::InvalidRequest(
            "initiative guidance cannot be empty".to_string(),
        ));
    }
    let id = format!("{}-{}", request.initiative_id, stable_slug(&guidance));
    let strength = request.strength.clone();
    let initiative_id = request.initiative_id.clone();
    let summary = GuidanceSummary {
        id,
        guidance: format!("Initiative {initiative_id}: {guidance}"),
        strength,
        active: true,
    };
    let event_context = event_context.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            append_event(
                database,
                &event_context,
                "initiative.guidance_set",
                format!("CEO guidance set for initiative {initiative_id}."),
                &BlimsEventPayload::InitiativeGuidanceSet {
                    initiative_id,
                    guidance: summary.clone(),
                },
            )
            .await?;
            Ok::<_, BlimsStateError>(summary)
        })
    })
}

fn set_guidance(
    request: &GuidanceSetRequest,
    event_context: &EventContext,
) -> Result<GuidanceSummary, BlimsStateError> {
    let guidance = request.guidance.trim().to_string();
    if guidance.is_empty() {
        return Err(BlimsStateError::InvalidRequest(
            "guidance cannot be empty".to_string(),
        ));
    }
    let id = stable_slug(&guidance);
    let strength = request.strength.clone();
    let summary = GuidanceSummary {
        id,
        guidance,
        strength,
        active: true,
    };
    let event_context = event_context.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            append_event(
                database,
                &event_context,
                "guidance.set",
                "CEO guidance set.".to_string(),
                &BlimsEventPayload::GuidanceSet {
                    guidance: summary.clone(),
                },
            )
            .await?;
            Ok::<_, BlimsStateError>(summary)
        })
    })
}

fn list_guidance(working_directory: &Path) -> Result<Vec<GuidanceSummary>, BlimsStateError> {
    with_database(working_directory, |database| {
        Box::pin(async move { load_guidance(database).await })
    })
}

async fn load_guidance(database: &dyn Database) -> Result<Vec<GuidanceSummary>, BlimsStateError> {
    database
        .select("executive_guidance")
        .columns(&["id", "guidance", "strength", "active"])
        .sort("created_at", SortDirection::Desc)
        .execute(database)
        .await?
        .iter()
        .map(guidance_summary)
        .collect()
}

fn list_events(request: &EventListRequest) -> Result<Vec<BlimsEventSummary>, BlimsStateError> {
    let limit = usize::try_from(request.limit.min(500)).unwrap_or(500);
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            database
                .select("events")
                .columns(&[
                    "id",
                    "event_version",
                    "company_id",
                    "kind",
                    "summary",
                    "payload_json",
                    "correlation_id",
                    "causation_id",
                ])
                .sort("id", SortDirection::Desc)
                .limit(limit)
                .execute(database)
                .await?
                .iter()
                .map(event_summary)
                .collect()
        })
    })
}

fn rebuild_projections(
    working_directory: &Path,
) -> Result<ProjectionRebuildReport, BlimsStateError> {
    with_database(working_directory, |database| {
        Box::pin(async move { rebuild_projections_from_database(database).await })
    })
}

async fn rebuild_projections_from_database(
    database: &dyn Database,
) -> Result<ProjectionRebuildReport, BlimsStateError> {
    let events = load_event_stream(database).await?;
    let state = replay_events(&events)?;
    apply_projection_state(database, &state).await?;
    Ok(state.report(events.len()))
}

async fn load_event_stream(
    database: &dyn Database,
) -> Result<Vec<BlimsEventSummary>, BlimsStateError> {
    database
        .select("events")
        .columns(&[
            "id",
            "event_version",
            "company_id",
            "kind",
            "summary",
            "payload_json",
            "correlation_id",
            "causation_id",
        ])
        .sort("id", SortDirection::Asc)
        .execute(database)
        .await?
        .iter()
        .map(event_summary)
        .collect()
}

fn list_tasks(working_directory: &Path) -> Result<Vec<TaskSummary>, BlimsStateError> {
    with_database(working_directory, |database| {
        Box::pin(async move {
            database
                .select("tasks")
                .columns(&[
                    "id",
                    "initiative_id",
                    "title",
                    "description",
                    "status",
                    "assigned_agent_id",
                    "rationale",
                    "blocker",
                    "priority",
                ])
                .sort("priority", SortDirection::Asc)
                .execute(database)
                .await?
                .iter()
                .map(task_summary)
                .collect()
        })
    })
}

fn inspect_task(request: &TaskInspectRequest) -> Result<TaskSummary, BlimsStateError> {
    let task_id = request.task_id.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            database
                .select("tasks")
                .columns(&[
                    "id",
                    "initiative_id",
                    "title",
                    "description",
                    "status",
                    "assigned_agent_id",
                    "rationale",
                    "blocker",
                    "priority",
                ])
                .filter(Box::new(where_eq("id", task_id)))
                .limit(1)
                .execute_first(database)
                .await?
                .as_ref()
                .map(task_summary)
                .transpose()?
                .ok_or(BlimsStateError::MissingColumn("task"))
        })
    })
}

fn record_task_outcome(
    request: &TaskOutcomeRequest,
    event_context: &EventContext,
) -> Result<AgentStatsRecord, BlimsStateError> {
    let request = request.clone();
    let event_context = event_context.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            let task = load_task(database, &request.task_id).await?;
            let mut stats = load_agent_stats(database)
                .await?
                .into_iter()
                .find(|stats| stats.agent_id == task.assigned_agent_id)
                .unwrap_or_else(|| starter_agent_stats(&task.assigned_agent_id));
            apply_task_outcome_to_stats(&mut stats, request.success);
            append_event(
                database,
                &event_context,
                "task.outcome_recorded",
                format!("Task outcome recorded: {}", request.summary),
                &BlimsEventPayload::TaskOutcomeRecorded {
                    task_id: task.id,
                    agent_id: stats.agent_id.clone(),
                    success: request.success,
                    summary: request.summary,
                    stats: stats.clone(),
                },
            )
            .await?;
            Ok::<_, BlimsStateError>(stats)
        })
    })
}

fn apply_task_outcome_to_stats(stats: &mut AgentStatsRecord, success: bool) {
    if success {
        stats.confidence = (stats.confidence + 3).min(100);
        stats.quality_modifier += 1;
        stats.persistence_modifier += 1;
    } else {
        stats.confidence = (stats.confidence - 4).max(0);
        stats.risk_modifier += 2;
        stats.focus = (stats.focus + 1).min(100);
    }
}

fn build_task_work_prompt(request: &TaskInspectRequest) -> Result<TaskWorkPrompt, BlimsStateError> {
    let task_id = request.task_id.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            let data = load_company_data_from_database(database).await?;
            let task = database
                .select("tasks")
                .columns(&[
                    "id",
                    "initiative_id",
                    "title",
                    "description",
                    "status",
                    "assigned_agent_id",
                    "rationale",
                    "blocker",
                    "priority",
                ])
                .filter(Box::new(where_eq("id", task_id)))
                .limit(1)
                .execute_first(database)
                .await?
                .as_ref()
                .map(task_summary)
                .transpose()?
                .ok_or(BlimsStateError::MissingColumn("task"))?;
            Ok(TaskWorkPrompt {
                task_id: task.id.clone(),
                agent_id: task.assigned_agent_id.clone(),
                prompt: task_work_prompt_text(&task, &data),
            })
        })
    })
}

fn list_artifacts(working_directory: &Path) -> Result<Vec<ArtifactSummary>, BlimsStateError> {
    with_database(working_directory, |database| {
        Box::pin(async move {
            database
                .select("artifacts")
                .columns(&["id", "initiative_id", "kind", "title", "status"])
                .sort("created_at", SortDirection::Desc)
                .execute(database)
                .await?
                .iter()
                .map(artifact_summary)
                .collect()
        })
    })
}

fn create_artifact(
    request: &ArtifactCreateRequest,
    event_context: &EventContext,
) -> Result<ArtifactDetail, BlimsStateError> {
    let kind = request.kind.trim().to_string();
    let title = request.title.trim().to_string();
    if kind.is_empty() || title.is_empty() {
        return Err(BlimsStateError::InvalidRequest(
            "artifact kind and title are required".to_string(),
        ));
    }
    let artifact = ArtifactDetail {
        id: format!("{}-{}", stable_slug(&kind), stable_slug(&title)),
        initiative_id: request.initiative_id.clone(),
        kind,
        title,
        status: "draft".to_string(),
        payload_json: request.payload_json.clone(),
    };
    let event_context = event_context.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            append_event(
                database,
                &event_context,
                "artifact.created",
                format!("Artifact created: {}", artifact.title),
                &BlimsEventPayload::ArtifactCreated {
                    artifact: artifact.clone(),
                },
            )
            .await?;
            Ok::<_, BlimsStateError>(artifact)
        })
    })
}

fn inspect_artifact(request: &ArtifactInspectRequest) -> Result<ArtifactDetail, BlimsStateError> {
    let artifact_id = request.artifact_id.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move { load_artifact(database, &artifact_id).await })
    })
}

async fn load_artifact(
    database: &dyn Database,
    artifact_id: &str,
) -> Result<ArtifactDetail, BlimsStateError> {
    database
        .select("artifacts")
        .columns(&[
            "id",
            "initiative_id",
            "kind",
            "title",
            "status",
            "payload_json",
        ])
        .filter(Box::new(where_eq("id", artifact_id.to_string())))
        .limit(1)
        .execute_first(database)
        .await?
        .as_ref()
        .map(artifact_detail)
        .transpose()?
        .ok_or(BlimsStateError::MissingColumn("artifact"))
}

fn set_artifact_status(
    request: &ArtifactInspectRequest,
    event_context: &EventContext,
    status: &str,
) -> Result<ArtifactDetail, BlimsStateError> {
    let artifact_id = request.artifact_id.clone();
    let status = status.to_string();
    let event_context = event_context.clone();
    let working_directory = request.working_directory.clone();
    with_database(&working_directory, move |database| {
        Box::pin(async move {
            let artifact = load_artifact(database, &artifact_id).await?;
            set_artifact_status_in_database(database, &artifact_id, &status, &event_context)
                .await?;
            Ok::<_, BlimsStateError>(ArtifactDetail { status, ..artifact })
        })
    })
}

async fn set_artifact_status_in_database(
    database: &dyn Database,
    artifact_id: &str,
    status: &str,
    event_context: &EventContext,
) -> Result<(), BlimsStateError> {
    let artifact_id = artifact_id.to_string();
    let status = status.to_string();
    load_artifact(database, &artifact_id).await?;
    append_event(
        database,
        event_context,
        "artifact.status_set",
        format!("Artifact {artifact_id} status set to {status}."),
        &BlimsEventPayload::ArtifactStatusSet {
            artifact_id,
            status,
        },
    )
    .await
}

fn register_proposal(
    request: &ProposalRegisterRequest,
    event_context: &EventContext,
) -> Result<WorkProposalSummary, BlimsStateError> {
    let request = request.clone();
    let event_context = event_context.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            let task = load_task(database, &request.task_id).await?;
            let id = format!("proposal-{}", request.task_id);
            let summary = format!("Draft work proposal for {}", task.title);
            let proposal = WorkProposalSummary {
                id,
                task_id: task.id,
                initiative_id: task.initiative_id,
                agent_id: task.assigned_agent_id,
                session_id: request.session_id,
                worktree_path: request.worktree_path.display().to_string(),
                branch: request.branch,
                status: "draft".to_string(),
                summary,
                validation_notes: "not yet reported".to_string(),
            };
            append_event(
                database,
                &event_context,
                "proposal.registered",
                format!("Work proposal registered: {}", proposal.id),
                &BlimsEventPayload::ProposalRegistered {
                    proposal: proposal.clone(),
                },
            )
            .await?;
            append_event(
                database,
                &event_context,
                "worktree.recorded",
                format!("Worktree recorded for task {}", proposal.task_id),
                &BlimsEventPayload::WorktreeRecorded {
                    worktree: WorktreeRecord {
                        id: format!("worktree-{}", proposal.task_id),
                        task_id: proposal.task_id.clone(),
                        agent_id: proposal.agent_id.clone(),
                        session_id: proposal.session_id.clone(),
                        path: proposal.worktree_path.clone(),
                        branch: proposal.branch.clone(),
                        status: "active".to_string(),
                    },
                },
            )
            .await?;
            Ok(proposal)
        })
    })
}

fn list_proposals(working_directory: &Path) -> Result<Vec<WorkProposalSummary>, BlimsStateError> {
    with_database(working_directory, |database| {
        Box::pin(async move {
            database
                .select("work_proposals")
                .columns(&proposal_columns())
                .sort("updated_at", SortDirection::Desc)
                .execute(database)
                .await?
                .iter()
                .map(proposal_summary)
                .collect()
        })
    })
}

fn inspect_proposal(request: &ProposalRequest) -> Result<WorkProposalSummary, BlimsStateError> {
    let proposal_id = request.proposal_id.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move { load_proposal(database, &proposal_id).await })
    })
}

fn set_proposal_status(
    request: &ProposalRequest,
    event_context: &EventContext,
    status: &str,
) -> Result<WorkProposalSummary, BlimsStateError> {
    let proposal_id = request.proposal_id.clone();
    let status = status.to_string();
    let event_context = event_context.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            let proposal = load_proposal(database, &proposal_id).await?;
            set_proposal_status_in_database(database, &proposal_id, &status, &event_context)
                .await?;
            Ok(WorkProposalSummary { status, ..proposal })
        })
    })
}

async fn set_proposal_status_in_database(
    database: &dyn Database,
    proposal_id: &str,
    status: &str,
    event_context: &EventContext,
) -> Result<(), BlimsStateError> {
    let proposal_id = proposal_id.to_string();
    let status = status.to_string();
    load_proposal(database, &proposal_id).await?;
    append_event(
        database,
        event_context,
        "proposal.status_set",
        format!("Proposal {proposal_id} status set to {status}."),
        &BlimsEventPayload::ProposalStatusSet {
            proposal_id,
            status,
        },
    )
    .await
}

fn record_proposal_patch(
    request: &ProposalRecordPatchRequest,
    event_context: &EventContext,
) -> Result<ArtifactDetail, BlimsStateError> {
    let request = request.clone();
    let event_context = event_context.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            let proposal = load_proposal(database, &request.proposal_id).await?;
            let id = format!("patch-{}", proposal.id);
            let payload = serde_json::json!({
                "proposal_id": proposal.id,
                "task_id": proposal.task_id,
                "session_id": proposal.session_id,
                "worktree_path": proposal.worktree_path,
                "branch": proposal.branch,
                "patch": request.patch,
            });
            let payload_json = serde_json::to_string_pretty(&payload)?;
            let artifact = ArtifactDetail {
                id,
                initiative_id: proposal.initiative_id,
                kind: "proposal_patch".to_string(),
                title: format!("Patch for {}", proposal.id),
                status: "draft".to_string(),
                payload_json,
            };
            append_event(
                database,
                &event_context,
                "artifact.created",
                format!("Artifact created: {}", artifact.title),
                &BlimsEventPayload::ArtifactCreated {
                    artifact: artifact.clone(),
                },
            )
            .await?;
            Ok(artifact)
        })
    })
}

fn build_agent_talk_prompt(
    request: &AgentTalkPromptRequest,
) -> Result<AgentTalkPrompt, BlimsStateError> {
    let agent_id = request.agent_id.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            let data = load_company_data_from_database(database).await?;
            let agent = data
                .agents
                .iter()
                .find(|agent| agent.id == agent_id)
                .ok_or_else(|| {
                    BlimsStateError::InvalidRequest(format!("unknown Blims agent: {agent_id}"))
                })?;
            let conversation_id = format!(
                "conversation-{}-{}",
                agent.id,
                latest_event_id(database).await? + 1
            );
            Ok(AgentTalkPrompt {
                agent_id,
                conversation_id,
                prompt: agent_talk_prompt_text(agent, &data),
            })
        })
    })
}

fn autonomous_agent_planning_prompt(agent: &AgentRecord, data: &CompanyData) -> String {
    let initiatives = data
        .initiatives
        .iter()
        .map(|initiative| {
            format!(
                "* [{} priority {}] {} — {}",
                initiative.status, initiative.priority, initiative.title, initiative.description
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let tasks = data
        .tasks
        .iter()
        .filter(|task| task.assigned_agent_id.is_empty() || task.assigned_agent_id == agent.id)
        .map(|task| format!("* [{}] {} — {}", task.status, task.title, task.description))
        .collect::<Vec<_>>()
        .join("\n");
    let guidance = data
        .guidance
        .iter()
        .filter(|item| item.active)
        .map(|item| format!("* [{}] {}", item.strength, item.guidance))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are {}, a {} in Blims, a cozy autonomous AI company inside Bcode.\n\n\
         Company mission: {}\nCulture: {}\n\nActive CEO guidance:\n{}\n\n\
         Current initiatives:\n{}\n\nCandidate tasks for you:\n{}\n\n\
         Think like a proactive teammate. Decide what you should do next, whether any initiative should be reprioritized, paused, split, or clarified, and what concrete artifact/proposal/task should be produced. Return concise JSON with fields: decision, rationale, suggested_commands, risks, questions_for_ceo.",
        agent.name,
        agent.role,
        data.company.mission,
        data.company.culture,
        if guidance.is_empty() {
            "* none"
        } else {
            &guidance
        },
        if initiatives.is_empty() {
            "* none"
        } else {
            &initiatives
        },
        if tasks.is_empty() { "* none" } else { &tasks },
    )
}

fn task_work_prompt_text(task: &TaskSummary, data: &CompanyData) -> String {
    let initiative = data
        .initiatives
        .iter()
        .find(|initiative| initiative.id == task.initiative_id);
    let assigned_agent = data
        .agents
        .iter()
        .find(|agent| agent.id == task.assigned_agent_id);
    let guidance = data
        .guidance
        .iter()
        .map(|item| format!("* [{}] {}", item.strength, item.guidance))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are working as a Blims agent inside Bcode. Do real useful work for this repo, but keep changes sandboxed/proposed unless explicitly approved.\n\n\
         Company mission: {}\nCulture: {}\nCulture priority bias: {}\n\nActive CEO guidance:\n{}\n\n\
         Initiative: {}\nInitiative description: {}\n\n\
         Task `{}`: {}\nDescription: {}\nStatus: {}\nPriority: {}\nAssigned agent: {}\nRationale: {}\n\n\
         Produce concrete implementation, review, research, docs, or artifact work as appropriate. Prefer small reviewable changes. Explain what you changed or propose, validation to run, risks, and next steps.",
        data.company.mission,
        data.company.culture,
        data.company.culture_priority_bias(),
        if guidance.is_empty() {
            "* none"
        } else {
            &guidance
        },
        initiative.map_or("unknown initiative", |initiative| initiative.title.as_str()),
        initiative.map_or("unknown initiative description", |initiative| {
            initiative.description.as_str()
        }),
        task.id,
        task.title,
        task.description,
        task.status,
        task.priority,
        assigned_agent.map_or_else(
            || task.assigned_agent_id.clone(),
            |agent| format!("{} ({})", agent.name, agent.role),
        ),
        task.rationale
    )
}

fn agent_talk_prompt_text(agent: &AgentRecord, data: &CompanyData) -> String {
    let initiatives = data
        .initiatives
        .iter()
        .map(|initiative| format!("* [{}] {}", initiative.status, initiative.title))
        .collect::<Vec<_>>()
        .join("\n");
    let guidance = data
        .guidance
        .iter()
        .map(|item| format!("* [{}] {}", item.strength, item.guidance))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are {}, a {} inside Blims, a cozy AI company simulator running inside Bcode.\n\n\
         Room: {}\nStatus: {}\nCompany mission: {}\nCulture: {}\nCulture priority bias: {}\n\n\
         Active CEO guidance:\n{}\n\nActive initiatives:\n{}\n\n\
         Reply in-character as this Blims agent. Be cozy, useful, dynamic, candid, and specific. \
         Tell the CEO what you are thinking about, what you recommend next, and whether anything needs attention.",
        agent.name,
        agent.role,
        agent.room_id,
        agent.status,
        data.company.mission,
        data.company.culture,
        data.company.culture_priority_bias(),
        if guidance.is_empty() {
            "* none"
        } else {
            &guidance
        },
        if initiatives.is_empty() {
            "* none"
        } else {
            &initiatives
        },
    )
}

fn build_initiative_plan_prompt(
    request: &InitiativePlanPromptRequest,
) -> Result<InitiativePlanningPrompt, BlimsStateError> {
    let initiative_id = request.initiative_id.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            let data = load_company_data_from_database(database).await?;
            let initiative = load_initiative(database, &initiative_id).await?;
            Ok(InitiativePlanningPrompt {
                initiative_id,
                prompt: planning_prompt(&data, &initiative),
            })
        })
    })
}

fn import_initiative_plan(
    request: &InitiativeImportPlanRequest,
    event_context: &EventContext,
) -> Result<AiInitiativePlan, BlimsStateError> {
    let initiative_id = request.initiative_id.clone();
    let plan = request.plan.clone();
    let plan_for_response = plan.clone();
    let event_context = event_context.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            let payload_json = serde_json::to_string(&plan)?;
            let artifact_id = format!("plan-{initiative_id}");
            let artifact = ArtifactDetail {
                id: artifact_id.clone(),
                initiative_id: initiative_id.clone(),
                kind: "ai_plan".to_string(),
                title: "AI-generated initiative plan".to_string(),
                status: "draft".to_string(),
                payload_json: payload_json.clone(),
            };
            database
                .insert("artifacts")
                .value("id", artifact_id)
                .value("initiative_id", initiative_id.clone())
                .value("kind", "ai_plan")
                .value("title", "AI-generated initiative plan")
                .value("status", "draft")
                .value("payload_json", payload_json)
                .execute(database)
                .await?;
            let mut task_summaries = Vec::new();
            for task in &plan.tasks {
                let task_id = format!("{}-{}", initiative_id, stable_slug(&task.title));
                let task_summary = TaskSummary {
                    id: task_id.clone(),
                    initiative_id: initiative_id.clone(),
                    title: task.title.clone(),
                    description: task.description.clone(),
                    status: "proposed".to_string(),
                    assigned_agent_id: task.suggested_agent_id.clone().unwrap_or_default(),
                    rationale: task.rationale.clone(),
                    blocker: String::new(),
                    priority: task.priority,
                };
                task_summaries.push(task_summary);
            }
            append_event(
                database,
                &event_context,
                "artifact.created",
                format!("Artifact created: {}", artifact.title),
                &BlimsEventPayload::ArtifactCreated { artifact },
            )
            .await?;
            for task in &task_summaries {
                append_event(
                    database,
                    &event_context,
                    "task.created",
                    format!("Task created: {}", task.title),
                    &BlimsEventPayload::TaskCreated { task: task.clone() },
                )
                .await?;
            }
            append_event(
                database,
                &event_context,
                "initiative.plan_imported",
                format!("AI plan imported for initiative {initiative_id}."),
                &BlimsEventPayload::InitiativePlanImported {
                    initiative_id,
                    task_count: plan.tasks.len(),
                },
            )
            .await?;
            Ok::<_, BlimsStateError>(plan_for_response)
        })
    })
}

async fn load_task(database: &dyn Database, task_id: &str) -> Result<TaskSummary, BlimsStateError> {
    database
        .select("tasks")
        .columns(&[
            "id",
            "initiative_id",
            "title",
            "description",
            "status",
            "assigned_agent_id",
            "rationale",
            "priority",
        ])
        .filter(Box::new(where_eq("id", task_id)))
        .limit(1)
        .execute_first(database)
        .await?
        .as_ref()
        .map(task_summary)
        .transpose()?
        .ok_or(BlimsStateError::MissingColumn("task"))
}

async fn load_proposal(
    database: &dyn Database,
    proposal_id: &str,
) -> Result<WorkProposalSummary, BlimsStateError> {
    database
        .select("work_proposals")
        .columns(&proposal_columns())
        .filter(Box::new(where_eq("id", proposal_id)))
        .limit(1)
        .execute_first(database)
        .await?
        .as_ref()
        .map(proposal_summary)
        .transpose()?
        .ok_or(BlimsStateError::MissingColumn("proposal"))
}

const fn proposal_columns() -> [&'static str; 10] {
    [
        "id",
        "task_id",
        "initiative_id",
        "agent_id",
        "session_id",
        "worktree_path",
        "branch",
        "status",
        "summary",
        "validation_notes",
    ]
}

async fn load_initiative(
    database: &dyn Database,
    initiative_id: &str,
) -> Result<InitiativeSummary, BlimsStateError> {
    database
        .select("initiatives")
        .columns(&["id", "title", "description", "status", "priority"])
        .filter(Box::new(where_eq("id", initiative_id)))
        .limit(1)
        .execute_first(database)
        .await?
        .as_ref()
        .map(initiative_summary)
        .transpose()?
        .ok_or(BlimsStateError::MissingColumn("initiative"))
}

fn planning_prompt(data: &CompanyData, initiative: &InitiativeSummary) -> String {
    let agents = data
        .agents
        .iter()
        .map(|agent| format!("* {} (`{}`): {}", agent.name, agent.id, agent.role))
        .collect::<Vec<_>>()
        .join("\n");
    let guidance = data
        .guidance
        .iter()
        .map(|item| format!("* [{}] {}", item.strength, item.guidance))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are Mira, the Product Lead inside Blims, a cozy AI company simulator for Bcode.\n\n\
         Company mission: {}\n\
         Company culture: {}\n\n\
         Active CEO guidance:\n{}\n\n\
         Current agents:\n{}\n\n\
         Initiative `{}`: {}\n\
         Description: {}\n\n\
         Create a dynamic, creative, useful initial plan. Return ONLY JSON matching this schema:\n\
         {{\"summary\": string, \"acceptance_criteria\": string[], \"tasks\": [{{\"title\": string, \"description\": string, \"suggested_agent_id\": string|null, \"rationale\": string, \"priority\": number}}], \"risks\": string[], \"questions\": string[], \"cozy_game_opportunities\": string[]}}",
        data.company.mission,
        data.company.culture,
        if guidance.is_empty() {
            "* none"
        } else {
            &guidance
        },
        agents,
        initiative.id,
        initiative.title,
        initiative.description,
    )
}

fn stable_slug(value: &str) -> String {
    value
        .chars()
        .filter_map(|character| {
            if character.is_ascii_alphanumeric() {
                Some(character.to_ascii_lowercase())
            } else if character.is_whitespace() || matches!(character, '-' | '_') {
                Some('-')
            } else {
                None
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

async fn load_company_data_from_database(
    database: &dyn Database,
) -> Result<CompanyData, BlimsStateError> {
    let company = load_company_record(database).await?;
    let (world_theme, player_room_id) = load_world_record(database).await?;
    let room_rows = load_world_room_rows(database).await?;
    let agent_rows = load_agent_rows(database).await?;
    let department_rows = load_department_rows(database).await?;
    let team_rows = load_team_rows(database).await?;
    let initiatives = load_initiatives(database).await?;
    let guidance = database
        .select("executive_guidance")
        .columns(&["id", "guidance", "strength", "active"])
        .filter(Box::new(where_eq("active", true)))
        .sort("created_at", SortDirection::Desc)
        .execute(database)
        .await?
        .iter()
        .map(guidance_summary)
        .collect::<Result<Vec<_>, _>>()?;
    let proposals = load_proposals(database).await?;
    let tasks = load_tasks(database).await?;
    let artifacts = load_artifacts(database).await?;
    let agent_stats = load_agent_stats(database).await?;
    let relationships = load_agent_relationships(database).await?;
    let memories = load_agent_memories(database).await?;
    let world_objects = load_world_objects(database).await?;
    let permissions = load_agent_permissions(database).await?;
    let permission_escalations = load_permission_escalations(database).await?;
    let contract_violations = load_contract_violations(database).await?;

    Ok(CompanyData {
        company,
        world_theme,
        player_room_id,
        rooms: room_rows
            .iter()
            .map(room_record)
            .collect::<Result<Vec<_>, _>>()?,
        agents: agent_rows
            .iter()
            .map(agent_record)
            .collect::<Result<Vec<_>, _>>()?,
        departments: department_rows
            .iter()
            .map(department_record)
            .collect::<Result<Vec<_>, _>>()?,
        teams: team_rows
            .iter()
            .map(team_record)
            .collect::<Result<Vec<_>, _>>()?,
        initiatives,
        projects: load_projects(database).await?,
        guidance,
        proposals,
        tasks,
        artifacts,
        contract_violations,
        agent_stats,
        relationships,
        memories,
        permissions,
        permission_escalations,
        world_objects,
    })
}

async fn load_world_room_rows(database: &dyn Database) -> Result<Vec<Row>, BlimsStateError> {
    Ok(database
        .select("world_rooms")
        .columns(&[
            "id",
            "name",
            "purpose",
            "room_kind",
            "productivity_modifier",
            "x",
            "y",
            "symbol",
            "color",
        ])
        .sort("id", SortDirection::Asc)
        .execute(database)
        .await?)
}

async fn load_agent_rows(database: &dyn Database) -> Result<Vec<Row>, BlimsStateError> {
    Ok(database
        .select("agents")
        .columns(&[
            "id",
            "name",
            "role",
            "department_id",
            "team_id",
            "status",
            "room_id",
        ])
        .sort("id", SortDirection::Asc)
        .execute(database)
        .await?)
}

async fn load_department_rows(database: &dyn Database) -> Result<Vec<Row>, BlimsStateError> {
    Ok(database
        .select("departments")
        .columns(&["id", "name", "purpose"])
        .sort("id", SortDirection::Asc)
        .execute(database)
        .await?)
}

async fn load_team_rows(database: &dyn Database) -> Result<Vec<Row>, BlimsStateError> {
    Ok(database
        .select("teams")
        .columns(&["id", "department_id", "name", "purpose"])
        .sort("id", SortDirection::Asc)
        .execute(database)
        .await?)
}

fn submit_command(
    request: &CommandSubmitRequest,
) -> Result<CommandSubmitResponse, BlimsStateError> {
    if request.command_id.trim().is_empty() {
        return Err(BlimsStateError::InvalidCommandEnvelope(
            "command_id cannot be empty".to_string(),
        ));
    }
    if request.actor.trim().is_empty() {
        return Err(BlimsStateError::InvalidCommandEnvelope(
            "actor cannot be empty".to_string(),
        ));
    }
    let request = request.clone();
    let event_context = request.event_context();
    let working_directory = request.working_directory.clone();
    with_database(&working_directory, move |database| {
        Box::pin(async move {
            let operation_id = format!("op-{}", request.command_id);
            let priority = request.command.priority();
            append_event(
                database,
                &event_context,
                "command.submitted",
                format!("Command submitted: {}", request.command.operation_kind()),
                &BlimsEventPayload::CommandSubmitted {
                    command_id: request.command_id.clone(),
                    actor: request.actor.clone(),
                    frontend_id: request.frontend_id.clone(),
                    kind: request.command.operation_kind().to_string(),
                    priority: priority.to_string(),
                },
            )
            .await?;
            run_command_operation(database, &request, &operation_id).await?;
            Ok(CommandSubmitResponse {
                command_id: request.command_id,
                operation_id,
                status: BlimsOperationStatus::Completed,
                priority,
            })
        })
    })
}

#[allow(clippy::too_many_lines)]
async fn run_command_operation(
    database: &dyn Database,
    request: &CommandSubmitRequest,
    operation_id: &str,
) -> Result<(), BlimsStateError> {
    let event_context = request.event_context();
    append_event(
        database,
        &event_context,
        "operation.status_set",
        format!("Operation {operation_id} is running."),
        &BlimsEventPayload::OperationStatusSet {
            operation_id: operation_id.to_string(),
            status: BlimsOperationStatus::Running.to_string(),
            error: None,
        },
    )
    .await?;
    match &request.command {
        BlimsCommand::RefreshDashboard => {
            run_refresh_dashboard_command(database, operation_id, &event_context).await
        }
        BlimsCommand::MovePlayer { room_id } => {
            run_move_player_command(database, room_id, operation_id, &event_context).await
        }
        BlimsCommand::TickWorld { tick_id, now_ms } => {
            run_tick_world_command(
                database,
                tick_id.as_deref(),
                *now_ms,
                operation_id,
                &event_context,
            )
            .await
        }
        BlimsCommand::SelectWorldTemplate { template_id } => {
            select_world_template_in_database(database, template_id, &event_context).await?;
            complete_operation(database, &event_context, operation_id).await
        }
        BlimsCommand::SetProposalStatus {
            proposal_id,
            status,
        } => {
            set_proposal_status_in_database(database, proposal_id, status, &event_context).await?;
            complete_operation(database, &event_context, operation_id).await
        }
        BlimsCommand::SetArtifactStatus {
            artifact_id,
            status,
        } => {
            set_artifact_status_in_database(database, artifact_id, status, &event_context).await?;
            complete_operation(database, &event_context, operation_id).await
        }
        BlimsCommand::OpenAgentConversation {
            agent_id,
            conversation_id,
            session_id,
            summary,
        } => {
            record_conversation_in_database(
                database,
                conversation_id,
                agent_id,
                session_id,
                summary,
                &event_context,
            )
            .await?;
            complete_operation(database, &event_context, operation_id).await
        }
        BlimsCommand::RecordConversationMessage {
            conversation_id,
            agent_id,
            speaker,
            message,
        } => {
            record_conversation_message_in_database(
                database,
                conversation_id,
                agent_id,
                speaker,
                message,
                &event_context,
            )
            .await?;
            complete_operation(database, &event_context, operation_id).await
        }
        BlimsCommand::ScheduleCompanyTick { tick_id, now_ms } => {
            tick_world_in_database(database, tick_id.as_deref(), *now_ms, &event_context).await?;
            complete_operation(database, &event_context, operation_id).await
        }
        BlimsCommand::ScheduleAgentPlanning { agent_id } => {
            record_agent_planning_cycle(database, agent_id, &event_context).await?;
            complete_operation(database, &event_context, operation_id).await
        }
        BlimsCommand::ScheduleTaskWork { task_id } => {
            record_scheduled_task_work(database, task_id, &event_context).await?;
            complete_operation(database, &event_context, operation_id).await
        }
        BlimsCommand::OpenDashboardView | BlimsCommand::CloseDashboardView => {
            complete_operation(database, &event_context, operation_id).await
        }
    }
}

async fn run_refresh_dashboard_command(
    database: &dyn Database,
    operation_id: &str,
    event_context: &EventContext,
) -> Result<(), BlimsStateError> {
    refresh_dashboard_projection(database, operation_id).await?;
    append_event(
        database,
        event_context,
        "dashboard.projection_refreshed",
        "CEO dashboard projection refreshed.".to_string(),
        &BlimsEventPayload::DashboardProjectionRefreshed {
            operation_id: operation_id.to_string(),
        },
    )
    .await
}

async fn run_move_player_command(
    database: &dyn Database,
    room_id: &str,
    operation_id: &str,
    event_context: &EventContext,
) -> Result<(), BlimsStateError> {
    move_player_in_database(database, room_id, event_context).await?;
    complete_operation(database, event_context, operation_id).await
}

async fn run_tick_world_command(
    database: &dyn Database,
    tick_id: Option<&str>,
    now_ms: Option<i64>,
    operation_id: &str,
    event_context: &EventContext,
) -> Result<(), BlimsStateError> {
    tick_world_in_database(database, tick_id, now_ms, event_context).await?;
    complete_operation(database, event_context, operation_id).await
}

async fn refresh_dashboard_projection(
    database: &dyn Database,
    operation_id: &str,
) -> Result<(), BlimsStateError> {
    let _projection =
        dashboard_projection_from_database(database, Some(operation_id.to_string())).await?;
    Ok(())
}

async fn complete_operation(
    database: &dyn Database,
    event_context: &EventContext,
    operation_id: &str,
) -> Result<(), BlimsStateError> {
    append_event(
        database,
        event_context,
        "operation.status_set",
        format!("Operation {operation_id} completed."),
        &BlimsEventPayload::OperationStatusSet {
            operation_id: operation_id.to_string(),
            status: BlimsOperationStatus::Completed.to_string(),
            error: None,
        },
    )
    .await
}

fn dashboard_projection(working_directory: &Path) -> Result<DashboardProjection, BlimsStateError> {
    with_database(working_directory, |database| {
        Box::pin(async move { dashboard_projection_from_database(database, None).await })
    })
}

async fn dashboard_projection_from_database(
    database: &dyn Database,
    refreshed_by_operation_id: Option<String>,
) -> Result<DashboardProjection, BlimsStateError> {
    Ok(DashboardProjection {
        initiatives: load_initiatives(database).await?,
        tasks: load_tasks(database).await?,
        proposals: load_proposals(database).await?,
        artifacts: load_artifacts(database).await?,
        guidance: load_guidance(database).await?,
        latest_event_id: latest_event_id(database).await?,
        refreshed_by_operation_id,
    })
}

#[allow(clippy::too_many_lines)]
fn scheduler_tick(request: &SchedulerTickRequest) -> Result<SchedulerTickReport, BlimsStateError> {
    let mut report = SchedulerTickReport {
        claimed: Vec::new(),
        completed: Vec::new(),
        failed: Vec::new(),
    };
    let limit = request.limit.min(25);
    for _ in 0..limit {
        let claim = OperationClaimNextRequest {
            working_directory: request.working_directory.clone(),
            worker_id: request.worker_id.clone(),
            lease_ms: request.lease_ms,
        };
        let Some(operation) = claim_next_operation(&claim)? else {
            break;
        };
        report.claimed.push(operation.clone());
        let run_request = OperationRunClaimedRequest {
            working_directory: request.working_directory.clone(),
            operation_id: operation.id,
        };
        match run_claimed_operation(&run_request) {
            Ok(completed) => report.completed.push(completed),
            Err(error) => {
                let fail_request = OperationFailRequest {
                    working_directory: request.working_directory.clone(),
                    operation_id: run_request.operation_id,
                    error: error.to_string(),
                };
                report.failed.push(fail_claimed_operation(&fail_request)?);
            }
        }
    }
    Ok(report)
}

fn run_claimed_operation(
    request: &OperationRunClaimedRequest,
) -> Result<BlimsOperationSummary, BlimsStateError> {
    let request = request.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            let operation = load_operation(database, &request.operation_id).await?;
            if operation.status != BlimsOperationStatus::Running.as_str() {
                return Err(BlimsStateError::InvalidRequest(format!(
                    "operation {} is not running",
                    operation.id
                )));
            }
            let event_context = EventContext {
                correlation: operation.command_id.clone(),
                causation: operation.id.clone(),
                expected_latest: None,
            };
            run_operation_kind(database, &operation, &event_context).await?;
            complete_operation(database, &event_context, &operation.id).await?;
            delete_operation_lease(database, &operation.id).await?;
            finish_latest_operation_attempt(
                database,
                &operation.id,
                BlimsOperationStatus::Completed.as_str(),
                "",
                current_time_ms(),
            )
            .await?;
            load_operation(database, &operation.id).await
        })
    })
}

async fn run_operation_kind(
    database: &dyn Database,
    operation: &BlimsOperationSummary,
    event_context: &EventContext,
) -> Result<(), BlimsStateError> {
    match operation.kind.as_str() {
        "company.tick.scheduled" => {
            tick_world_in_database(
                database,
                Some(&operation.command_id),
                Some(current_time_ms()),
                event_context,
            )
            .await
        }
        "agent.planning.scheduled" => {
            let agent_id = operation
                .command_id
                .strip_prefix("agent-planning-")
                .unwrap_or(&operation.actor);
            record_agent_planning_cycle(database, agent_id, event_context).await
        }
        "task.work.scheduled" => {
            let task_id = operation
                .command_id
                .strip_prefix("task-work-")
                .unwrap_or("");
            record_scheduled_task_work(database, task_id, event_context).await
        }
        _ => Ok(()),
    }
}

async fn record_agent_planning_cycle(
    database: &dyn Database,
    agent_id: &str,
    event_context: &EventContext,
) -> Result<(), BlimsStateError> {
    let data = load_company_data_from_database(database).await?;
    let agent = data
        .agents
        .iter()
        .find(|agent| agent.id == agent_id)
        .ok_or_else(|| BlimsStateError::InvalidRequest(format!("unknown agent: {agent_id}")))?;
    let rationale = format!(
        "{} reviewed company state and is preparing an autonomous planning pass aligned with their role: {}.",
        agent.name, agent.role
    );
    append_event(
        database,
        event_context,
        "agent.planning_cycle_recorded",
        format!("Agent planning cycle recorded: {}", agent.id),
        &BlimsEventPayload::AgentPlanningCycleRecorded {
            agent_id: agent.id.clone(),
            rationale,
        },
    )
    .await?;
    let work = PreparedAiWorkItem {
        id: format!(
            "ai-plan-{}-{}",
            agent.id,
            stable_slug(&event_context.correlation)
        ),
        operation_id: event_context.causation.clone(),
        kind: "agent_planning".to_string(),
        agent_id: agent.id.clone(),
        task_id: None,
        prompt: autonomous_agent_planning_prompt(agent, &data),
    };
    append_event(
        database,
        event_context,
        "ai.work_prepared",
        format!("AI planning work prepared for {}.", agent.name),
        &BlimsEventPayload::AiWorkPrepared { work },
    )
    .await
}

async fn record_scheduled_task_work(
    database: &dyn Database,
    task_id: &str,
    event_context: &EventContext,
) -> Result<(), BlimsStateError> {
    let data = load_company_data_from_database(database).await?;
    let task = data
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .cloned()
        .ok_or_else(|| BlimsStateError::InvalidRequest(format!("unknown task: {task_id}")))?;
    let rationale = format!(
        "Scheduled autonomous work for task '{}' with assigned agent {}.",
        task.title, task.assigned_agent_id
    );
    append_event(
        database,
        event_context,
        "task.work_scheduled",
        format!("Task work scheduled: {}", task.id),
        &BlimsEventPayload::TaskWorkScheduled {
            task_id: task.id.clone(),
            agent_id: task.assigned_agent_id.clone(),
            rationale,
        },
    )
    .await?;
    let work = PreparedAiWorkItem {
        id: format!(
            "ai-task-{}-{}",
            task.id,
            stable_slug(&event_context.correlation)
        ),
        operation_id: event_context.causation.clone(),
        kind: "task_work".to_string(),
        agent_id: task.assigned_agent_id.clone(),
        task_id: Some(task.id.clone()),
        prompt: task_work_prompt_text(&task, &data),
    };
    append_event(
        database,
        event_context,
        "ai.work_prepared",
        format!("AI task work prepared for {}.", task.id),
        &BlimsEventPayload::AiWorkPrepared { work },
    )
    .await
}

fn claim_next_operation(
    request: &OperationClaimNextRequest,
) -> Result<Option<BlimsOperationSummary>, BlimsStateError> {
    let request = request.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            let now_ms = current_time_ms();
            let Some(operation) = load_claimable_operation(database, now_ms).await? else {
                return Ok(None);
            };
            let lease_expires_at_ms = now_ms.saturating_add(request.lease_ms.max(1));
            upsert_operation_lease(
                database,
                &operation.id,
                &request.worker_id,
                lease_expires_at_ms,
                now_ms,
            )
            .await?;
            insert_operation_attempt(
                database,
                &operation.id,
                &request.worker_id,
                BlimsOperationStatus::Running.as_str(),
                "",
                now_ms,
                0,
            )
            .await?;
            database
                .update("operations")
                .value("status", BlimsOperationStatus::Running.as_str())
                .value("error", "")
                .filter(Box::new(where_eq("id", operation.id.clone())))
                .execute(database)
                .await?;
            load_operation(database, &operation.id).await.map(Some)
        })
    })
}

fn complete_claimed_operation(
    request: &OperationCompleteRequest,
) -> Result<BlimsOperationSummary, BlimsStateError> {
    let request = request.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            database
                .update("operations")
                .value("status", BlimsOperationStatus::Completed.as_str())
                .value("error", "")
                .filter(Box::new(where_eq("id", request.operation_id.clone())))
                .execute(database)
                .await?;
            delete_operation_lease(database, &request.operation_id).await?;
            finish_latest_operation_attempt(
                database,
                &request.operation_id,
                BlimsOperationStatus::Completed.as_str(),
                "",
                current_time_ms(),
            )
            .await?;
            load_operation(database, &request.operation_id).await
        })
    })
}

fn fail_claimed_operation(
    request: &OperationFailRequest,
) -> Result<BlimsOperationSummary, BlimsStateError> {
    let request = request.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            database
                .update("operations")
                .value("status", BlimsOperationStatus::Failed.as_str())
                .value("error", request.error.clone())
                .filter(Box::new(where_eq("id", request.operation_id.clone())))
                .execute(database)
                .await?;
            delete_operation_lease(database, &request.operation_id).await?;
            finish_latest_operation_attempt(
                database,
                &request.operation_id,
                BlimsOperationStatus::Failed.as_str(),
                &request.error,
                current_time_ms(),
            )
            .await?;
            load_operation(database, &request.operation_id).await
        })
    })
}

async fn load_claimable_operation(
    database: &dyn Database,
    now_ms: i64,
) -> Result<Option<BlimsOperationSummary>, BlimsStateError> {
    let operations = database
        .select("operations")
        .columns(&operation_columns())
        .execute(database)
        .await?
        .iter()
        .map(operation_summary)
        .collect::<Result<Vec<_>, _>>()?;
    let leases = database
        .select("operation_leases")
        .columns(&["operation_id", "lease_expires_at_ms"])
        .execute(database)
        .await?;
    Ok(operations
        .into_iter()
        .filter(|operation| operation_is_claimable(operation, &leases, now_ms))
        .min_by_key(operation_claim_sort_key))
}

fn operation_is_claimable(operation: &BlimsOperationSummary, leases: &[Row], now_ms: i64) -> bool {
    if operation.status != BlimsOperationStatus::Queued.as_str()
        && operation.status != BlimsOperationStatus::Running.as_str()
    {
        return false;
    }
    let lease_expires_at_ms = leases
        .iter()
        .find(|lease| {
            lease
                .get("operation_id")
                .is_some_and(|value| value.as_str().is_some_and(|id| id == operation.id))
        })
        .and_then(|lease| lease.get("lease_expires_at_ms"))
        .and_then(|value| value.as_i64());
    operation.status == BlimsOperationStatus::Queued.as_str()
        || lease_expires_at_ms.is_some_and(|expires_at| expires_at <= now_ms)
}

fn operation_claim_sort_key(operation: &BlimsOperationSummary) -> (i64, String) {
    (
        operation_priority_rank(&operation.priority),
        operation.id.clone(),
    )
}

#[allow(clippy::missing_const_for_fn)]
fn operation_priority_rank(priority: &str) -> i64 {
    match priority {
        "interactive" => 0,
        "foreground" => 1,
        "background" => 2,
        "maintenance" => 3,
        _ => 9,
    }
}

async fn load_operation(
    database: &dyn Database,
    operation_id: &str,
) -> Result<BlimsOperationSummary, BlimsStateError> {
    database
        .select("operations")
        .columns(&operation_columns())
        .filter(Box::new(where_eq("id", operation_id.to_string())))
        .limit(1)
        .execute_first(database)
        .await?
        .as_ref()
        .map(operation_summary)
        .transpose()?
        .ok_or(BlimsStateError::MissingColumn("operation"))
}

const fn operation_columns() -> [&'static str; 9] {
    [
        "id",
        "command_id",
        "actor",
        "frontend_id",
        "kind",
        "priority",
        "status",
        "result_event_id",
        "error",
    ]
}

async fn upsert_operation_lease(
    database: &dyn Database,
    operation_id: &str,
    worker_id: &str,
    lease_expires_at_ms: i64,
    heartbeat_at_ms: i64,
) -> Result<(), BlimsStateError> {
    let existing = database
        .select("operation_leases")
        .columns(&["operation_id"])
        .filter(Box::new(where_eq("operation_id", operation_id.to_string())))
        .limit(1)
        .execute_first(database)
        .await?;
    if existing.is_some() {
        database
            .update("operation_leases")
            .value("worker_id", worker_id.to_string())
            .value("lease_expires_at_ms", lease_expires_at_ms)
            .value("heartbeat_at_ms", heartbeat_at_ms)
            .filter(Box::new(where_eq("operation_id", operation_id.to_string())))
            .execute(database)
            .await?;
    } else {
        database
            .insert("operation_leases")
            .value("operation_id", operation_id.to_string())
            .value("worker_id", worker_id.to_string())
            .value("lease_expires_at_ms", lease_expires_at_ms)
            .value("heartbeat_at_ms", heartbeat_at_ms)
            .execute(database)
            .await?;
    }
    Ok(())
}

async fn delete_operation_lease(
    database: &dyn Database,
    operation_id: &str,
) -> Result<(), BlimsStateError> {
    database
        .delete("operation_leases")
        .filter(Box::new(where_eq("operation_id", operation_id.to_string())))
        .execute(database)
        .await?;
    Ok(())
}

async fn insert_operation_attempt(
    database: &dyn Database,
    operation_id: &str,
    worker_id: &str,
    status: &str,
    error: &str,
    started_at_ms: i64,
    finished_at_ms: i64,
) -> Result<(), BlimsStateError> {
    database
        .insert("operation_attempts")
        .value("operation_id", operation_id.to_string())
        .value("worker_id", worker_id.to_string())
        .value("status", status.to_string())
        .value("error", error.to_string())
        .value("started_at_ms", started_at_ms)
        .value("finished_at_ms", finished_at_ms)
        .execute(database)
        .await?;
    Ok(())
}

async fn finish_latest_operation_attempt(
    database: &dyn Database,
    operation_id: &str,
    status: &str,
    error: &str,
    finished_at_ms: i64,
) -> Result<(), BlimsStateError> {
    let latest = database
        .select("operation_attempts")
        .columns(&["id"])
        .filter(Box::new(where_eq("operation_id", operation_id.to_string())))
        .sort("id", SortDirection::Desc)
        .limit(1)
        .execute_first(database)
        .await?;
    if let Some(row) = latest
        && let Some(id) = row.get("id").and_then(|value| value.as_i64())
    {
        database
            .update("operation_attempts")
            .value("status", status.to_string())
            .value("error", error.to_string())
            .value("finished_at_ms", finished_at_ms)
            .filter(Box::new(where_eq("id", id)))
            .execute(database)
            .await?;
    }
    Ok(())
}

fn current_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0_i64, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

fn activity_item(row: &Row) -> Result<ActivityItem, BlimsStateError> {
    let event = event_summary(row)?;
    let payload = serde_json::from_str::<BlimsEventPayload>(&event.payload_json).ok();
    let (actor_id, room_id, severity, action_hint) = activity_metadata(&event, payload.as_ref());
    Ok(ActivityItem {
        id: format!("activity-{}", event.id),
        event_id: event.id,
        kind: event.kind,
        title: event.summary,
        body: activity_body(payload.as_ref()),
        actor_id,
        room_id,
        severity,
        action_hint,
    })
}

fn activity_metadata(
    event: &BlimsEventSummary,
    payload: Option<&BlimsEventPayload>,
) -> (String, String, String, String) {
    match payload {
        Some(BlimsEventPayload::AgentMoved { agent_id, room_id }) => (
            agent_id.clone(),
            room_id.clone(),
            "info".to_string(),
            "Walk over and talk".to_string(),
        ),
        Some(
            BlimsEventPayload::AgentStatusSet { agent_id, .. }
            | BlimsEventPayload::AgentPlanningCycleRecorded { agent_id, .. },
        ) => (
            agent_id.clone(),
            String::new(),
            "info".to_string(),
            "Ask status".to_string(),
        ),
        Some(BlimsEventPayload::AiWorkPrepared { work }) => (
            work.agent_id.clone(),
            String::new(),
            "attention".to_string(),
            format!("Start AI work {}", work.id),
        ),
        Some(BlimsEventPayload::ProposalRegistered { proposal }) => (
            proposal.agent_id.clone(),
            String::new(),
            "attention".to_string(),
            "Review proposal".to_string(),
        ),
        Some(BlimsEventPayload::ArtifactCreated { artifact }) => (
            String::new(),
            String::new(),
            "attention".to_string(),
            format!("Review artifact {}", artifact.id),
        ),
        _ => (
            String::new(),
            String::new(),
            if event.kind.contains("failed") {
                "warning"
            } else {
                "info"
            }
            .to_string(),
            String::new(),
        ),
    }
}

fn activity_body(payload: Option<&BlimsEventPayload>) -> String {
    match payload {
        Some(BlimsEventPayload::AiWorkPrepared { work }) => {
            format!("{} has {} work ready.", work.agent_id, work.kind)
        }
        Some(BlimsEventPayload::TaskWorkScheduled {
            task_id, agent_id, ..
        }) => {
            format!("{agent_id} is queued to work on {task_id}.")
        }
        Some(BlimsEventPayload::AgentPlanningCycleRecorded { rationale, .. }) => rationale.clone(),
        Some(BlimsEventPayload::ConversationMessageRecorded {
            speaker, message, ..
        }) => {
            format!("{speaker}: {message}")
        }
        _ => String::new(),
    }
}

fn proposal_inbox_item(proposal: WorkProposalSummary) -> Option<CeoInboxItem> {
    matches!(proposal.status.as_str(), "draft" | "ready" | "pending").then(|| CeoInboxItem {
        id: proposal.id.clone(),
        kind: "proposal".to_string(),
        title: format!("Review proposal {}", proposal.id),
        summary: proposal.summary,
        priority: 20,
        actor_id: proposal.agent_id,
        action_label: "Open dashboard".to_string(),
        action_command: format!("proposal.review:{}", proposal.id),
    })
}

fn artifact_inbox_item(artifact: ArtifactSummary) -> Option<CeoInboxItem> {
    matches!(artifact.status.as_str(), "draft" | "ready" | "pending").then(|| CeoInboxItem {
        id: artifact.id.clone(),
        kind: "artifact".to_string(),
        title: format!("Review artifact {}", artifact.id),
        summary: artifact.title,
        priority: 30,
        actor_id: String::new(),
        action_label: "Open dashboard".to_string(),
        action_command: format!("artifact.review:{}", artifact.id),
    })
}

fn ai_work_inbox_item(work: PreparedAiWorkItem) -> CeoInboxItem {
    CeoInboxItem {
        id: work.id.clone(),
        kind: "ai_work".to_string(),
        title: format!("Start {} work", work.kind),
        summary: work.task_id.as_ref().map_or_else(
            || format!("{} has planning work ready", work.agent_id),
            |task_id| format!("{} is ready to work on {task_id}", work.agent_id),
        ),
        priority: 10,
        actor_id: work.agent_id,
        action_label: "Start AI work".to_string(),
        action_command: format!("ai_work.start:{}", work.id),
    }
}

async fn load_failed_operation_inbox_items(
    database: &dyn Database,
) -> Result<Vec<CeoInboxItem>, BlimsStateError> {
    Ok(database
        .select("operations")
        .columns(&operation_columns())
        .filter(Box::new(where_eq(
            "status",
            BlimsOperationStatus::Failed.as_str(),
        )))
        .sort("updated_at", SortDirection::Desc)
        .limit(10)
        .execute(database)
        .await?
        .iter()
        .map(operation_summary)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|operation| CeoInboxItem {
            id: operation.id.clone(),
            kind: "operation_failed".to_string(),
            title: format!("Operation failed: {}", operation.kind),
            summary: operation.error,
            priority: 5,
            actor_id: operation.actor,
            action_label: "Inspect operations".to_string(),
            action_command: format!("operation.inspect:{}", operation.id),
        })
        .collect())
}

fn list_activity_items(
    request: &ActivityListRequest,
) -> Result<Vec<ActivityItem>, BlimsStateError> {
    let limit = usize::try_from(request.limit).unwrap_or(usize::MAX);
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            database
                .select("events")
                .columns(&[
                    "id",
                    "event_version",
                    "company_id",
                    "kind",
                    "summary",
                    "payload_json",
                    "correlation_id",
                    "causation_id",
                ])
                .sort("id", SortDirection::Desc)
                .limit(limit)
                .execute(database)
                .await?
                .iter()
                .map(activity_item)
                .collect()
        })
    })
}

fn list_ceo_inbox_items(request: &CeoInboxRequest) -> Result<Vec<CeoInboxItem>, BlimsStateError> {
    let limit = usize::try_from(request.limit).unwrap_or(usize::MAX);
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            let mut items = Vec::new();
            items.extend(
                load_proposals(database)
                    .await?
                    .into_iter()
                    .filter_map(proposal_inbox_item),
            );
            items.extend(
                load_artifacts(database)
                    .await?
                    .into_iter()
                    .filter_map(artifact_inbox_item),
            );
            items.extend(
                list_prepared_ai_work_from_database(database, limit)
                    .await?
                    .into_iter()
                    .map(ai_work_inbox_item),
            );
            items.extend(load_failed_operation_inbox_items(database).await?);
            items.sort_by_key(|item| (item.priority, item.id.clone()));
            items.truncate(limit);
            Ok(items)
        })
    })
}

fn list_prepared_ai_work(
    request: &AiWorkListRequest,
) -> Result<Vec<PreparedAiWorkItem>, BlimsStateError> {
    let limit = usize::try_from(request.limit).unwrap_or(usize::MAX);
    with_database(&request.working_directory, move |database| {
        Box::pin(async move { list_prepared_ai_work_from_database(database, limit).await })
    })
}

async fn list_prepared_ai_work_from_database(
    database: &dyn Database,
    limit: usize,
) -> Result<Vec<PreparedAiWorkItem>, BlimsStateError> {
    let events = database
        .select("events")
        .columns(&["payload_json"])
        .filter(Box::new(where_eq("kind", "ai.work_prepared")))
        .sort("id", SortDirection::Desc)
        .limit(limit)
        .execute(database)
        .await?;
    events
        .iter()
        .filter_map(|event| {
            let payload = event
                .get("payload_json")
                .and_then(|value| value.as_str().map(ToOwned::to_owned))?;
            match serde_json::from_str::<BlimsEventPayload>(&payload).ok()? {
                BlimsEventPayload::AiWorkPrepared { work } => Some(Ok(work)),
                _ => None,
            }
        })
        .collect()
}

fn list_operations(
    request: &OperationListRequest,
) -> Result<Vec<BlimsOperationSummary>, BlimsStateError> {
    let limit = usize::try_from(request.limit).unwrap_or(usize::MAX);
    let working_directory = request.working_directory.clone();
    with_database(&working_directory, move |database| {
        Box::pin(async move {
            database
                .select("operations")
                .columns(&operation_columns())
                .sort("updated_at", SortDirection::Desc)
                .limit(limit)
                .execute(database)
                .await?
                .iter()
                .map(operation_summary)
                .collect()
        })
    })
}

struct OperationProjectionRecord<'a> {
    id: &'a str,
    command_id: &'a str,
    actor: &'a str,
    frontend_id: &'a str,
    kind: &'a str,
    priority: &'a str,
    status: &'a str,
    result_event_id: Option<i64>,
    error: &'a str,
}

async fn upsert_operation(
    database: &dyn Database,
    record: &OperationProjectionRecord<'_>,
) -> Result<(), BlimsStateError> {
    let existing = database
        .select("operations")
        .columns(&["id"])
        .filter(Box::new(where_eq("id", record.id)))
        .limit(1)
        .execute_first(database)
        .await?;
    if existing.is_some() {
        database
            .update("operations")
            .value("status", record.status.to_string())
            .value(
                "result_event_id",
                record.result_event_id.unwrap_or_default(),
            )
            .value("error", record.error.to_string())
            .filter(Box::new(where_eq("id", record.id.to_string())))
            .execute(database)
            .await?;
    } else {
        database
            .insert("operations")
            .value("id", record.id.to_string())
            .value("command_id", record.command_id.to_string())
            .value("actor", record.actor.to_string())
            .value("frontend_id", record.frontend_id.to_string())
            .value("kind", record.kind.to_string())
            .value("priority", record.priority.to_string())
            .value("status", record.status.to_string())
            .value(
                "result_event_id",
                record.result_event_id.unwrap_or_default(),
            )
            .value("error", record.error.to_string())
            .execute(database)
            .await?;
    }
    Ok(())
}

async fn load_initiatives(
    database: &dyn Database,
) -> Result<Vec<InitiativeSummary>, BlimsStateError> {
    database
        .select("initiatives")
        .columns(&["id", "title", "description", "status", "priority"])
        .sort("priority", SortDirection::Asc)
        .execute(database)
        .await?
        .iter()
        .map(initiative_summary)
        .collect::<Result<Vec<_>, _>>()
}

async fn load_projects(database: &dyn Database) -> Result<Vec<ProjectSummary>, BlimsStateError> {
    database
        .select("projects")
        .columns(&["id", "initiative_id", "title", "status", "rationale"])
        .sort("id", SortDirection::Asc)
        .execute(database)
        .await?
        .iter()
        .map(project_summary)
        .collect::<Result<Vec<_>, _>>()
}

async fn load_proposals(
    database: &dyn Database,
) -> Result<Vec<WorkProposalSummary>, BlimsStateError> {
    database
        .select("work_proposals")
        .columns(&proposal_columns())
        .sort("updated_at", SortDirection::Desc)
        .execute(database)
        .await?
        .iter()
        .map(proposal_summary)
        .collect::<Result<Vec<_>, _>>()
}

async fn load_tasks(database: &dyn Database) -> Result<Vec<TaskSummary>, BlimsStateError> {
    database
        .select("tasks")
        .columns(&[
            "id",
            "initiative_id",
            "title",
            "description",
            "status",
            "assigned_agent_id",
            "rationale",
            "blocker",
            "priority",
        ])
        .sort("priority", SortDirection::Asc)
        .execute(database)
        .await?
        .iter()
        .map(task_summary)
        .collect::<Result<Vec<_>, _>>()
}

async fn load_artifacts(database: &dyn Database) -> Result<Vec<ArtifactSummary>, BlimsStateError> {
    database
        .select("artifacts")
        .columns(&["id", "initiative_id", "kind", "title", "status"])
        .sort("created_at", SortDirection::Desc)
        .execute(database)
        .await?
        .iter()
        .map(artifact_summary)
        .collect::<Result<Vec<_>, _>>()
}

async fn load_agent_stats(
    database: &dyn Database,
) -> Result<Vec<AgentStatsRecord>, BlimsStateError> {
    database
        .select("agent_stats")
        .columns(&[
            "agent_id",
            "traits",
            "skills",
            "energy",
            "morale",
            "focus",
            "confidence",
            "speed_modifier",
            "quality_modifier",
            "risk_modifier",
            "creativity_modifier",
            "persistence_modifier",
            "collaboration_modifier",
        ])
        .sort("agent_id", SortDirection::Asc)
        .execute(database)
        .await?
        .iter()
        .map(agent_stats_record)
        .collect::<Result<Vec<_>, _>>()
}

async fn load_agent_relationships(
    database: &dyn Database,
) -> Result<Vec<AgentRelationshipRecord>, BlimsStateError> {
    database
        .select("agent_relationships")
        .columns(&["agent_id", "other_agent_id", "affinity", "trust", "notes"])
        .sort("agent_id", SortDirection::Asc)
        .execute(database)
        .await?
        .iter()
        .map(agent_relationship_record)
        .collect::<Result<Vec<_>, _>>()
}

async fn load_agent_memories(
    database: &dyn Database,
) -> Result<Vec<AgentMemoryRecord>, BlimsStateError> {
    database
        .select("memories")
        .columns(&["id", "agent_id", "kind", "summary", "importance"])
        .sort("importance", SortDirection::Desc)
        .execute(database)
        .await?
        .iter()
        .map(agent_memory_record)
        .collect::<Result<Vec<_>, _>>()
}

async fn load_world_objects(
    database: &dyn Database,
) -> Result<Vec<WorldObjectRecord>, BlimsStateError> {
    database
        .select("world_objects")
        .columns(&[
            "id",
            "room_id",
            "name",
            "kind",
            "symbol",
            "color",
            "interaction",
            "productivity_modifier",
        ])
        .sort("id", SortDirection::Asc)
        .execute(database)
        .await?
        .iter()
        .map(world_object_record)
        .collect::<Result<Vec<_>, _>>()
}

async fn load_agent_permissions(
    database: &dyn Database,
) -> Result<Vec<AgentPermissionRecord>, BlimsStateError> {
    database
        .select("agent_permissions")
        .columns(&[
            "agent_id",
            "bcode_agent_id",
            "bash",
            "read",
            "write",
            "edit",
            "external_directory",
        ])
        .sort("agent_id", SortDirection::Asc)
        .execute(database)
        .await?
        .iter()
        .map(agent_permission_record)
        .collect::<Result<Vec<_>, _>>()
}

async fn load_permission_escalations(
    database: &dyn Database,
) -> Result<Vec<PermissionEscalationRecord>, BlimsStateError> {
    database
        .select("permission_escalations")
        .columns(&[
            "id",
            "agent_id",
            "category",
            "requested_policy",
            "reason",
            "status",
        ])
        .sort("id", SortDirection::Desc)
        .execute(database)
        .await?
        .iter()
        .map(permission_escalation_record)
        .collect::<Result<Vec<_>, _>>()
}

async fn load_contract_violations(
    database: &dyn Database,
) -> Result<Vec<ContractViolationRecord>, BlimsStateError> {
    database
        .select("contract_violations")
        .columns(&["id", "agent_id", "severity", "summary", "action"])
        .sort("id", SortDirection::Desc)
        .execute(database)
        .await?
        .iter()
        .map(contract_violation_record)
        .collect::<Result<Vec<_>, _>>()
}

async fn load_company_record(database: &dyn Database) -> Result<CompanyRecord, BlimsStateError> {
    let row = database
        .select("companies")
        .columns(&["name", "mission", "culture", "lifecycle_status"])
        .limit(1)
        .execute_first(database)
        .await?
        .ok_or(BlimsStateError::MissingColumn("companies"))?;
    Ok(CompanyRecord {
        name: required_text(&row, "name")?,
        mission: required_text(&row, "mission")?,
        culture: required_text(&row, "culture")?,
        lifecycle_status: required_text(&row, "lifecycle_status")?,
    })
}

async fn load_world_record(database: &dyn Database) -> Result<(String, String), BlimsStateError> {
    let row = database
        .select("worlds")
        .columns(&["theme", "player_room_id"])
        .limit(1)
        .execute_first(database)
        .await?
        .ok_or(BlimsStateError::MissingColumn("worlds"))?;
    Ok((
        required_text(&row, "theme")?,
        required_text(&row, "player_room_id")?,
    ))
}

fn room_record(row: &Row) -> Result<RoomRecord, BlimsStateError> {
    Ok(RoomRecord {
        id: required_text(row, "id")?,
        name: required_text(row, "name")?,
        purpose: required_text(row, "purpose")?,
        room_kind: required_text(row, "room_kind")?,
        productivity_modifier: required_i64(row, "productivity_modifier")?,
        x: required_i64(row, "x")?,
        y: required_i64(row, "y")?,
        symbol: required_text(row, "symbol")?,
        color: required_text(row, "color")?,
    })
}

fn agent_record(row: &Row) -> Result<AgentRecord, BlimsStateError> {
    Ok(AgentRecord {
        id: required_text(row, "id")?,
        name: required_text(row, "name")?,
        role: required_text(row, "role")?,
        department_id: required_text(row, "department_id")?,
        team_id: required_text(row, "team_id")?,
        status: required_text(row, "status")?,
        room_id: required_text(row, "room_id")?,
    })
}

fn agent_stats_record(row: &Row) -> Result<AgentStatsRecord, BlimsStateError> {
    Ok(AgentStatsRecord {
        agent_id: required_text(row, "agent_id")?,
        traits: required_text(row, "traits")?,
        skills: required_text(row, "skills")?,
        energy: required_i64(row, "energy")?,
        morale: required_i64(row, "morale")?,
        focus: required_i64(row, "focus")?,
        confidence: required_i64(row, "confidence")?,
        speed_modifier: required_i64(row, "speed_modifier")?,
        quality_modifier: required_i64(row, "quality_modifier")?,
        risk_modifier: required_i64(row, "risk_modifier")?,
        creativity_modifier: required_i64(row, "creativity_modifier")?,
        persistence_modifier: required_i64(row, "persistence_modifier")?,
        collaboration_modifier: required_i64(row, "collaboration_modifier")?,
    })
}

fn agent_relationship_record(row: &Row) -> Result<AgentRelationshipRecord, BlimsStateError> {
    Ok(AgentRelationshipRecord {
        agent_id: required_text(row, "agent_id")?,
        other_agent_id: required_text(row, "other_agent_id")?,
        affinity: required_i64(row, "affinity")?,
        trust: required_i64(row, "trust")?,
        notes: required_text(row, "notes")?,
    })
}

fn agent_memory_record(row: &Row) -> Result<AgentMemoryRecord, BlimsStateError> {
    Ok(AgentMemoryRecord {
        id: required_text(row, "id")?,
        agent_id: required_text(row, "agent_id")?,
        kind: required_text(row, "kind")?,
        summary: required_text(row, "summary")?,
        importance: required_i64(row, "importance")?,
    })
}

fn world_object_record(row: &Row) -> Result<WorldObjectRecord, BlimsStateError> {
    Ok(WorldObjectRecord {
        id: required_text(row, "id")?,
        room_id: required_text(row, "room_id")?,
        name: required_text(row, "name")?,
        kind: required_text(row, "kind")?,
        symbol: required_text(row, "symbol")?,
        color: required_text(row, "color")?,
        interaction: required_text(row, "interaction")?,
        productivity_modifier: required_i64(row, "productivity_modifier")?,
    })
}

fn agent_permission_record(row: &Row) -> Result<AgentPermissionRecord, BlimsStateError> {
    Ok(AgentPermissionRecord {
        agent_id: required_text(row, "agent_id")?,
        bcode_agent_id: required_text(row, "bcode_agent_id")?,
        bash: required_text(row, "bash")?,
        read: required_text(row, "read")?,
        write: required_text(row, "write")?,
        edit: required_text(row, "edit")?,
        external_directory: required_text(row, "external_directory")?,
    })
}

fn permission_escalation_record(row: &Row) -> Result<PermissionEscalationRecord, BlimsStateError> {
    Ok(PermissionEscalationRecord {
        id: required_text(row, "id")?,
        agent_id: required_text(row, "agent_id")?,
        category: required_text(row, "category")?,
        requested_policy: required_text(row, "requested_policy")?,
        reason: required_text(row, "reason")?,
        status: required_text(row, "status")?,
    })
}

fn contract_violation_record(row: &Row) -> Result<ContractViolationRecord, BlimsStateError> {
    Ok(ContractViolationRecord {
        id: required_text(row, "id")?,
        agent_id: required_text(row, "agent_id")?,
        severity: required_text(row, "severity")?,
        summary: required_text(row, "summary")?,
        action: required_text(row, "action")?,
    })
}

fn department_record(row: &Row) -> Result<DepartmentRecord, BlimsStateError> {
    Ok(DepartmentRecord {
        id: required_text(row, "id")?,
        name: required_text(row, "name")?,
        purpose: required_text(row, "purpose")?,
    })
}

fn team_record(row: &Row) -> Result<TeamRecord, BlimsStateError> {
    Ok(TeamRecord {
        id: required_text(row, "id")?,
        department_id: required_text(row, "department_id")?,
        name: required_text(row, "name")?,
        purpose: required_text(row, "purpose")?,
    })
}

fn initiative_summary(row: &Row) -> Result<InitiativeSummary, BlimsStateError> {
    Ok(InitiativeSummary {
        id: required_text(row, "id")?,
        title: required_text(row, "title")?,
        description: required_text(row, "description")?,
        status: required_text(row, "status")?,
        priority: required_i64(row, "priority")?,
    })
}

fn project_summary(row: &Row) -> Result<ProjectSummary, BlimsStateError> {
    Ok(ProjectSummary {
        id: required_text(row, "id")?,
        initiative_id: required_text(row, "initiative_id")?,
        title: required_text(row, "title")?,
        status: required_text(row, "status")?,
        rationale: required_text(row, "rationale")?,
    })
}

fn guidance_summary(row: &Row) -> Result<GuidanceSummary, BlimsStateError> {
    Ok(GuidanceSummary {
        id: required_text(row, "id")?,
        guidance: required_text(row, "guidance")?,
        strength: required_text(row, "strength")?,
        active: required_bool(row, "active")?,
    })
}

fn operation_summary(row: &Row) -> Result<BlimsOperationSummary, BlimsStateError> {
    Ok(BlimsOperationSummary {
        id: required_text(row, "id")?,
        command_id: required_text(row, "command_id")?,
        actor: required_text(row, "actor")?,
        frontend_id: required_text(row, "frontend_id")?,
        kind: required_text(row, "kind")?,
        priority: required_text(row, "priority")?,
        status: required_text(row, "status")?,
        result_event_id: row.get("result_event_id").and_then(|value| value.as_i64()),
        error: required_text(row, "error")?,
    })
}

fn event_summary(row: &Row) -> Result<BlimsEventSummary, BlimsStateError> {
    Ok(BlimsEventSummary {
        id: required_i64(row, "id")?,
        event_version: required_i64(row, "event_version")?,
        company_id: required_text(row, "company_id")?,
        kind: required_text(row, "kind")?,
        summary: required_text(row, "summary")?,
        payload_json: required_text(row, "payload_json")?,
        correlation_id: required_text(row, "correlation_id")?,
        causation_id: required_text(row, "causation_id")?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ProjectionState {
    lifecycle_status: String,
    initiatives: Vec<InitiativeSummary>,
    projects: Vec<ProjectSummary>,
    guidance: Vec<GuidanceSummary>,
    artifacts: Vec<ArtifactDetail>,
    proposals: Vec<WorkProposalSummary>,
    tasks: Vec<TaskSummary>,
    rooms: Vec<RoomRecord>,
    agents: Vec<AgentRecord>,
    agent_stats: Vec<AgentStatsRecord>,
    relationships: Vec<AgentRelationshipRecord>,
    memories: Vec<AgentMemoryRecord>,
    permissions: Vec<AgentPermissionRecord>,
    permission_escalations: Vec<PermissionEscalationRecord>,
    world_objects: Vec<WorldObjectRecord>,
    contract_violations: Vec<ContractViolationRecord>,
    departments: Vec<DepartmentRecord>,
    teams: Vec<TeamRecord>,
}

impl ProjectionState {
    fn report(&self, events_replayed: usize) -> ProjectionRebuildReport {
        ProjectionRebuildReport {
            events_replayed,
            initiatives_projected: self.initiatives.len(),
            guidance_projected: self.guidance.len(),
            artifacts_projected: self.artifacts.len(),
            proposals_projected: self.proposals.len(),
            tasks_projected: self.tasks.len(),
            agents_projected: self.agents.len(),
            departments_projected: self.departments.len(),
            teams_projected: self.teams.len(),
            rooms_projected: self.rooms.len(),
            lifecycle_status: self.lifecycle_status.clone(),
        }
    }
}

fn replay_events(events: &[BlimsEventSummary]) -> Result<ProjectionState, BlimsStateError> {
    let mut state = ProjectionState {
        lifecycle_status: "running".to_string(),
        ..ProjectionState::default()
    };
    for event in events {
        replay_event(event, &mut state)?;
    }
    Ok(state)
}

fn replay_event(
    event: &BlimsEventSummary,
    state: &mut ProjectionState,
) -> Result<(), BlimsStateError> {
    let payload =
        serde_json::from_str::<BlimsEventPayload>(&event.payload_json).map_err(|source| {
            BlimsStateError::EventReplay {
                event_id: event.id,
                kind: event.kind.clone(),
                source,
            }
        })?;
    match payload {
        BlimsEventPayload::CompanyLifecycleSet { lifecycle_status } => {
            state.lifecycle_status = lifecycle_status;
        }
        BlimsEventPayload::InitiativeCreated { initiative } => {
            upsert_by_id(&mut state.initiatives, initiative, |initiative| {
                &initiative.id
            });
        }
        BlimsEventPayload::ProjectCreated { project } => {
            upsert_by_id(&mut state.projects, project, |project| &project.id);
        }
        BlimsEventPayload::InitiativeStatusSet {
            initiative_id,
            status,
        } => {
            if let Some(initiative) = state
                .initiatives
                .iter_mut()
                .find(|initiative| initiative.id == initiative_id)
            {
                initiative.status = status;
            }
        }
        BlimsEventPayload::GuidanceSet { guidance } => {
            upsert_by_id(&mut state.guidance, guidance, |guidance| &guidance.id);
        }
        BlimsEventPayload::InitiativeGuidanceSet { guidance, .. } => {
            upsert_by_id(&mut state.guidance, guidance, |guidance| &guidance.id);
        }
        BlimsEventPayload::ProposalRegistered { .. }
        | BlimsEventPayload::WorktreeRecorded { .. }
        | BlimsEventPayload::ProposalStatusSet { .. } => {
            replay_proposal_event(payload, state);
        }
        BlimsEventPayload::ArtifactCreated { .. } | BlimsEventPayload::ArtifactStatusSet { .. } => {
            replay_artifact_event(payload, state);
        }
        BlimsEventPayload::TaskCreated { task } => {
            upsert_by_id(&mut state.tasks, task, |task| &task.id);
        }
        BlimsEventPayload::TaskOutcomeRecorded { stats, .. } => {
            upsert_by_id(&mut state.agent_stats, stats, |stats| &stats.agent_id);
        }
        BlimsEventPayload::AgentHired { .. }
        | BlimsEventPayload::AgentMoved { .. }
        | BlimsEventPayload::AgentPlanningCycleRecorded { .. }
        | BlimsEventPayload::AiWorkPrepared { .. }
        | BlimsEventPayload::AgentStatsSet { .. }
        | BlimsEventPayload::AgentRelationshipSet { .. }
        | BlimsEventPayload::AgentMemoryRecorded { .. }
        | BlimsEventPayload::AgentPermissionSet { .. }
        | BlimsEventPayload::PermissionEscalationRequested { .. }
        | BlimsEventPayload::ContractViolationRecorded { .. }
        | BlimsEventPayload::AgentStatusSet { .. } => {
            replay_agent_event(payload, state);
        }
        BlimsEventPayload::AgentContractUpdated { .. }
        | BlimsEventPayload::CommandSubmitted { .. }
        | BlimsEventPayload::DashboardProjectionRefreshed { .. }
        | BlimsEventPayload::OperationStatusSet { .. }
        | BlimsEventPayload::StarterOfficeSelected { .. }
        | BlimsEventPayload::PlayerMoved { .. }
        | BlimsEventPayload::TaskWorkScheduled { .. }
        | BlimsEventPayload::InitiativePlanImported { .. }
        | BlimsEventPayload::ReportGenerated { .. }
        | BlimsEventPayload::ConversationMessageRecorded { .. }
        | BlimsEventPayload::ConversationRecorded { .. } => {}
        BlimsEventPayload::DepartmentCreated { .. }
        | BlimsEventPayload::TeamCreated { .. }
        | BlimsEventPayload::WorldRoomCreated { .. }
        | BlimsEventPayload::WorldObjectPlaced { .. } => {
            replay_org_world_event(payload, state);
        }
    }
    Ok(())
}

fn replay_org_world_event(payload: BlimsEventPayload, state: &mut ProjectionState) {
    match payload {
        BlimsEventPayload::DepartmentCreated { id, name, purpose } => {
            upsert_by_id(
                &mut state.departments,
                DepartmentRecord { id, name, purpose },
                |department| &department.id,
            );
        }
        BlimsEventPayload::TeamCreated {
            id,
            department_id,
            name,
            purpose,
        } => {
            upsert_by_id(
                &mut state.teams,
                TeamRecord {
                    id,
                    department_id,
                    name,
                    purpose,
                },
                |team| &team.id,
            );
        }
        BlimsEventPayload::WorldRoomCreated { room } => {
            upsert_by_id(&mut state.rooms, room.into(), |room| &room.id);
        }
        BlimsEventPayload::WorldObjectPlaced { object } => {
            upsert_by_id(&mut state.world_objects, object, |object| &object.id);
        }
        _ => {}
    }
}

fn replay_proposal_event(payload: BlimsEventPayload, state: &mut ProjectionState) {
    match payload {
        BlimsEventPayload::ProposalRegistered { proposal } => {
            upsert_by_id(&mut state.proposals, proposal, |proposal| &proposal.id);
        }
        BlimsEventPayload::ProposalStatusSet {
            proposal_id,
            status,
        } => {
            if let Some(proposal) = state
                .proposals
                .iter_mut()
                .find(|proposal| proposal.id == proposal_id)
            {
                proposal.status = status;
            }
        }
        _ => {}
    }
}

fn replay_artifact_event(payload: BlimsEventPayload, state: &mut ProjectionState) {
    match payload {
        BlimsEventPayload::ArtifactCreated { mut artifact } => {
            artifact.status = "draft".to_string();
            upsert_by_id(&mut state.artifacts, artifact, |artifact| &artifact.id);
        }
        BlimsEventPayload::ArtifactStatusSet {
            artifact_id,
            status,
        } => {
            if let Some(artifact) = state
                .artifacts
                .iter_mut()
                .find(|artifact| artifact.id == artifact_id)
            {
                artifact.status = status;
            }
        }
        _ => {}
    }
}

fn replay_agent_event(payload: BlimsEventPayload, state: &mut ProjectionState) {
    match payload {
        BlimsEventPayload::AgentHired { agent } => {
            upsert_by_id(&mut state.agents, agent.into(), |agent| &agent.id);
        }
        BlimsEventPayload::AgentMoved { agent_id, room_id } => {
            if let Some(agent) = state.agents.iter_mut().find(|agent| agent.id == agent_id) {
                agent.room_id = room_id;
            }
        }
        BlimsEventPayload::AgentStatsSet { stats } => {
            upsert_by_id(&mut state.agent_stats, stats, |stats| &stats.agent_id);
        }
        BlimsEventPayload::AgentRelationshipSet { relationship } => {
            if let Some(existing) = state.relationships.iter_mut().find(|existing| {
                existing.agent_id == relationship.agent_id
                    && existing.other_agent_id == relationship.other_agent_id
            }) {
                *existing = relationship;
            } else {
                state.relationships.push(relationship);
            }
        }
        BlimsEventPayload::AgentMemoryRecorded { memory } => {
            upsert_by_id(&mut state.memories, memory, |memory| &memory.id);
        }
        BlimsEventPayload::AgentPermissionSet { permission } => {
            upsert_by_id(&mut state.permissions, permission, |permission| {
                &permission.agent_id
            });
        }
        BlimsEventPayload::PermissionEscalationRequested { request } => {
            upsert_by_id(&mut state.permission_escalations, request, |request| {
                &request.id
            });
        }
        BlimsEventPayload::ContractViolationRecorded { violation } => {
            upsert_by_id(&mut state.contract_violations, violation, |violation| {
                &violation.id
            });
        }
        BlimsEventPayload::AgentStatusSet { agent_id, status } => {
            if let Some(agent) = state.agents.iter_mut().find(|agent| agent.id == agent_id) {
                agent.status = status;
            }
        }
        _ => {}
    }
}

fn upsert_by_id<T>(items: &mut Vec<T>, item: T, id: impl Fn(&T) -> &String) {
    let item_id = id(&item).clone();
    if let Some(existing) = items.iter_mut().find(|existing| id(existing) == &item_id) {
        *existing = item;
    } else {
        items.push(item);
    }
}

async fn apply_projection_state(
    database: &dyn Database,
    state: &ProjectionState,
) -> Result<(), BlimsStateError> {
    database
        .update("companies")
        .value("lifecycle_status", state.lifecycle_status.clone())
        .filter(Box::new(where_eq("id", "default")))
        .execute(database)
        .await?;
    replace_initiative_projections(database, &state.initiatives).await?;
    replace_project_projections(database, &state.projects).await?;
    replace_guidance_projections(database, &state.guidance).await?;
    replace_artifact_projections(database, &state.artifacts).await?;
    replace_proposal_projections(database, &state.proposals).await?;
    replace_task_projections(database, &state.tasks).await?;
    replace_department_projections(database, &state.departments).await?;
    replace_team_projections(database, &state.teams).await?;
    replace_world_room_projections(database, &state.rooms).await?;
    replace_agent_projections(database, &state.agents).await?;
    replace_agent_stats_projections(database, &state.agent_stats).await?;
    replace_agent_relationship_projections(database, &state.relationships).await?;
    replace_agent_memory_projections(database, &state.memories).await?;
    replace_agent_permission_projections(database, &state.permissions).await?;
    replace_permission_escalation_projections(database, &state.permission_escalations).await?;
    replace_contract_violation_projections(database, &state.contract_violations).await?;
    replace_world_object_projections(database, &state.world_objects).await
}

async fn replace_one_initiative_projection(
    database: &dyn Database,
    initiative: &InitiativeSummary,
) -> Result<(), BlimsStateError> {
    database
        .delete("initiatives")
        .filter(Box::new(where_eq("id", initiative.id.clone())))
        .execute(database)
        .await?;
    database
        .insert("initiatives")
        .value("id", initiative.id.clone())
        .value("company_id", "default")
        .value("title", initiative.title.clone())
        .value("description", initiative.description.clone())
        .value("status", initiative.status.clone())
        .value("priority", initiative.priority)
        .value("created_by", "event_replay")
        .value("guidance", "")
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_initiative_projections(
    database: &dyn Database,
    initiatives: &[InitiativeSummary],
) -> Result<(), BlimsStateError> {
    database.delete("initiatives").execute(database).await?;
    for initiative in initiatives {
        replace_one_initiative_projection(database, initiative).await?;
    }
    Ok(())
}

async fn replace_one_project_projection(
    database: &dyn Database,
    project: &ProjectSummary,
) -> Result<(), BlimsStateError> {
    database
        .delete("projects")
        .filter(Box::new(where_eq("id", project.id.clone())))
        .execute(database)
        .await?;
    database
        .insert("projects")
        .value("id", project.id.clone())
        .value("initiative_id", project.initiative_id.clone())
        .value("title", project.title.clone())
        .value("status", project.status.clone())
        .value("rationale", project.rationale.clone())
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_project_projections(
    database: &dyn Database,
    projects: &[ProjectSummary],
) -> Result<(), BlimsStateError> {
    database.delete("projects").execute(database).await?;
    for project in projects {
        replace_one_project_projection(database, project).await?;
    }
    Ok(())
}

async fn replace_one_guidance_projection(
    database: &dyn Database,
    item: &GuidanceSummary,
) -> Result<(), BlimsStateError> {
    database
        .delete("executive_guidance")
        .filter(Box::new(where_eq("id", item.id.clone())))
        .execute(database)
        .await?;
    database
        .insert("executive_guidance")
        .value("id", item.id.clone())
        .value("company_id", "default")
        .value("guidance", item.guidance.clone())
        .value("strength", item.strength.clone())
        .value("active", item.active)
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_guidance_projections(
    database: &dyn Database,
    guidance: &[GuidanceSummary],
) -> Result<(), BlimsStateError> {
    database
        .delete("executive_guidance")
        .execute(database)
        .await?;
    for item in guidance {
        replace_one_guidance_projection(database, item).await?;
    }
    Ok(())
}

async fn replace_one_artifact_projection(
    database: &dyn Database,
    artifact: &ArtifactDetail,
    status: &str,
) -> Result<(), BlimsStateError> {
    database
        .delete("artifacts")
        .filter(Box::new(where_eq("id", artifact.id.clone())))
        .execute(database)
        .await?;
    database
        .insert("artifacts")
        .value("id", artifact.id.clone())
        .value("initiative_id", artifact.initiative_id.clone())
        .value("kind", artifact.kind.clone())
        .value("title", artifact.title.clone())
        .value("status", status)
        .value("payload_json", artifact.payload_json.clone())
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_artifact_projections(
    database: &dyn Database,
    artifacts: &[ArtifactDetail],
) -> Result<(), BlimsStateError> {
    database.delete("artifacts").execute(database).await?;
    for artifact in artifacts {
        replace_one_artifact_projection(database, artifact, &artifact.status).await?;
    }
    Ok(())
}

async fn replace_one_proposal_projection(
    database: &dyn Database,
    proposal: &WorkProposalSummary,
) -> Result<(), BlimsStateError> {
    database
        .delete("work_proposals")
        .filter(Box::new(where_eq("id", proposal.id.clone())))
        .execute(database)
        .await?;
    database
        .insert("work_proposals")
        .value("id", proposal.id.clone())
        .value("task_id", proposal.task_id.clone())
        .value("initiative_id", proposal.initiative_id.clone())
        .value("agent_id", proposal.agent_id.clone())
        .value("session_id", proposal.session_id.clone())
        .value("worktree_path", proposal.worktree_path.clone())
        .value("branch", proposal.branch.clone())
        .value("status", proposal.status.clone())
        .value("summary", proposal.summary.clone())
        .value("validation_notes", proposal.validation_notes.clone())
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_proposal_projections(
    database: &dyn Database,
    proposals: &[WorkProposalSummary],
) -> Result<(), BlimsStateError> {
    database.delete("work_proposals").execute(database).await?;
    for proposal in proposals {
        replace_one_proposal_projection(database, proposal).await?;
    }
    Ok(())
}

async fn replace_one_worktree_projection(
    database: &dyn Database,
    worktree: &WorktreeRecord,
) -> Result<(), BlimsStateError> {
    database
        .delete("worktree_records")
        .filter(Box::new(where_eq("id", worktree.id.clone())))
        .execute(database)
        .await?;
    database
        .insert("worktree_records")
        .value("id", worktree.id.clone())
        .value("task_id", worktree.task_id.clone())
        .value("agent_id", worktree.agent_id.clone())
        .value("session_id", worktree.session_id.clone())
        .value("path", worktree.path.clone())
        .value("branch", worktree.branch.clone())
        .value("status", worktree.status.clone())
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_one_report_projection(
    database: &dyn Database,
    report: &ReportRecord,
) -> Result<(), BlimsStateError> {
    database
        .delete("reports")
        .filter(Box::new(where_eq("id", report.id.clone())))
        .execute(database)
        .await?;
    database
        .insert("reports")
        .value("id", report.id.clone())
        .value("report_type", report.report_type.clone())
        .value("title", report.title.clone())
        .value("summary", report.summary.clone())
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_one_conversation_projection(
    database: &dyn Database,
    conversation: &ConversationRecord,
) -> Result<(), BlimsStateError> {
    database
        .delete("conversations")
        .filter(Box::new(where_eq("id", conversation.id.clone())))
        .execute(database)
        .await?;
    database
        .insert("conversations")
        .value("id", conversation.id.clone())
        .value("agent_id", conversation.agent_id.clone())
        .value("session_id", conversation.session_id.clone())
        .value("status", conversation.status.clone())
        .value("summary", conversation.summary.clone())
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_one_task_projection(
    database: &dyn Database,
    task: &TaskSummary,
) -> Result<(), BlimsStateError> {
    database
        .delete("tasks")
        .filter(Box::new(where_eq("id", task.id.clone())))
        .execute(database)
        .await?;
    database
        .insert("tasks")
        .value("id", task.id.clone())
        .value("initiative_id", task.initiative_id.clone())
        .value("title", task.title.clone())
        .value("description", task.description.clone())
        .value("status", task.status.clone())
        .value("assigned_agent_id", task.assigned_agent_id.clone())
        .value("rationale", task.rationale.clone())
        .value("blocker", task.blocker.clone())
        .value("priority", task.priority)
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_task_projections(
    database: &dyn Database,
    tasks: &[TaskSummary],
) -> Result<(), BlimsStateError> {
    database.delete("tasks").execute(database).await?;
    for task in tasks {
        replace_one_task_projection(database, task).await?;
    }
    Ok(())
}

async fn replace_one_department_projection(
    database: &dyn Database,
    department: &DepartmentRecord,
) -> Result<(), BlimsStateError> {
    database
        .insert("departments")
        .value("id", department.id.clone())
        .value("company_id", "default")
        .value("name", department.name.clone())
        .value("purpose", department.purpose.clone())
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_department_projections(
    database: &dyn Database,
    departments: &[DepartmentRecord],
) -> Result<(), BlimsStateError> {
    database.delete("departments").execute(database).await?;
    for department in departments {
        replace_one_department_projection(database, department).await?;
    }
    Ok(())
}

async fn replace_one_team_projection(
    database: &dyn Database,
    team: &TeamRecord,
) -> Result<(), BlimsStateError> {
    database
        .insert("teams")
        .value("id", team.id.clone())
        .value("department_id", team.department_id.clone())
        .value("name", team.name.clone())
        .value("purpose", team.purpose.clone())
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_team_projections(
    database: &dyn Database,
    teams: &[TeamRecord],
) -> Result<(), BlimsStateError> {
    database.delete("teams").execute(database).await?;
    for team in teams {
        replace_one_team_projection(database, team).await?;
    }
    Ok(())
}

async fn replace_one_world_room_projection(
    database: &dyn Database,
    room: &RoomRecord,
) -> Result<(), BlimsStateError> {
    database
        .delete("world_rooms")
        .filter(Box::new(where_eq("id", room.id.clone())))
        .execute(database)
        .await?;
    database
        .insert("world_rooms")
        .value("id", room.id.clone())
        .value("world_id", "default")
        .value("name", room.name.clone())
        .value("purpose", room.purpose.clone())
        .value("room_kind", room.room_kind.clone())
        .value("productivity_modifier", room.productivity_modifier)
        .value("x", room.x)
        .value("y", room.y)
        .value("symbol", room.symbol.clone())
        .value("color", room.color.clone())
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_world_room_projections(
    database: &dyn Database,
    rooms: &[RoomRecord],
) -> Result<(), BlimsStateError> {
    database.delete("world_rooms").execute(database).await?;
    for room in rooms {
        replace_one_world_room_projection(database, room).await?;
    }
    Ok(())
}

async fn replace_one_world_object_projection(
    database: &dyn Database,
    object: &WorldObjectRecord,
) -> Result<(), BlimsStateError> {
    database
        .delete("world_objects")
        .filter(Box::new(where_eq("id", object.id.clone())))
        .execute(database)
        .await?;
    database
        .insert("world_objects")
        .value("id", object.id.clone())
        .value("room_id", object.room_id.clone())
        .value("name", object.name.clone())
        .value("kind", object.kind.clone())
        .value("symbol", object.symbol.clone())
        .value("color", object.color.clone())
        .value("interaction", object.interaction.clone())
        .value("productivity_modifier", object.productivity_modifier)
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_world_object_projections(
    database: &dyn Database,
    objects: &[WorldObjectRecord],
) -> Result<(), BlimsStateError> {
    database.delete("world_objects").execute(database).await?;
    for object in objects {
        replace_one_world_object_projection(database, object).await?;
    }
    Ok(())
}

async fn replace_one_agent_projection(
    database: &dyn Database,
    agent: &AgentRecord,
) -> Result<(), BlimsStateError> {
    database
        .delete("agents")
        .filter(Box::new(where_eq("id", agent.id.clone())))
        .execute(database)
        .await?;
    database
        .insert("agents")
        .value("id", agent.id.clone())
        .value("name", agent.name.clone())
        .value("role", agent.role.clone())
        .value("department_id", agent.department_id.clone())
        .value("team_id", agent.team_id.clone())
        .value("status", agent.status.clone())
        .value("room_id", agent.room_id.clone())
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_agent_projections(
    database: &dyn Database,
    agents: &[AgentRecord],
) -> Result<(), BlimsStateError> {
    database.delete("agents").execute(database).await?;
    for agent in agents {
        replace_one_agent_projection(database, agent).await?;
    }
    Ok(())
}

async fn replace_one_agent_stats_projection(
    database: &dyn Database,
    stats: &AgentStatsRecord,
) -> Result<(), BlimsStateError> {
    database
        .delete("agent_stats")
        .filter(Box::new(where_eq("agent_id", stats.agent_id.clone())))
        .execute(database)
        .await?;
    database
        .insert("agent_stats")
        .value("agent_id", stats.agent_id.clone())
        .value("traits", stats.traits.clone())
        .value("skills", stats.skills.clone())
        .value("energy", stats.energy)
        .value("morale", stats.morale)
        .value("focus", stats.focus)
        .value("confidence", stats.confidence)
        .value("speed_modifier", stats.speed_modifier)
        .value("quality_modifier", stats.quality_modifier)
        .value("risk_modifier", stats.risk_modifier)
        .value("creativity_modifier", stats.creativity_modifier)
        .value("persistence_modifier", stats.persistence_modifier)
        .value("collaboration_modifier", stats.collaboration_modifier)
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_agent_stats_projections(
    database: &dyn Database,
    stats: &[AgentStatsRecord],
) -> Result<(), BlimsStateError> {
    database.delete("agent_stats").execute(database).await?;
    for item in stats {
        replace_one_agent_stats_projection(database, item).await?;
    }
    Ok(())
}

async fn replace_one_agent_relationship_projection(
    database: &dyn Database,
    relationship: &AgentRelationshipRecord,
) -> Result<(), BlimsStateError> {
    let id = relationship_id(&relationship.agent_id, &relationship.other_agent_id);
    database
        .delete("agent_relationships")
        .filter(Box::new(where_eq("id", id.clone())))
        .execute(database)
        .await?;
    database
        .insert("agent_relationships")
        .value("id", id)
        .value("agent_id", relationship.agent_id.clone())
        .value("other_agent_id", relationship.other_agent_id.clone())
        .value("affinity", relationship.affinity)
        .value("trust", relationship.trust)
        .value("notes", relationship.notes.clone())
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_agent_relationship_projections(
    database: &dyn Database,
    relationships: &[AgentRelationshipRecord],
) -> Result<(), BlimsStateError> {
    database
        .delete("agent_relationships")
        .execute(database)
        .await?;
    for relationship in relationships {
        replace_one_agent_relationship_projection(database, relationship).await?;
    }
    Ok(())
}

async fn replace_one_agent_memory_projection(
    database: &dyn Database,
    memory: &AgentMemoryRecord,
) -> Result<(), BlimsStateError> {
    database
        .delete("memories")
        .filter(Box::new(where_eq("id", memory.id.clone())))
        .execute(database)
        .await?;
    database
        .insert("memories")
        .value("id", memory.id.clone())
        .value("agent_id", memory.agent_id.clone())
        .value("kind", memory.kind.clone())
        .value("summary", memory.summary.clone())
        .value("importance", memory.importance)
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_agent_memory_projections(
    database: &dyn Database,
    memories: &[AgentMemoryRecord],
) -> Result<(), BlimsStateError> {
    database.delete("memories").execute(database).await?;
    for memory in memories {
        replace_one_agent_memory_projection(database, memory).await?;
    }
    Ok(())
}

async fn replace_one_agent_permission_projection(
    database: &dyn Database,
    permission: &AgentPermissionRecord,
) -> Result<(), BlimsStateError> {
    database
        .delete("agent_permissions")
        .filter(Box::new(where_eq("agent_id", permission.agent_id.clone())))
        .execute(database)
        .await?;
    database
        .insert("agent_permissions")
        .value("agent_id", permission.agent_id.clone())
        .value("bcode_agent_id", permission.bcode_agent_id.clone())
        .value("bash", permission.bash.clone())
        .value("read", permission.read.clone())
        .value("write", permission.write.clone())
        .value("edit", permission.edit.clone())
        .value("external_directory", permission.external_directory.clone())
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_agent_permission_projections(
    database: &dyn Database,
    permissions: &[AgentPermissionRecord],
) -> Result<(), BlimsStateError> {
    database
        .delete("agent_permissions")
        .execute(database)
        .await?;
    for permission in permissions {
        replace_one_agent_permission_projection(database, permission).await?;
    }
    Ok(())
}

async fn replace_one_permission_escalation_projection(
    database: &dyn Database,
    escalation: &PermissionEscalationRecord,
) -> Result<(), BlimsStateError> {
    database
        .delete("permission_escalations")
        .filter(Box::new(where_eq("id", escalation.id.clone())))
        .execute(database)
        .await?;
    database
        .insert("permission_escalations")
        .value("id", escalation.id.clone())
        .value("agent_id", escalation.agent_id.clone())
        .value("category", escalation.category.clone())
        .value("requested_policy", escalation.requested_policy.clone())
        .value("reason", escalation.reason.clone())
        .value("status", escalation.status.clone())
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_permission_escalation_projections(
    database: &dyn Database,
    escalations: &[PermissionEscalationRecord],
) -> Result<(), BlimsStateError> {
    database
        .delete("permission_escalations")
        .execute(database)
        .await?;
    for escalation in escalations {
        replace_one_permission_escalation_projection(database, escalation).await?;
    }
    Ok(())
}

async fn replace_one_contract_violation_projection(
    database: &dyn Database,
    violation: &ContractViolationRecord,
) -> Result<(), BlimsStateError> {
    database
        .delete("contract_violations")
        .filter(Box::new(where_eq("id", violation.id.clone())))
        .execute(database)
        .await?;
    database
        .insert("contract_violations")
        .value("id", violation.id.clone())
        .value("agent_id", violation.agent_id.clone())
        .value("severity", violation.severity.clone())
        .value("summary", violation.summary.clone())
        .value("action", violation.action.clone())
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_contract_violation_projections(
    database: &dyn Database,
    violations: &[ContractViolationRecord],
) -> Result<(), BlimsStateError> {
    database
        .delete("contract_violations")
        .execute(database)
        .await?;
    for violation in violations {
        replace_one_contract_violation_projection(database, violation).await?;
    }
    Ok(())
}

fn relationship_id(agent_id: &str, other_agent_id: &str) -> String {
    format!("{agent_id}->{other_agent_id}")
}

async fn replace_one_agent_contract_projection(
    database: &dyn Database,
    agent_id: &str,
    responsibilities: &str,
    restrictions: &str,
    escalation: &str,
) -> Result<(), BlimsStateError> {
    database
        .delete("agent_contracts")
        .filter(Box::new(where_eq("agent_id", agent_id)))
        .execute(database)
        .await?;
    database
        .insert("agent_contracts")
        .value("agent_id", agent_id)
        .value("responsibilities", responsibilities)
        .value("restrictions", restrictions)
        .value("escalation", escalation)
        .value(
            "reporting_expectations",
            "Report status, blockers, rationale, validation, risk, and next steps before requesting CEO approval.",
        )
        .value(
            "disciplinary_policy",
            "Disagreeing with CEO guidance is allowed when logged with rationale; unsafe or hidden violations trigger warning, suspension, or firing workflows.",
        )
        .execute(database)
        .await?;
    Ok(())
}

fn task_summary(row: &Row) -> Result<TaskSummary, BlimsStateError> {
    Ok(TaskSummary {
        id: required_text(row, "id")?,
        initiative_id: required_text(row, "initiative_id")?,
        title: required_text(row, "title")?,
        description: required_text(row, "description")?,
        status: required_text(row, "status")?,
        assigned_agent_id: required_text(row, "assigned_agent_id")?,
        rationale: required_text(row, "rationale")?,
        blocker: required_text(row, "blocker")?,
        priority: required_i64(row, "priority")?,
    })
}

fn artifact_summary(row: &Row) -> Result<ArtifactSummary, BlimsStateError> {
    Ok(ArtifactSummary {
        id: required_text(row, "id")?,
        initiative_id: required_text(row, "initiative_id")?,
        kind: required_text(row, "kind")?,
        title: required_text(row, "title")?,
        status: required_text(row, "status")?,
    })
}

fn artifact_detail(row: &Row) -> Result<ArtifactDetail, BlimsStateError> {
    Ok(ArtifactDetail {
        id: required_text(row, "id")?,
        initiative_id: required_text(row, "initiative_id")?,
        kind: required_text(row, "kind")?,
        title: required_text(row, "title")?,
        status: required_text(row, "status")?,
        payload_json: required_text(row, "payload_json")?,
    })
}

fn proposal_summary(row: &Row) -> Result<WorkProposalSummary, BlimsStateError> {
    Ok(WorkProposalSummary {
        id: required_text(row, "id")?,
        task_id: required_text(row, "task_id")?,
        initiative_id: required_text(row, "initiative_id")?,
        agent_id: required_text(row, "agent_id")?,
        session_id: required_text(row, "session_id")?,
        worktree_path: required_text(row, "worktree_path")?,
        branch: required_text(row, "branch")?,
        status: required_text(row, "status")?,
        summary: required_text(row, "summary")?,
        validation_notes: required_text(row, "validation_notes")?,
    })
}

fn required_text(row: &Row, column: &'static str) -> Result<String, BlimsStateError> {
    row.get(column)
        .and_then(|value| value.as_str().map(ToString::to_string))
        .ok_or(BlimsStateError::MissingColumn(column))
}

fn required_i64(row: &Row, column: &'static str) -> Result<i64, BlimsStateError> {
    row.get(column)
        .and_then(|value| value.as_i64())
        .ok_or(BlimsStateError::MissingColumn(column))
}

fn required_bool(row: &Row, column: &'static str) -> Result<bool, BlimsStateError> {
    row.get(column)
        .and_then(|value| {
            value
                .as_bool()
                .or_else(|| value.as_i64().map(|value| value != 0))
        })
        .ok_or(BlimsStateError::MissingColumn(column))
}

fn generate_morning_report(
    request: &WorkspaceRequest,
    event_context: &EventContext,
) -> Result<MorningReport, BlimsStateError> {
    let event_context = event_context.clone();
    let working_directory = request.working_directory.clone();
    with_database(&working_directory, move |database| {
        Box::pin(async move {
            let data = load_company_data_from_database(database).await?;
            let report = morning_report(&data);
            append_event(
                database,
                &event_context,
                "report.generated",
                report.title.clone(),
                &BlimsEventPayload::ReportGenerated {
                    report: ReportRecord {
                        id: format!("morning-{}", latest_event_id(database).await? + 1),
                        report_type: "morning".to_string(),
                        title: report.title.clone(),
                        summary: report.bullets.join("\n"),
                    },
                },
            )
            .await?;
            Ok::<_, BlimsStateError>(report)
        })
    })
}

fn morning_report(data: &CompanyData) -> MorningReport {
    let mut bullets = vec![
        format!("Mission: {}", data.company.mission),
        format!("Culture: {}", data.company.culture),
        format!(
            "{} starter agents are active across {} rooms.",
            data.agents.len(),
            data.rooms.len()
        ),
    ];
    let completed = data
        .tasks
        .iter()
        .filter(|task| task.status == "done" || task.status == "completed")
        .count();
    if completed > 0 {
        bullets.push(format!("Completed work items: {completed}"));
    }
    let paused = data
        .initiatives
        .iter()
        .filter(|initiative| initiative.status == "paused")
        .count();
    if paused > 0 {
        bullets.push(format!(
            "Paused initiatives needing rationale review: {paused}"
        ));
    }
    let blocked = data
        .tasks
        .iter()
        .filter(|task| task.status == "blocked")
        .count();
    if blocked > 0 {
        bullets.push(format!("Blockers: {blocked} task(s) blocked"));
    }
    if data.guidance.is_empty() {
        bullets.push("No active CEO guidance yet.".to_string());
    } else {
        bullets.push(format!("Active CEO guidance: {}", data.guidance.len()));
        if let Some(guidance) = data.guidance.first() {
            bullets.push(format!("Top guidance: {}", guidance.guidance));
        }
    }
    if data.initiatives.is_empty() {
        bullets.push(
            "No active initiatives yet. Add CEO guidance to wake the company loop.".to_string(),
        );
    } else {
        bullets.push(format!("Active initiatives: {}", data.initiatives.len()));
        if let Some(initiative) = data.initiatives.first() {
            bullets.push(format!("Top initiative: {}", initiative.title));
        }
    }
    if data.proposals.is_empty() {
        bullets.push("No work proposals are open yet.".to_string());
    } else {
        let drafts = data
            .proposals
            .iter()
            .filter(|proposal| proposal.status == "draft")
            .count();
        let ready = data
            .proposals
            .iter()
            .filter(|proposal| proposal.status == "ready_for_review")
            .count();
        bullets.push(format!("Open work proposals: {}", data.proposals.len()));
        if drafts > 0 {
            bullets.push(format!("Draft proposals in progress: {drafts}"));
        }
        if ready > 0 {
            bullets.push(format!("Ready for CEO review: {ready}"));
        }
    }
    append_permission_escalation_report_bullets(data, &mut bullets);
    append_contract_violation_report_bullets(data, &mut bullets);
    if let Some(memory) = data.memories.first() {
        bullets.push(format!("Notable discovery: {}", memory.summary));
    }

    MorningReport {
        title: format!("{} morning report", data.company.name),
        bullets,
    }
}

fn append_permission_escalation_report_bullets(data: &CompanyData, bullets: &mut Vec<String>) {
    if data.permission_escalations.is_empty() {
        return;
    }
    bullets.push(format!(
        "Pending permission escalations: {}",
        data.permission_escalations.len()
    ));
    if let Some(escalation) = data.permission_escalations.first() {
        bullets.push(format!(
            "Top permission request [{}]: {} wants {} because {}",
            escalation.category,
            escalation.agent_id,
            escalation.requested_policy,
            escalation.reason
        ));
    }
}

fn append_contract_violation_report_bullets(data: &CompanyData, bullets: &mut Vec<String>) {
    if data.contract_violations.is_empty() {
        return;
    }
    bullets.push(format!(
        "Contract/permission issues: {}",
        data.contract_violations.len()
    ));
    if let Some(violation) = data.contract_violations.first() {
        bullets.push(format!(
            "Top issue [{}]: {} — {}",
            violation.severity, violation.agent_id, violation.summary
        ));
    }
}

fn department_report(
    data: &CompanyData,
    department_id: &str,
) -> Result<DepartmentReport, BlimsStateError> {
    let department = data
        .departments
        .iter()
        .find(|department| department.id == department_id)
        .ok_or_else(|| {
            BlimsStateError::InvalidRequest(format!("unknown department: {department_id}"))
        })?;
    let teams_count = data
        .teams
        .iter()
        .filter(|team| team.department_id == department.id)
        .count();
    let agents = data
        .agents
        .iter()
        .filter(|agent| agent.department_id == department.id)
        .collect::<Vec<_>>();
    let active_agents = agents
        .iter()
        .filter(|agent| agent.status != "suspended" && agent.status != "fired")
        .count();
    let tasks = data
        .tasks
        .iter()
        .filter(|task| {
            agents
                .iter()
                .any(|agent| agent.id == task.assigned_agent_id)
        })
        .collect::<Vec<_>>();
    let ready_reviews = data
        .proposals
        .iter()
        .filter(|proposal| proposal.status == "ready_for_review")
        .count();
    let mut bullets = vec![
        format!("Purpose: {}", department.purpose),
        format!("Teams: {teams_count}"),
        format!("Agents: {} total, {active_agents} active", agents.len()),
        format!("Assigned tasks: {}", tasks.len()),
        format!("Ready reviews across company: {ready_reviews}"),
        format!("Standup yesterday: moved priority work and kept context fresh."),
        format!("Standup today: focus on highest-priority active tasks and review queue."),
    ];
    let risk_count = agents
        .iter()
        .filter(|agent| {
            data.agent_stats
                .iter()
                .any(|stats| stats.agent_id == agent.id && (stats.morale < 50 || stats.focus < 50))
        })
        .count();
    if risk_count > 0 {
        bullets.push(format!(
            "Morale/focus risk: {risk_count} agent(s) need attention"
        ));
    }
    if let Some(task) = tasks.first() {
        bullets.push(format!("Top task: {} ({})", task.title, task.status));
    }
    Ok(DepartmentReport {
        title: format!("{} department report", department.name),
        department_id: department.id.clone(),
        bullets,
    })
}

fn agent_report(data: &CompanyData, agent_id: &str) -> Result<AgentReport, BlimsStateError> {
    let agent = data
        .agents
        .iter()
        .find(|agent| agent.id == agent_id)
        .ok_or_else(|| BlimsStateError::InvalidRequest(format!("unknown agent: {agent_id}")))?;
    let assigned_tasks = data
        .tasks
        .iter()
        .filter(|task| task.assigned_agent_id == agent.id)
        .collect::<Vec<_>>();
    let proposals = data
        .proposals
        .iter()
        .filter(|proposal| proposal.agent_id == agent.id)
        .collect::<Vec<_>>();
    let artifacts = count_agent_related_artifacts(data, &assigned_tasks);
    let mut bullets =
        agent_report_base_bullets(agent, assigned_tasks.len(), proposals.len(), artifacts);
    append_agent_context_bullets(data, agent, &mut bullets);
    if let Some(task) = assigned_tasks.first() {
        bullets.push(format!(
            "Current likely focus: {} ({})",
            task.title, task.status
        ));
    }
    if let Some(proposal) = proposals.first() {
        bullets.push(format!(
            "Latest proposal: {} ({})",
            proposal.summary, proposal.status
        ));
    }
    Ok(AgentReport {
        title: format!("{} agent report", agent.name),
        agent_id: agent.id.clone(),
        bullets,
    })
}

fn count_agent_related_artifacts(data: &CompanyData, assigned_tasks: &[&TaskSummary]) -> usize {
    data.artifacts
        .iter()
        .filter(|artifact| {
            assigned_tasks
                .iter()
                .any(|task| task.initiative_id == artifact.initiative_id)
        })
        .count()
}

fn agent_report_base_bullets(
    agent: &AgentRecord,
    assigned_tasks: usize,
    proposals: usize,
    artifacts: usize,
) -> Vec<String> {
    vec![
        format!("Role: {}", agent.role),
        format!("Status: {}", agent.status),
        format!("Location: {}", agent.room_id),
        format!("What I did: {proposals} proposal(s), {artifacts} related artifact(s)"),
        format!("What I am doing: {assigned_tasks} assigned task(s)"),
        "Why I chose this: following priority, CEO guidance, and company culture.".to_string(),
        "What I need: clear approval on ready proposals and help on blockers.".to_string(),
    ]
}

fn append_agent_context_bullets(
    data: &CompanyData,
    agent: &AgentRecord,
    bullets: &mut Vec<String>,
) {
    if let Some(stats) = data
        .agent_stats
        .iter()
        .find(|stats| stats.agent_id == agent.id)
    {
        bullets.push(stats.report_line());
        bullets.push(stats.mechanics_line());
        bullets.push(format!("Traits: {}", stats.traits));
        bullets.push(format!("Skills: {}", stats.skills));
    }
    append_agent_social_bullets(data, agent, bullets);
    for escalation in data
        .permission_escalations
        .iter()
        .filter(|escalation| escalation.agent_id == agent.id && escalation.status == "pending")
        .take(2)
    {
        bullets.push(format!(
            "Pending permission request [{}]: {} — {}",
            escalation.category, escalation.requested_policy, escalation.reason
        ));
    }
    if let Some(permission) = data
        .permissions
        .iter()
        .find(|permission| permission.agent_id == agent.id)
    {
        bullets.push(format!(
            "Bcode policy {}: bash {}, read {}, write {}, edit {}, external {}",
            permission.bcode_agent_id,
            permission.bash,
            permission.read,
            permission.write,
            permission.edit,
            permission.external_directory
        ));
    }
}

fn append_agent_social_bullets(data: &CompanyData, agent: &AgentRecord, bullets: &mut Vec<String>) {
    for relationship in data
        .relationships
        .iter()
        .filter(|relationship| relationship.agent_id == agent.id)
        .take(2)
    {
        bullets.push(format!(
            "Relationship with {}: affinity {}, trust {} — {}",
            relationship.other_agent_id,
            relationship.affinity,
            relationship.trust,
            relationship.notes
        ));
    }
    for memory in data
        .memories
        .iter()
        .filter(|memory| memory.agent_id == agent.id)
        .take(2)
    {
        bullets.push(format!(
            "Memory [{} · importance {}]: {}",
            memory.kind, memory.importance, memory.summary
        ));
    }
}

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value)
        .unwrap_or_else(|error| ServiceResponse::error("serialization_failed", error.to_string()))
}

fn parse_service_command<T>(
    request: &ServiceRequest,
    fallback_context: &EventContext,
) -> Result<(T, EventContext), BlimsStateError>
where
    T: for<'de> Deserialize<'de>,
{
    if let Ok(envelope) = request.payload_json::<BlimsCommandEnvelope<T>>() {
        if envelope.command_id.trim().is_empty() {
            return Err(BlimsStateError::InvalidCommandEnvelope(
                "command_id cannot be empty".to_string(),
            ));
        }
        if envelope.actor.trim().is_empty() {
            return Err(BlimsStateError::InvalidCommandEnvelope(
                "actor cannot be empty".to_string(),
            ));
        }
        let event_context = envelope.event_context();
        return Ok((envelope.payload, event_context));
    }
    request
        .payload_json::<T>()
        .map(|payload| (payload, fallback_context.clone()))
        .map_err(|error| BlimsStateError::InvalidRequest(error.to_string()))
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

export_plugin!(BlimsPlugin, MANIFEST);

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(BlimsPlugin, include_str!("../bcode-plugin.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn company_status_starts_not_started() {
        let temp = tempfile_path("status");
        let status = company_status(&temp);

        assert_eq!(status.state, CompanyLifecycleState::NotStarted);
        assert!(!status.daemon_connected);
        assert_eq!(status.state_root, temp.join(DEFAULT_STATE_ROOT));
        assert_eq!(status.lifecycle_status, "not_started");
    }

    #[test]
    fn state_paths_default_to_repo_local() {
        let cwd = PathBuf::from("/tmp/blims-repo");
        let paths = state_paths(&cwd);

        assert_eq!(paths.state_root, cwd.join(DEFAULT_STATE_ROOT));
        assert_eq!(
            paths.database_path,
            cwd.join(DEFAULT_STATE_ROOT).join(DATABASE_FILE_NAME)
        );
    }

    #[test]
    fn world_snapshot_has_starter_agents() {
        let snapshot = fallback_world_snapshot();

        assert_eq!(snapshot.theme, "Cozy Startup Loft");
        assert_eq!(snapshot.agents.len(), 3);
    }

    #[test]
    fn protocol_envelope_round_trips_through_bmux_codec() {
        let request = BlimsProtocolRequest {
            protocol_version: BLIMS_PROTOCOL_VERSION,
            operation: OP_COMPANY_STATUS.to_string(),
            payload: WorkspaceRequest {
                working_directory: PathBuf::from("/tmp/blims-repo"),
                expected_latest_event_id: None,
                correlation_id: None,
                causation_id: None,
            },
        };

        let bytes =
            bmux_codec::to_positional_vec(&request).expect("protocol request should encode");
        let decoded: BlimsProtocolRequest<WorkspaceRequest> =
            bmux_codec::from_positional_bytes(&bytes).expect("protocol request should decode");

        assert_eq!(decoded, request);
    }

    #[test]
    fn typed_event_payload_serializes_with_versionable_shape() {
        let payload = BlimsEventPayload::CompanyLifecycleSet {
            lifecycle_status: "paused".to_string(),
        };
        let json = serde_json::to_string(&payload).expect("event payload should encode");

        assert!(json.contains("company_lifecycle_set"));
        assert!(json.contains("paused"));
    }

    #[test]
    fn create_company_state_seeds_world_row() {
        let temp = tempfile_path("world-row");
        if temp.exists() {
            std::fs::remove_dir_all(&temp).expect("stale temp state should be removable");
        }
        create_company_state(&temp).expect("company state should initialize");

        let world = world_snapshot(&temp).expect("world snapshot should load after create");
        let interactions =
            available_interactions(&temp).expect("interactions should load after create");

        assert_eq!(world.theme, "Cozy Startup Loft");
        assert_eq!(interactions.room_id, "ceo-nook");
        std::fs::remove_dir_all(temp).expect("temp state should be removable");
    }

    #[test]
    fn replay_events_reconstructs_projection_state() {
        let events = projection_test_events();

        let state = replay_events(&events).expect("events should replay");

        assert_eq!(state.lifecycle_status, "running");
        assert_eq!(state.initiatives.len(), 1);
        assert_eq!(state.initiatives[0].status, "paused");
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.tasks[0].assigned_agent_id, "mira");
        assert_eq!(state.rooms.len(), 1);
        assert_eq!(state.agents.len(), 1);
        assert_eq!(state.departments.len(), 1);
        assert_eq!(state.teams.len(), 1);
        assert_eq!(state.agents[0].room_id, "ceo-nook");
        assert_eq!(state.agents[0].status, "reporting");
    }

    fn projection_test_initiative() -> (InitiativeSummary, String) {
        let initiative = InitiativeSummary {
            id: "launch-blims".to_string(),
            title: "Launch Blims".to_string(),
            description: "Make the office come alive".to_string(),
            status: "active".to_string(),
            priority: 1,
        };
        let initiative_id = initiative.id.clone();
        (initiative, initiative_id)
    }

    fn projection_test_task() -> TaskSummary {
        TaskSummary {
            id: "launch-blims-sketch-loop".to_string(),
            initiative_id: "launch-blims".to_string(),
            title: "Sketch the loop".to_string(),
            description: "Describe the event sourced game loop".to_string(),
            status: "proposed".to_string(),
            assigned_agent_id: "mira".to_string(),
            rationale: "Need a playable first loop".to_string(),
            blocker: String::new(),
            priority: 5,
        }
    }

    fn projection_test_room() -> RoomSnapshot {
        RoomSnapshot {
            id: "whiteboard".to_string(),
            name: "Whiteboard".to_string(),
            purpose: "planning".to_string(),
            room_kind: "meeting_room".to_string(),
            productivity_modifier: 6,
            x: 1,
            y: 1,
            symbol: "▦".to_string(),
            color: "bright-white".to_string(),
        }
    }

    fn projection_test_agent() -> AgentSnapshot {
        AgentSnapshot {
            id: "mira".to_string(),
            name: "Mira".to_string(),
            role: "Product Lead".to_string(),
            status: "thinking".to_string(),
            room_id: "whiteboard".to_string(),
        }
    }

    fn projection_test_events() -> Vec<BlimsEventSummary> {
        let (initiative, initiative_id) = projection_test_initiative();
        let task = projection_test_task();
        let room = projection_test_room();
        let agent = projection_test_agent();
        let department_id = "product".to_string();
        vec![
            test_event(
                1,
                "company.lifecycle_set",
                &BlimsEventPayload::CompanyLifecycleSet {
                    lifecycle_status: "running".to_string(),
                },
            ),
            test_event(
                2,
                "initiative.created",
                &BlimsEventPayload::InitiativeCreated { initiative },
            ),
            test_event(
                3,
                "initiative.status_set",
                &BlimsEventPayload::InitiativeStatusSet {
                    initiative_id,
                    status: "paused".to_string(),
                },
            ),
            test_event(4, "task.created", &BlimsEventPayload::TaskCreated { task }),
            test_event(
                5,
                "world.room_created",
                &BlimsEventPayload::WorldRoomCreated { room },
            ),
            test_event(6, "agent.hired", &BlimsEventPayload::AgentHired { agent }),
            test_event(
                7,
                "department.created",
                &BlimsEventPayload::DepartmentCreated {
                    id: department_id.clone(),
                    name: "Product".to_string(),
                    purpose: "strategy".to_string(),
                },
            ),
            test_event(
                8,
                "team.created",
                &BlimsEventPayload::TeamCreated {
                    id: "product-leads".to_string(),
                    department_id,
                    name: "Product Leads".to_string(),
                    purpose: "direction".to_string(),
                },
            ),
            test_event(
                9,
                "agent.moved",
                &BlimsEventPayload::AgentMoved {
                    agent_id: "mira".to_string(),
                    room_id: "ceo-nook".to_string(),
                },
            ),
            test_event(
                10,
                "agent.status_set",
                &BlimsEventPayload::AgentStatusSet {
                    agent_id: "mira".to_string(),
                    status: "reporting".to_string(),
                },
            ),
        ]
    }

    fn test_event(id: i64, kind: &str, payload: &BlimsEventPayload) -> BlimsEventSummary {
        BlimsEventSummary {
            id,
            event_version: 1,
            company_id: "default".to_string(),
            kind: kind.to_string(),
            summary: kind.to_string(),
            payload_json: serde_json::to_string(&payload).expect("payload should encode"),
            correlation_id: "test".to_string(),
            causation_id: "test".to_string(),
        }
    }

    fn tempfile_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("bcode-blims-test-{name}-{}", std::process::id()))
    }
}
