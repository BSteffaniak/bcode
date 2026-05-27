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

/// Artifact inspect operation.
pub const OP_ARTIFACT_INSPECT: &str = "artifact.inspect";

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

/// Morning report operation.
pub const OP_REPORT_MORNING: &str = "report.morning";

/// Event list operation.
pub const OP_EVENT_LIST: &str = "event.list";

/// Event projection rebuild operation.
pub const OP_EVENT_REBUILD_PROJECTIONS: &str = "event.rebuild_projections";

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

        match context.request.operation.as_str() {
            OP_COMPANY_STATUS => service_company_status(&context.request),
            OP_COMPANY_CREATE | OP_COMPANY_LOAD => service_company_create(&context.request),
            OP_COMPANY_PAUSE => service_company_lifecycle(
                &context.request,
                &EventContext::from_request(&context.request),
                "paused",
            ),
            OP_COMPANY_RESUME => service_company_lifecycle(
                &context.request,
                &EventContext::from_request(&context.request),
                "running",
            ),
            OP_COMPANY_SHUTDOWN => service_company_lifecycle(
                &context.request,
                &EventContext::from_request(&context.request),
                "shutdown",
            ),
            OP_AGENT_LIST => service_agent_list(&context.request),
            OP_INITIATIVE_CREATE => service_initiative_create(
                &context.request,
                &EventContext::from_request(&context.request),
            ),
            OP_INITIATIVE_LIST => service_initiative_list(&context.request),
            OP_INITIATIVE_INSPECT => service_initiative_inspect(&context.request),
            OP_INITIATIVE_SET_GUIDANCE => service_initiative_set_guidance(
                &context.request,
                &EventContext::from_request(&context.request),
            ),
            OP_INITIATIVE_PAUSE => service_initiative_status(
                &context.request,
                &EventContext::from_request(&context.request),
                "paused",
            ),
            OP_INITIATIVE_RESUME => service_initiative_status(
                &context.request,
                &EventContext::from_request(&context.request),
                "active",
            ),
            OP_GUIDANCE_SET => service_guidance_set(
                &context.request,
                &EventContext::from_request(&context.request),
            ),
            OP_GUIDANCE_LIST => service_guidance_list(&context.request),
            OP_INITIATIVE_PLAN_PROMPT => service_initiative_plan_prompt(&context.request),
            OP_INITIATIVE_IMPORT_PLAN => service_initiative_import_plan(
                &context.request,
                &EventContext::from_request(&context.request),
            ),
            OP_TASK_LIST => service_task_list(&context.request),
            OP_TASK_INSPECT => service_task_inspect(&context.request),
            OP_TASK_WORK_PROMPT => service_task_work_prompt(&context.request),
            OP_ARTIFACT_LIST => service_artifact_list(&context.request),
            OP_ARTIFACT_INSPECT => service_artifact_inspect(&context.request),
            OP_PROPOSAL_REGISTER => service_proposal_register(
                &context.request,
                &EventContext::from_request(&context.request),
            ),
            OP_PROPOSAL_LIST => service_proposal_list(&context.request),
            OP_PROPOSAL_INSPECT => service_proposal_inspect(&context.request),
            OP_PROPOSAL_MARK_READY => service_proposal_mark_ready(
                &context.request,
                &EventContext::from_request(&context.request),
            ),
            OP_PROPOSAL_APPROVE => service_proposal_status(
                &context.request,
                &EventContext::from_request(&context.request),
                "approved",
            ),
            OP_PROPOSAL_REJECT => service_proposal_status(
                &context.request,
                &EventContext::from_request(&context.request),
                "rejected",
            ),
            OP_PROPOSAL_DEFER => service_proposal_status(
                &context.request,
                &EventContext::from_request(&context.request),
                "deferred",
            ),
            OP_PROPOSAL_RECORD_PATCH => service_proposal_record_patch(
                &context.request,
                &EventContext::from_request(&context.request),
            ),
            OP_AGENT_TALK_PROMPT => service_agent_talk_prompt(&context.request),
            OP_WORLD_SNAPSHOT => service_world_snapshot(&context.request),
            OP_REPORT_MORNING => service_morning_report(&context.request),
            OP_EVENT_LIST => service_event_list(&context.request),
            OP_EVENT_REBUILD_PROJECTIONS => service_event_rebuild_projections(&context.request),
            _ => ServiceResponse::error("unsupported_operation", "unsupported Blims operation"),
        }
    }
}

/// Request carrying the workspace root for repo-local Blims state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Optional correlation id for event-sourced commands.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Optional causation id for event-sourced commands.
    #[serde(default)]
    pub causation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EventContext {
    correlation_id: String,
    causation_id: String,
}

impl EventContext {
    fn from_request(request: &ServiceRequest) -> Self {
        Self {
            correlation_id: format!("service:{}", request.operation),
            causation_id: format!("service:{}", request.operation),
        }
    }

    fn merge_workspace_request(&self, request: &WorkspaceRequest) -> Self {
        Self {
            correlation_id: request
                .correlation_id
                .clone()
                .unwrap_or_else(|| self.correlation_id.clone()),
            causation_id: request
                .causation_id
                .clone()
                .unwrap_or_else(|| self.causation_id.clone()),
        }
    }

    fn merge_optional_ids(
        &self,
        correlation_id: Option<&String>,
        causation_id: Option<&String>,
    ) -> Self {
        Self {
            correlation_id: correlation_id
                .cloned()
                .unwrap_or_else(|| self.correlation_id.clone()),
            causation_id: causation_id
                .cloned()
                .unwrap_or_else(|| self.causation_id.clone()),
        }
    }

    fn merge_initiative_create_request(&self, request: &InitiativeCreateRequest) -> Self {
        self.merge_optional_ids(
            request.correlation_id.as_ref(),
            request.causation_id.as_ref(),
        )
    }

    fn merge_initiative_guidance_request(&self, request: &InitiativeGuidanceRequest) -> Self {
        self.merge_optional_ids(
            request.correlation_id.as_ref(),
            request.causation_id.as_ref(),
        )
    }

    fn merge_guidance_set_request(&self, request: &GuidanceSetRequest) -> Self {
        self.merge_optional_ids(
            request.correlation_id.as_ref(),
            request.causation_id.as_ref(),
        )
    }

    fn merge_initiative_import_plan_request(&self, request: &InitiativeImportPlanRequest) -> Self {
        self.merge_optional_ids(
            request.correlation_id.as_ref(),
            request.causation_id.as_ref(),
        )
    }

    fn merge_proposal_register_request(&self, request: &ProposalRegisterRequest) -> Self {
        self.merge_optional_ids(
            request.correlation_id.as_ref(),
            request.causation_id.as_ref(),
        )
    }

    fn merge_initiative_inspect_request(&self, request: &InitiativeInspectRequest) -> Self {
        self.merge_optional_ids(
            request.correlation_id.as_ref(),
            request.causation_id.as_ref(),
        )
    }

    fn merge_proposal_request(&self, request: &ProposalRequest) -> Self {
        self.merge_optional_ids(
            request.correlation_id.as_ref(),
            request.causation_id.as_ref(),
        )
    }

    fn merge_proposal_record_patch_request(&self, request: &ProposalRecordPatchRequest) -> Self {
        self.merge_optional_ids(
            request.correlation_id.as_ref(),
            request.causation_id.as_ref(),
        )
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
    /// Number of projected rooms.
    pub rooms_projected: usize,
    /// Current projected company lifecycle status.
    pub lifecycle_status: String,
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

/// Request to inspect an artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactInspectRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Artifact id.
    pub artifact_id: String,
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
    /// Prompt text to send to an AI conversation session.
    pub prompt: String,
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
}

/// Snapshot of the currently visible Blims world.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldSnapshot {
    /// Starter office theme name.
    pub theme: String,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompanyRecord {
    name: String,
    mission: String,
    culture: String,
    lifecycle_status: String,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct RoomRecord {
    id: String,
    name: String,
    purpose: String,
}

impl From<RoomSnapshot> for RoomRecord {
    fn from(snapshot: RoomSnapshot) -> Self {
        Self {
            id: snapshot.id,
            name: snapshot.name,
            purpose: snapshot.purpose,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BlimsEventPayload {
    CompanyLifecycleSet {
        lifecycle_status: String,
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
    ProposalStatusSet {
        proposal_id: String,
        status: String,
    },
    ArtifactCreated {
        artifact: ArtifactDetail,
    },
    TaskCreated {
        task: TaskSummary,
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
    WorldRoomCreated {
        room: RoomSnapshot,
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
    /// Required state was missing.
    #[error("Blims company state has not been created at {0}")]
    StateMissing(PathBuf),
    /// Persisted state row was missing an expected column.
    #[error("Blims state row is missing column {0}")]
    MissingColumn(&'static str),
    /// A request field was invalid.
    #[error("invalid Blims request: {0}")]
    InvalidRequest(String),
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
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
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

fn service_initiative_create(
    request: &ServiceRequest,
    event_context: &EventContext,
) -> ServiceResponse {
    let request = match request.payload_json::<InitiativeCreateRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let event_context = event_context.merge_initiative_create_request(&request);
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
    let request = match request.payload_json::<InitiativeGuidanceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let event_context = event_context.merge_initiative_guidance_request(&request);
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
    let request = match request.payload_json::<InitiativeInspectRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let event_context = event_context.merge_initiative_inspect_request(&request);
    match set_initiative_status(&request, &event_context, status) {
        Ok(initiative) => json_response(&initiative),
        Err(error) => ServiceResponse::error("initiative_status_failed", error.to_string()),
    }
}

fn service_guidance_set(request: &ServiceRequest, event_context: &EventContext) -> ServiceResponse {
    let request = match request.payload_json::<GuidanceSetRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let event_context = event_context.merge_guidance_set_request(&request);
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
    let request = match request.payload_json::<InitiativeImportPlanRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let event_context = event_context.merge_initiative_import_plan_request(&request);
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

fn service_proposal_register(
    request: &ServiceRequest,
    event_context: &EventContext,
) -> ServiceResponse {
    let request = match request.payload_json::<ProposalRegisterRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let event_context = event_context.merge_proposal_register_request(&request);
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
    let request = match request.payload_json::<ProposalRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let event_context = event_context.merge_proposal_request(&request);
    match set_proposal_status(&request, &event_context, status) {
        Ok(proposal) => json_response(&proposal),
        Err(error) => ServiceResponse::error("proposal_status_failed", error.to_string()),
    }
}

fn service_proposal_record_patch(
    request: &ServiceRequest,
    event_context: &EventContext,
) -> ServiceResponse {
    let request = match request.payload_json::<ProposalRecordPatchRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let event_context = event_context.merge_proposal_record_patch_request(&request);
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

    match load_company_data(&request.working_directory) {
        Ok(data) => json_response(&WorldSnapshot {
            theme: data.world_theme,
            player_name: "CEO".to_string(),
            rooms: data
                .rooms
                .into_iter()
                .map(|room| RoomSnapshot {
                    id: room.id,
                    name: room.name,
                    purpose: room.purpose,
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
        }),
        Err(_) => json_response(&fallback_world_snapshot()),
    }
}

fn service_morning_report(request: &ServiceRequest) -> ServiceResponse {
    let Ok(request) = request.payload_json::<WorkspaceRequest>() else {
        return json_response(&fallback_morning_report());
    };

    load_company_data(&request.working_directory).map_or_else(
        |_| json_response(&fallback_morning_report()),
        |data| json_response(&morning_report(&data)),
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
    let message = match state {
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
    };

    CompanyStatus {
        state,
        message,
        daemon_connected: false,
        state_root: paths.state_root,
        database_path: paths.database_path,
        lifecycle_status: lifecycle_status.unwrap_or_else(|| "not_started".to_string()),
    }
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
            Ok::<(), BlimsStateError>(())
        })
    })
    .join()
    .map_err(panic_to_blims_error)??;

    Ok(company_status(working_directory))
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
    create_table("agent_contracts")
        .if_not_exists(true)
        .column(text_column("agent_id"))
        .column(text_column("responsibilities"))
        .column(text_column("restrictions"))
        .column(text_column("escalation"))
        .primary_key("agent_id")
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
    create_table("tasks")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("initiative_id"))
        .column(text_column("title"))
        .column(text_column("description"))
        .column(text_column("status"))
        .column(text_column("assigned_agent_id"))
        .column(text_column("rationale"))
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
    seed_company_created_event(database).await?;
    seed_starter_world_events(database).await?;
    seed_departments(database).await?;
    seed_teams(database).await?;
    seed_world(database).await?;
    seed_agents(database).await
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

async fn seed_starter_world_events(
    database: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    for room in fallback_world_snapshot().rooms {
        let payload_json =
            serde_json::to_string(&BlimsEventPayload::WorldRoomCreated { room: room.clone() })
                .expect("starter room event should encode");
        database
            .insert("events")
            .value("company_id", "default")
            .value("kind", "world.room_created")
            .value("summary", format!("Starter room created: {}", room.name))
            .value("payload_json", payload_json)
            .value("correlation_id", "bootstrap")
            .value("causation_id", "bootstrap")
            .value("event_version", 1_i64)
            .execute(database)
            .await?;
    }
    for agent in fallback_world_snapshot().agents {
        let payload_json = serde_json::to_string(&BlimsEventPayload::AgentHired {
            agent: agent.clone(),
        })
        .expect("starter agent event should encode");
        database
            .insert("events")
            .value("company_id", "default")
            .value("kind", "agent.hired")
            .value("summary", format!("Starter agent hired: {}", agent.name))
            .value("payload_json", payload_json)
            .value("correlation_id", "bootstrap")
            .value("causation_id", "bootstrap")
            .value("event_version", 1_i64)
            .execute(database)
            .await?;
    }
    Ok(())
}

async fn seed_departments(database: &dyn Database) -> Result<(), switchy_database::DatabaseError> {
    for (id, name, purpose) in [
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
    ] {
        database
            .exec_raw(&format!(
                "INSERT INTO departments (id, company_id, name, purpose) \
                 SELECT '{id}', 'default', '{name}', '{purpose}' \
                 WHERE NOT EXISTS (SELECT 1 FROM departments WHERE id = '{id}')"
            ))
            .await?;
    }
    Ok(())
}

async fn seed_teams(database: &dyn Database) -> Result<(), switchy_database::DatabaseError> {
    for (id, department_id, name, purpose) in [
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
    ] {
        database
            .exec_raw(&format!(
                "INSERT INTO teams (id, department_id, name, purpose) \
                 SELECT '{id}', '{department_id}', '{name}', '{purpose}' \
                 WHERE NOT EXISTS (SELECT 1 FROM teams WHERE id = '{id}')"
            ))
            .await?;
    }
    Ok(())
}

async fn seed_world(database: &dyn Database) -> Result<(), switchy_database::DatabaseError> {
    database
        .exec_raw(
            "INSERT INTO worlds (id, company_id, theme, player_room_id) \
             SELECT 'default', 'default', 'Cozy Startup Loft', 'ceo-nook' \
             WHERE NOT EXISTS (SELECT 1 FROM worlds WHERE id = 'default')",
        )
        .await?;
    for (id, name, purpose) in [
        (
            "ceo-nook",
            "CEO Nook",
            "company policy and executive guidance",
        ),
        (
            "whiteboard",
            "Whiteboard",
            "initiatives, priorities, and planning",
        ),
        (
            "engineering",
            "Engineering Desks",
            "implementation focus and worktree coding",
        ),
        (
            "creative",
            "Creative Corner",
            "branding, docs, and design ideas",
        ),
        ("review", "Review Wall", "artifact inspection and approval"),
    ] {
        database
            .exec_raw(&format!(
                "INSERT INTO world_rooms (id, world_id, name, purpose) \
                 SELECT '{id}', 'default', '{name}', '{purpose}' \
                 WHERE NOT EXISTS (SELECT 1 FROM world_rooms WHERE id = '{id}')"
            ))
            .await?;
    }
    Ok(())
}

async fn seed_agents(database: &dyn Database) -> Result<(), switchy_database::DatabaseError> {
    for agent in starter_agents() {
        database
            .exec_raw(&format!(
                "INSERT INTO agents (id, name, role, department_id, team_id, status, room_id) \
                 SELECT '{}', '{}', '{}', '{}', '{}', '{}', '{}' \
                 WHERE NOT EXISTS (SELECT 1 FROM agents WHERE id = '{}')",
                agent.id,
                agent.name,
                agent.role,
                agent.department_id,
                agent.team_id,
                agent.status,
                agent.room_id,
                agent.id
            ))
            .await?;
    }
    Ok(())
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

fn fallback_world_snapshot() -> WorldSnapshot {
    WorldSnapshot {
        theme: "Cozy Startup Loft".to_string(),
        player_name: "CEO".to_string(),
        rooms: vec![
            RoomSnapshot {
                id: "whiteboard".to_string(),
                name: "Whiteboard".to_string(),
                purpose: "initiatives, priorities, and planning".to_string(),
            },
            RoomSnapshot {
                id: "engineering".to_string(),
                name: "Engineering Desks".to_string(),
                purpose: "implementation focus and worktree coding".to_string(),
            },
            RoomSnapshot {
                id: "creative".to_string(),
                name: "Creative Corner".to_string(),
                purpose: "branding, docs, and design ideas".to_string(),
            },
        ],
        agents: starter_agents()
            .into_iter()
            .map(AgentRecord::snapshot)
            .collect(),
    }
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
    rooms: Vec<RoomRecord>,
    agents: Vec<AgentRecord>,
    initiatives: Vec<InitiativeSummary>,
    guidance: Vec<GuidanceSummary>,
    proposals: Vec<WorkProposalSummary>,
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
    database
        .insert("events")
        .value("company_id", "default")
        .value("kind", kind)
        .value("summary", summary)
        .value("payload_json", payload_json)
        .value("correlation_id", event_context.correlation_id.clone())
        .value("causation_id", event_context.causation_id.clone())
        .value("event_version", 1_i64)
        .execute(database)
        .await?;
    apply_event_projection(database, payload).await
}

async fn apply_event_projection(
    database: &dyn Database,
    payload: &BlimsEventPayload,
) -> Result<(), BlimsStateError> {
    match payload {
        BlimsEventPayload::CompanyLifecycleSet { lifecycle_status } => {
            database
                .update("companies")
                .value("lifecycle_status", lifecycle_status.clone())
                .filter(Box::new(where_eq("id", "default")))
                .execute(database)
                .await?;
        }
        BlimsEventPayload::InitiativeCreated { initiative } => {
            replace_one_initiative_projection(database, initiative).await?;
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
            replace_one_artifact_projection(database, artifact).await?;
        }
        BlimsEventPayload::TaskCreated { task } => {
            replace_one_task_projection(database, task).await?;
        }
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
        BlimsEventPayload::AgentStatusSet { agent_id, status } => {
            database
                .update("agents")
                .value("status", status.clone())
                .filter(Box::new(where_eq("id", agent_id.clone())))
                .execute(database)
                .await?;
        }
        BlimsEventPayload::WorldRoomCreated { room } => {
            replace_one_world_room_projection(database, &room.clone().into()).await?;
        }
        BlimsEventPayload::InitiativePlanImported { .. } => {}
    }
    Ok(())
}

fn load_company_data(working_directory: &Path) -> Result<CompanyData, BlimsStateError> {
    with_database(working_directory, |database| {
        Box::pin(load_company_data_from_database(database))
    })
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
        Box::pin(async move {
            database
                .select("executive_guidance")
                .columns(&["id", "guidance", "strength", "active"])
                .sort("created_at", SortDirection::Desc)
                .execute(database)
                .await?
                .iter()
                .map(guidance_summary)
                .collect()
        })
    })
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
        Box::pin(async move {
            let events = load_event_stream(database).await?;
            let state = replay_events(&events)?;
            apply_projection_state(database, &state).await?;
            Ok(state.report(events.len()))
        })
    })
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
                .columns(&["id", "initiative_id", "kind", "title"])
                .sort("created_at", SortDirection::Desc)
                .execute(database)
                .await?
                .iter()
                .map(artifact_summary)
                .collect()
        })
    })
}

fn inspect_artifact(request: &ArtifactInspectRequest) -> Result<ArtifactDetail, BlimsStateError> {
    let artifact_id = request.artifact_id.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            database
                .select("artifacts")
                .columns(&["id", "initiative_id", "kind", "title", "payload_json"])
                .filter(Box::new(where_eq("id", artifact_id)))
                .limit(1)
                .execute_first(database)
                .await?
                .as_ref()
                .map(artifact_detail)
                .transpose()?
                .ok_or(BlimsStateError::MissingColumn("artifact"))
        })
    })
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
            append_event(
                database,
                &event_context,
                "proposal.status_set",
                format!("Proposal {proposal_id} status set to {status}."),
                &BlimsEventPayload::ProposalStatusSet {
                    proposal_id,
                    status: status.clone(),
                },
            )
            .await?;
            Ok(WorkProposalSummary { status, ..proposal })
        })
    })
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
            Ok(AgentTalkPrompt {
                agent_id,
                prompt: agent_talk_prompt_text(agent, &data),
            })
        })
    })
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
         Company mission: {}\nCulture: {}\n\nActive CEO guidance:\n{}\n\n\
         Initiative: {}\nInitiative description: {}\n\n\
         Task `{}`: {}\nDescription: {}\nStatus: {}\nPriority: {}\nAssigned agent: {}\nRationale: {}\n\n\
         Produce concrete implementation, review, research, docs, or artifact work as appropriate. Prefer small reviewable changes. Explain what you changed or propose, validation to run, risks, and next steps.",
        data.company.mission,
        data.company.culture,
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
         Room: {}\nStatus: {}\nCompany mission: {}\nCulture: {}\n\n\
         Active CEO guidance:\n{}\n\nActive initiatives:\n{}\n\n\
         Reply in-character as this Blims agent. Be cozy, useful, dynamic, candid, and specific. \
         Tell the CEO what you are thinking about, what you recommend next, and whether anything needs attention.",
        agent.name,
        agent.role,
        agent.room_id,
        agent.status,
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
                payload_json: payload_json.clone(),
            };
            database
                .insert("artifacts")
                .value("id", artifact_id)
                .value("initiative_id", initiative_id.clone())
                .value("kind", "ai_plan")
                .value("title", "AI-generated initiative plan")
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
    let company = database
        .select("companies")
        .columns(&["name", "mission", "culture", "lifecycle_status"])
        .limit(1)
        .execute_first(database)
        .await?
        .ok_or(BlimsStateError::MissingColumn("companies"))?;
    let world = database
        .select("worlds")
        .columns(&["theme"])
        .limit(1)
        .execute_first(database)
        .await?
        .ok_or(BlimsStateError::MissingColumn("worlds"))?;
    let room_rows = database
        .select("world_rooms")
        .columns(&["id", "name", "purpose"])
        .sort("id", SortDirection::Asc)
        .execute(database)
        .await?;
    let agent_rows = database
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
        .await?;
    let initiatives = database
        .select("initiatives")
        .columns(&["id", "title", "description", "status", "priority"])
        .sort("priority", SortDirection::Asc)
        .execute(database)
        .await?
        .iter()
        .map(initiative_summary)
        .collect::<Result<Vec<_>, _>>()?;
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
    let proposals = database
        .select("work_proposals")
        .columns(&proposal_columns())
        .sort("updated_at", SortDirection::Desc)
        .execute(database)
        .await?
        .iter()
        .map(proposal_summary)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(CompanyData {
        company: CompanyRecord {
            name: required_text(&company, "name")?,
            mission: required_text(&company, "mission")?,
            culture: required_text(&company, "culture")?,
            lifecycle_status: required_text(&company, "lifecycle_status")?,
        },
        world_theme: required_text(&world, "theme")?,
        rooms: room_rows
            .iter()
            .map(room_record)
            .collect::<Result<Vec<_>, _>>()?,
        agents: agent_rows
            .iter()
            .map(agent_record)
            .collect::<Result<Vec<_>, _>>()?,
        initiatives,
        guidance,
        proposals,
    })
}

fn room_record(row: &Row) -> Result<RoomRecord, BlimsStateError> {
    Ok(RoomRecord {
        id: required_text(row, "id")?,
        name: required_text(row, "name")?,
        purpose: required_text(row, "purpose")?,
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

fn initiative_summary(row: &Row) -> Result<InitiativeSummary, BlimsStateError> {
    Ok(InitiativeSummary {
        id: required_text(row, "id")?,
        title: required_text(row, "title")?,
        description: required_text(row, "description")?,
        status: required_text(row, "status")?,
        priority: required_i64(row, "priority")?,
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
    guidance: Vec<GuidanceSummary>,
    artifacts: Vec<ArtifactDetail>,
    proposals: Vec<WorkProposalSummary>,
    tasks: Vec<TaskSummary>,
    rooms: Vec<RoomRecord>,
    agents: Vec<AgentRecord>,
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
        BlimsEventPayload::ArtifactCreated { artifact } => {
            upsert_by_id(&mut state.artifacts, artifact, |artifact| &artifact.id);
        }
        BlimsEventPayload::TaskCreated { task } => {
            upsert_by_id(&mut state.tasks, task, |task| &task.id);
        }
        BlimsEventPayload::AgentHired { agent } => {
            upsert_by_id(&mut state.agents, agent.into(), |agent| &agent.id);
        }
        BlimsEventPayload::AgentMoved { agent_id, room_id } => {
            if let Some(agent) = state.agents.iter_mut().find(|agent| agent.id == agent_id) {
                agent.room_id = room_id;
            }
        }
        BlimsEventPayload::AgentStatusSet { agent_id, status } => {
            if let Some(agent) = state.agents.iter_mut().find(|agent| agent.id == agent_id) {
                agent.status = status;
            }
        }
        BlimsEventPayload::WorldRoomCreated { room } => {
            upsert_by_id(&mut state.rooms, room.into(), |room| &room.id);
        }
        BlimsEventPayload::InitiativePlanImported { .. } => {}
    }
    Ok(())
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
    replace_guidance_projections(database, &state.guidance).await?;
    replace_artifact_projections(database, &state.artifacts).await?;
    replace_proposal_projections(database, &state.proposals).await?;
    replace_task_projections(database, &state.tasks).await?;
    replace_world_room_projections(database, &state.rooms).await?;
    replace_agent_projections(database, &state.agents).await
}

async fn replace_one_initiative_projection(
    database: &dyn Database,
    initiative: &InitiativeSummary,
) -> Result<(), BlimsStateError> {
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
    database.exec_raw("DELETE FROM initiatives").await?;
    for initiative in initiatives {
        replace_one_initiative_projection(database, initiative).await?;
    }
    Ok(())
}

async fn replace_one_guidance_projection(
    database: &dyn Database,
    item: &GuidanceSummary,
) -> Result<(), BlimsStateError> {
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
    database.exec_raw("DELETE FROM executive_guidance").await?;
    for item in guidance {
        replace_one_guidance_projection(database, item).await?;
    }
    Ok(())
}

async fn replace_one_artifact_projection(
    database: &dyn Database,
    artifact: &ArtifactDetail,
) -> Result<(), BlimsStateError> {
    database
        .insert("artifacts")
        .value("id", artifact.id.clone())
        .value("initiative_id", artifact.initiative_id.clone())
        .value("kind", artifact.kind.clone())
        .value("title", artifact.title.clone())
        .value("payload_json", artifact.payload_json.clone())
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_artifact_projections(
    database: &dyn Database,
    artifacts: &[ArtifactDetail],
) -> Result<(), BlimsStateError> {
    database.exec_raw("DELETE FROM artifacts").await?;
    for artifact in artifacts {
        replace_one_artifact_projection(database, artifact).await?;
    }
    Ok(())
}

async fn replace_one_proposal_projection(
    database: &dyn Database,
    proposal: &WorkProposalSummary,
) -> Result<(), BlimsStateError> {
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
    database.exec_raw("DELETE FROM work_proposals").await?;
    for proposal in proposals {
        replace_one_proposal_projection(database, proposal).await?;
    }
    Ok(())
}

async fn replace_one_task_projection(
    database: &dyn Database,
    task: &TaskSummary,
) -> Result<(), BlimsStateError> {
    database
        .insert("tasks")
        .value("id", task.id.clone())
        .value("initiative_id", task.initiative_id.clone())
        .value("title", task.title.clone())
        .value("description", task.description.clone())
        .value("status", task.status.clone())
        .value("assigned_agent_id", task.assigned_agent_id.clone())
        .value("rationale", task.rationale.clone())
        .value("priority", task.priority)
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_task_projections(
    database: &dyn Database,
    tasks: &[TaskSummary],
) -> Result<(), BlimsStateError> {
    database.exec_raw("DELETE FROM tasks").await?;
    for task in tasks {
        replace_one_task_projection(database, task).await?;
    }
    Ok(())
}

async fn replace_one_world_room_projection(
    database: &dyn Database,
    room: &RoomRecord,
) -> Result<(), BlimsStateError> {
    database
        .insert("world_rooms")
        .value("id", room.id.clone())
        .value("world_id", "default")
        .value("name", room.name.clone())
        .value("purpose", room.purpose.clone())
        .execute(database)
        .await?;
    Ok(())
}

async fn replace_world_room_projections(
    database: &dyn Database,
    rooms: &[RoomRecord],
) -> Result<(), BlimsStateError> {
    database.exec_raw("DELETE FROM world_rooms").await?;
    for room in rooms {
        replace_one_world_room_projection(database, room).await?;
    }
    Ok(())
}

async fn replace_one_agent_projection(
    database: &dyn Database,
    agent: &AgentRecord,
) -> Result<(), BlimsStateError> {
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
    database.exec_raw("DELETE FROM agents").await?;
    for agent in agents {
        replace_one_agent_projection(database, agent).await?;
    }
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
        priority: required_i64(row, "priority")?,
    })
}

fn artifact_summary(row: &Row) -> Result<ArtifactSummary, BlimsStateError> {
    Ok(ArtifactSummary {
        id: required_text(row, "id")?,
        initiative_id: required_text(row, "initiative_id")?,
        kind: required_text(row, "kind")?,
        title: required_text(row, "title")?,
    })
}

fn artifact_detail(row: &Row) -> Result<ArtifactDetail, BlimsStateError> {
    Ok(ArtifactDetail {
        id: required_text(row, "id")?,
        initiative_id: required_text(row, "initiative_id")?,
        kind: required_text(row, "kind")?,
        title: required_text(row, "title")?,
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

    MorningReport {
        title: format!("{} morning report", data.company.name),
        bullets,
    }
}

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value)
        .unwrap_or_else(|error| ServiceResponse::error("serialization_failed", error.to_string()))
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
                correlation_id: None,
                causation_id: None,
            },
        };

        let bytes = bmux_codec::to_vec(&request).expect("protocol request should encode");
        let decoded: BlimsProtocolRequest<WorkspaceRequest> =
            bmux_codec::from_bytes(&bytes).expect("protocol request should decode");

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
    fn replay_events_reconstructs_projection_state() {
        let initiative = InitiativeSummary {
            id: "launch-blims".to_string(),
            title: "Launch Blims".to_string(),
            description: "Make the office come alive".to_string(),
            status: "active".to_string(),
            priority: 1,
        };
        let initiative_id = initiative.id.clone();
        let task = TaskSummary {
            id: "launch-blims-sketch-loop".to_string(),
            initiative_id: "launch-blims".to_string(),
            title: "Sketch the loop".to_string(),
            description: "Describe the event sourced game loop".to_string(),
            status: "proposed".to_string(),
            assigned_agent_id: "mira".to_string(),
            rationale: "Need a playable first loop".to_string(),
            priority: 5,
        };
        let room = RoomSnapshot {
            id: "whiteboard".to_string(),
            name: "Whiteboard".to_string(),
            purpose: "planning".to_string(),
        };
        let agent = AgentSnapshot {
            id: "mira".to_string(),
            name: "Mira".to_string(),
            role: "Product Lead".to_string(),
            status: "thinking".to_string(),
            room_id: "whiteboard".to_string(),
        };
        let events = vec![
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
                "agent.moved",
                &BlimsEventPayload::AgentMoved {
                    agent_id: "mira".to_string(),
                    room_id: "ceo-nook".to_string(),
                },
            ),
            test_event(
                8,
                "agent.status_set",
                &BlimsEventPayload::AgentStatusSet {
                    agent_id: "mira".to_string(),
                    status: "reporting".to_string(),
                },
            ),
        ];

        let state = replay_events(&events).expect("events should replay");

        assert_eq!(state.lifecycle_status, "running");
        assert_eq!(state.initiatives.len(), 1);
        assert_eq!(state.initiatives[0].status, "paused");
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.tasks[0].assigned_agent_id, "mira");
        assert_eq!(state.rooms.len(), 1);
        assert_eq!(state.agents.len(), 1);
        assert_eq!(state.agents[0].room_id, "ceo-nook");
        assert_eq!(state.agents[0].status, "reporting");
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
