//! Native Tokio-backed TUI surface host APIs for plugins.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use crate::interaction::{InteractionInput, InteractionOutput, PluginInteraction};
use bcode_session_models::{ProjectionWindowRequest, SessionId};
use bcode_session_view_models::{ReasoningPresentationPolicy, SessionViewSnapshot};
use bmux_keyboard::KeyStroke;
use bmux_text_edit::{TextEditCommand, TextMotion};
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Style};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::sync::mpsc;

/// Boxed error returned by native TUI plugin surface factories.
pub type PluginTuiError = Box<dyn Error + Send + Sync>;

/// Boxed native TUI plugin surface.
pub type BoxedPluginTuiSurface = Box<dyn PluginTuiSurface>;

/// Boxed native TUI plugin surface future.
pub type PluginTuiSurfaceFuture =
    Pin<Box<dyn Future<Output = Result<BoxedPluginTuiSurface, PluginTuiError>> + Send + 'static>>;

/// Boxed asynchronous task accepted by a plugin host.
pub type PluginTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Errors returned by TUI host capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginTuiHostError {
    /// This host does not support the requested capability.
    Unsupported(String),
    /// The plugin is not permitted to use the requested capability.
    PermissionDenied(String),
    /// The plugin requested an invalid host operation.
    InvalidRequest(String),
    /// The host failed while preparing the operation.
    Internal(String),
}

impl fmt::Display for PluginTuiHostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(message)
            | Self::PermissionDenied(message)
            | Self::InvalidRequest(message)
            | Self::Internal(message) => formatter.write_str(message),
        }
    }
}

impl Error for PluginTuiHostError {}

/// Renderer-neutral durable workflow binding supplied by the owning plugin.
///
/// The fields are deliberately bounded and generic. Domain data belongs in the typed workflow
/// input and compiled definition rather than in an opaque metadata payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginWorkflowBinding {
    /// Plugin that owns product presentation and lifecycle vocabulary.
    pub owner_plugin_id: String,
    /// Stable product-facing workflow kind within the owner plugin.
    pub workflow_kind: String,
    /// Stable scope key used for bounded associated-run lookup, such as a session identity.
    pub scope_key: String,
    /// Optional compact user-facing label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_label: Option<String>,
    /// Whether at most one non-terminal run may exist for this owner/kind/scope.
    #[serde(default)]
    pub single_active: bool,
}

impl PluginWorkflowBinding {
    /// Return the exact generic lookup key carried across plugin host boundaries.
    #[must_use]
    pub fn lookup(&self) -> PluginWorkflowLookup {
        PluginWorkflowLookup {
            owner_plugin_id: self.owner_plugin_id.clone(),
            workflow_kind: self.workflow_kind.clone(),
            scope_key: self.scope_key.clone(),
        }
    }
}

/// Exact generic workflow association key available to plugin surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginWorkflowLookup {
    pub owner_plugin_id: String,
    pub workflow_kind: String,
    pub scope_key: String,
}

/// Durable workflow status exposed without coupling the plugin SDK to persistence internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginWorkflowStatus {
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
    RepairRequired,
}

/// Bounded associated workflow summary exposed to plugin-owned surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginWorkflowSummary {
    pub run_id: String,
    pub definition_id: String,
    pub definition_version: u32,
    pub status: PluginWorkflowStatus,
    /// Whether durable cancellation has been requested but the run is not yet terminal.
    #[serde(default)]
    pub cancellation_requested: bool,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Bounded associated workflow inspection exposed to plugin-owned surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginWorkflowInspection {
    pub run: PluginWorkflowSummary,
    pub definition: bcode_workflow::WorkflowDefinition,
    pub waits: Vec<serde_json::Value>,
    pub attempts: Vec<serde_json::Value>,
    pub events: Vec<serde_json::Value>,
    pub grants: Vec<serde_json::Value>,
    pub resource_leases: Vec<serde_json::Value>,
    pub outputs: Vec<serde_json::Value>,
    pub child_session_ids: Vec<SessionId>,
}

/// Lifecycle transition available through generic plugin host workflow routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginWorkflowControlAction {
    Pause,
    Resume,
    Cancel,
}

/// Result of applying one associated workflow lifecycle transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginWorkflowControlResult {
    pub run: Option<PluginWorkflowSummary>,
    pub changed: bool,
}

/// Async associated workflow lookup result.
pub type PluginWorkflowLookupFuture = Pin<
    Box<
        dyn Future<Output = Result<Option<PluginWorkflowSummary>, PluginTuiHostError>>
            + Send
            + 'static,
    >,
>;

/// Async associated workflow inspection result.
pub type PluginWorkflowInspectionFuture = Pin<
    Box<
        dyn Future<Output = Result<Option<PluginWorkflowInspection>, PluginTuiHostError>>
            + Send
            + 'static,
    >,
>;

/// Async associated workflow lifecycle result.
pub type PluginWorkflowControlFuture = Pin<
    Box<
        dyn Future<Output = Result<PluginWorkflowControlResult, PluginTuiHostError>>
            + Send
            + 'static,
    >,
>;

/// Async portable workflow-authoring catalog result.
pub type PluginWorkflowAuthoringCatalogFuture = Pin<
    Box<
        dyn Future<
                Output = Result<
                    bcode_workflow::WorkflowAuthoringCatalogSnapshot,
                    PluginTuiHostError,
                >,
            > + Send
            + 'static,
    >,
>;

/// Portable authored draft used by plugin-owned authoring surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginWorkflowAuthoringDraft {
    pub workflow_id: String,
    pub draft_id: String,
    pub base_revision: Option<u64>,
    pub generation: u64,
    pub document: bcode_workflow::WorkflowAuthoringDocument,
    pub producer: bcode_workflow::WorkflowProducerProvenance,
}

/// Result of applying one optimistic semantic draft edit batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginWorkflowAuthoringEditResult {
    Updated(Box<PluginWorkflowAuthoringDraft>),
    Conflict {
        expected_generation: u64,
        current_generation: u64,
    },
    Rejected {
        diagnostics: Vec<bcode_workflow::WorkflowValidationDiagnostic>,
    },
}

/// Result of publishing one exact draft generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginWorkflowAuthoringPublishResult {
    Published {
        revision: u64,
        activated: bool,
    },
    Conflict {
        expected_generation: u64,
        current_generation: u64,
    },
}

/// Async workflow-authoring draft result.
pub type PluginWorkflowAuthoringDraftFuture = Pin<
    Box<
        dyn Future<Output = Result<Option<PluginWorkflowAuthoringDraft>, PluginTuiHostError>>
            + Send
            + 'static,
    >,
>;

/// Portable immutable revision document used for semantic authoring review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginWorkflowAuthoringRevision {
    pub workflow_id: String,
    pub revision: u64,
    pub document: bcode_workflow::WorkflowAuthoringDocument,
}

/// Bounded renderer-neutral request for one tool-free structured model generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginStructuredGenerationRequest {
    pub session_name: String,
    pub system_prompt: String,
    pub prompt: String,
    pub output_name: String,
    pub output_schema: serde_json::Value,
    pub timeout_ms: u64,
}

/// Async structured model-generation result.
pub type PluginStructuredGenerationFuture =
    Pin<Box<dyn Future<Output = Result<serde_json::Value, PluginTuiHostError>> + Send + 'static>>;

/// Result of explicitly accepting one generated workflow candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginWorkflowGeneratedCandidateAcceptance {
    Created(Box<PluginWorkflowAuthoringDraft>),
    Updated(Box<PluginWorkflowAuthoringDraft>),
    Conflict {
        expected_generation: u64,
        current_generation: u64,
    },
    Rejected {
        diagnostics: Vec<bcode_workflow::WorkflowValidationDiagnostic>,
    },
}

/// Async generated-candidate acceptance result.
pub type PluginWorkflowGeneratedCandidateAcceptanceFuture = Pin<
    Box<
        dyn Future<Output = Result<PluginWorkflowGeneratedCandidateAcceptance, PluginTuiHostError>>
            + Send
            + 'static,
    >,
>;

/// Exact existing draft target for generated candidate replacement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginWorkflowGeneratedCandidateTarget {
    pub workflow_id: String,
    pub draft_id: String,
    pub expected_generation: u64,
}

/// Renderer-neutral generated candidate awaiting explicit user acceptance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginWorkflowGeneratedCandidate {
    pub document: bcode_workflow::WorkflowAuthoringDocument,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<PluginWorkflowGeneratedCandidateTarget>,
    pub draft_id: String,
    pub repair_attempts: u32,
}

/// Async workflow-template instantiation result.
pub type PluginWorkflowTemplateInstantiationFuture = Pin<
    Box<
        dyn Future<Output = Result<PluginWorkflowAuthoringDraft, PluginTuiHostError>>
            + Send
            + 'static,
    >,
>;

/// Renderer-neutral request to instantiate one exact maintainable template as a mutable draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginWorkflowTemplateInstantiationRequest {
    pub owner_plugin_id: String,
    pub template_id: String,
    pub template_version: u32,
    pub workflow_id: String,
    pub draft_id: String,
}

/// Async workflow-authoring revision result.
pub type PluginWorkflowAuthoringRevisionFuture = Pin<
    Box<
        dyn Future<Output = Result<Option<PluginWorkflowAuthoringRevision>, PluginTuiHostError>>
            + Send
            + 'static,
    >,
>;

/// Async workflow-authoring edit result.
pub type PluginWorkflowAuthoringEditFuture = Pin<
    Box<
        dyn Future<Output = Result<PluginWorkflowAuthoringEditResult, PluginTuiHostError>>
            + Send
            + 'static,
    >,
>;

/// Async workflow-authoring validation result.
pub type PluginWorkflowAuthoringValidationFuture = Pin<
    Box<
        dyn Future<Output = Result<bcode_workflow::WorkflowValidationReport, PluginTuiHostError>>
            + Send
            + 'static,
    >,
>;

/// Async workflow-authoring preview result.
pub type PluginWorkflowAuthoringPreviewFuture = Pin<
    Box<
        dyn Future<Output = Result<bcode_workflow::WorkflowCompilationPreview, PluginTuiHostError>>
            + Send
            + 'static,
    >,
>;

/// Async workflow-authoring publication result.
pub type PluginWorkflowAuthoringPublishFuture = Pin<
    Box<
        dyn Future<Output = Result<PluginWorkflowAuthoringPublishResult, PluginTuiHostError>>
            + Send
            + 'static,
    >,
>;

/// Renderer-neutral request to start one exact published workflow-package export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginWorkflowPackageExportStartRequest {
    pub package_export: bcode_workflow::WorkflowPackageExportIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub parent_session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_snapshot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
}

/// Async workflow-authoring start result.
pub type PluginWorkflowAuthoringStartFuture = Pin<
    Box<
        dyn Future<Output = Result<PluginWorkflowStartResponse, PluginTuiHostError>>
            + Send
            + 'static,
    >,
>;

/// Renderer-neutral request for a plugin surface to start one durable workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginWorkflowStartRequest {
    /// Logical and exact identities derived from the validated compiled definition.
    pub identity: bcode_workflow::WorkflowDefinitionIdentity,
    pub definition: bcode_workflow::WorkflowDefinition,
    /// Optional stable run identity chosen by the plugin for crash-safe start retries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub parent_session_id: SessionId,
    pub input: serde_json::Value,
    pub binding: PluginWorkflowBinding,
}

impl PluginWorkflowStartRequest {
    /// Build a renderer-neutral start request from a typed durable specification and typed input.
    ///
    /// # Errors
    ///
    /// Returns an error when the input cannot be serialized or fails its generated schema.
    pub fn typed<I>(
        spec: &bcode_workflow::WorkflowSpec<I>,
        input: &I,
        parent_session_id: SessionId,
        binding: PluginWorkflowBinding,
        run_id: Option<String>,
    ) -> Result<Self, bcode_workflow::WorkflowError>
    where
        I: Serialize + DeserializeOwned + schemars::JsonSchema + Send + 'static,
    {
        Ok(Self {
            identity: spec.identity().clone(),
            definition: spec.definition().clone(),
            run_id,
            parent_session_id,
            input: spec.serialize_input(input)?,
            binding,
        })
    }
}

/// Async workflow-start result returned by a native TUI host.
pub type PluginWorkflowStartFuture = Pin<
    Box<
        dyn Future<Output = Result<PluginWorkflowStartResponse, PluginTuiHostError>>
            + Send
            + 'static,
    >,
>;

/// Renderer-neutral durable workflow-start result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginWorkflowStartResponse {
    pub run_id: String,
    pub runtime_work_id: String,
}

/// Request to observe renderer-neutral semantic state for one explicit Bcode session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSessionViewSubscriptionRequest {
    /// Session to observe.
    pub session_id: SessionId,
    /// Bounded projection window used for initial attachment and authoritative resynchronization.
    pub projection: ProjectionWindowRequest,
    /// Renderer-local reasoning presentation policy. This never changes provider requests or
    /// durable session history.
    pub reasoning_policy: ReasoningPresentationPolicy,
    /// Requested snapshot channel buffer size. Hosts may clamp this value.
    pub buffer: usize,
}

/// Semantic session-view updates delivered to plugin-owned TUI surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSessionViewUpdate {
    /// Complete authoritative state. Consumers replace prior state by session id.
    Snapshot(Box<SessionViewSnapshot>),
    /// The observer stopped because attachment or daemon connectivity failed.
    Disconnected {
        /// Human-readable failure message.
        message: String,
    },
}

/// Active subscription to renderer-neutral semantic session state.
#[derive(Debug)]
pub struct PluginSessionViewSubscription {
    /// Receiver for complete authoritative snapshots and terminal observer failures.
    pub receiver: mpsc::Receiver<PluginSessionViewUpdate>,
}

/// Host services available to native TUI plugin surfaces.
pub trait PluginTuiHost: Send + Sync {
    /// Spawn an async task on Bcode's native Tokio runtime.
    fn spawn(&self, task: PluginTask);

    /// Spawn blocking work on Bcode's Tokio blocking pool.
    fn spawn_blocking(&self, task: Box<dyn FnOnce() + Send + 'static>);

    /// Request another terminal redraw.
    fn request_redraw(&self);

    /// Copy plain text to the host clipboard.
    ///
    /// # Errors
    ///
    /// Returns an error when the host does not support clipboard writes or the platform clipboard
    /// cannot be opened or updated.
    fn copy_text(&self, _text: String) -> Result<(), PluginTuiHostError> {
        Err(PluginTuiHostError::Unsupported(
            "clipboard writes are not available from this host".to_string(),
        ))
    }

    /// Resolve a key stroke to one host-configured text edit command.
    fn text_edit_command(&self, _stroke: KeyStroke) -> Option<TextEditCommand> {
        None
    }

    /// Resolve a key stroke to one host-configured selection motion.
    fn text_selection_motion(&self, _stroke: KeyStroke) -> Option<TextMotion> {
        None
    }

    /// Return whether a key stroke is configured to submit composer-like input.
    fn text_submit(&self, _stroke: KeyStroke) -> bool {
        false
    }

    /// Start one durable workflow through the host's generic workflow service.
    ///
    /// # Errors
    ///
    /// Returns an error when the host does not expose workflow start, permission is denied, or the
    /// daemon rejects registration/run admission.
    fn start_workflow(&self, _request: PluginWorkflowStartRequest) -> PluginWorkflowStartFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "workflow start is not available from this host".to_string(),
            ))
        })
    }

    /// Return the newest workflow associated with one exact generic binding.
    fn associated_workflow(&self, _lookup: PluginWorkflowLookup) -> PluginWorkflowLookupFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "workflow lookup is not available from this host".to_string(),
            ))
        })
    }

    /// Return a bounded aggregate inspection for the newest associated workflow.
    fn inspect_associated_workflow(
        &self,
        _lookup: PluginWorkflowLookup,
        _limit: usize,
    ) -> PluginWorkflowInspectionFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "workflow inspection is not available from this host".to_string(),
            ))
        })
    }

    /// Apply one lifecycle transition to the newest associated workflow.
    fn control_associated_workflow(
        &self,
        _lookup: PluginWorkflowLookup,
        _action: PluginWorkflowControlAction,
    ) -> PluginWorkflowControlFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "workflow lifecycle control is not available from this host".to_string(),
            ))
        })
    }

    /// Load the current bounded portable workflow-authoring catalog.
    ///
    /// The returned catalog contains only public workflow contracts. It does not expose daemon,
    /// plugin-runtime, or persistence implementation state.
    fn workflow_authoring_catalog(&self) -> PluginWorkflowAuthoringCatalogFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "workflow authoring is not available from this host".to_string(),
            ))
        })
    }

    /// Run one bounded, tool-free, structured model turn through the normal session boundary.
    fn generate_structured_output(
        &self,
        _request: PluginStructuredGenerationRequest,
    ) -> PluginStructuredGenerationFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "structured generation is not available from this host".to_string(),
            ))
        })
    }

    /// Persist one explicitly accepted generated candidate as a new or existing mutable draft.
    ///
    /// This operation cannot publish, activate, start, or grant permissions. Existing draft
    /// replacement remains guarded by exact optimistic generation.
    fn accept_generated_workflow_candidate(
        &self,
        _candidate: PluginWorkflowGeneratedCandidate,
    ) -> PluginWorkflowGeneratedCandidateAcceptanceFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "generated workflow candidate acceptance is not available from this host"
                    .to_string(),
            ))
        })
    }

    /// Instantiate one exact maintainable plugin template as normal mutable authored state.
    fn instantiate_workflow_template(
        &self,
        _request: PluginWorkflowTemplateInstantiationRequest,
    ) -> PluginWorkflowTemplateInstantiationFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "workflow template instantiation is not available from this host".to_string(),
            ))
        })
    }

    /// Load one exact mutable authored-workflow draft.
    fn workflow_authoring_draft(
        &self,
        _workflow_id: String,
        _draft_id: String,
    ) -> PluginWorkflowAuthoringDraftFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "workflow authoring is not available from this host".to_string(),
            ))
        })
    }

    /// Load one exact immutable authored-workflow revision for semantic review.
    fn workflow_authoring_revision(
        &self,
        _workflow_id: String,
        _revision: u64,
    ) -> PluginWorkflowAuthoringRevisionFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "workflow authoring is not available from this host".to_string(),
            ))
        })
    }

    /// Apply one generation-checked renderer-neutral semantic edit batch.
    fn apply_workflow_authoring_edits(
        &self,
        _workflow_id: String,
        _draft_id: String,
        _batch: bcode_workflow::WorkflowAuthoringEditBatch,
        _producer: bcode_workflow::WorkflowProducerProvenance,
    ) -> PluginWorkflowAuthoringEditFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "workflow authoring is not available from this host".to_string(),
            ))
        })
    }

    /// Validate one portable authoring document without mutation.
    fn validate_workflow_authoring(
        &self,
        _document: bcode_workflow::WorkflowAuthoringDocument,
    ) -> PluginWorkflowAuthoringValidationFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "workflow authoring is not available from this host".to_string(),
            ))
        })
    }

    /// Compile and preview one portable authoring document without mutation.
    fn preview_workflow_authoring(
        &self,
        _document: bcode_workflow::WorkflowAuthoringDocument,
        _configuration: Option<serde_json::Value>,
    ) -> PluginWorkflowAuthoringPreviewFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "workflow authoring is not available from this host".to_string(),
            ))
        })
    }

    /// Publish one exact draft generation as an immutable authored revision.
    fn publish_workflow_authoring_draft(
        &self,
        _workflow_id: String,
        _draft_id: String,
        _expected_generation: u64,
        _activate: bool,
    ) -> PluginWorkflowAuthoringPublishFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "workflow authoring is not available from this host".to_string(),
            ))
        })
    }

    /// Explicitly start one immutable authored-workflow revision.
    fn start_authored_workflow_revision(
        &self,
        _workflow_id: String,
        _revision: u64,
        _parent_session_id: SessionId,
        _workspace_snapshot: Option<String>,
        _configuration: Option<serde_json::Value>,
    ) -> PluginWorkflowAuthoringStartFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "workflow authoring is not available from this host".to_string(),
            ))
        })
    }

    /// Start one exact immutable published package export through the portable application contract.
    fn start_workflow_package_export(
        &self,
        _request: PluginWorkflowPackageExportStartRequest,
    ) -> PluginWorkflowAuthoringStartFuture {
        Box::pin(async {
            Err(PluginTuiHostError::Unsupported(
                "workflow package export start is not available from this host".to_string(),
            ))
        })
    }

    /// Observe renderer-neutral semantic state for one explicit Bcode session.
    ///
    /// The host owns bounded attachment, event projection, reconnect, and resynchronization. Every
    /// update is a complete `SessionViewSnapshot`, so consumers replace prior state instead of
    /// interpreting raw durable or live events.
    ///
    /// # Errors
    ///
    /// Returns an error when the host does not support semantic session observation, permission is
    /// denied, or the request is invalid.
    fn subscribe_session_view(
        &self,
        _request: PluginSessionViewSubscriptionRequest,
    ) -> Result<PluginSessionViewSubscription, PluginTuiHostError> {
        Err(PluginTuiHostError::Unsupported(
            "semantic session observation is not available from this host".to_string(),
        ))
    }
}

/// Text returned by a plugin-owned TUI surface.
///
/// Legacy string values deserialize as plain text. Rich text is opt-in through the formatted
/// object shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginTuiText {
    /// Backward-compatible untyped text, interpreted literally.
    Plain(String),
    /// Explicitly formatted text.
    Formatted {
        /// Text content.
        text: String,
        /// Renderer-neutral format.
        #[serde(default)]
        format: bcode_command::CommandTextFormat,
    },
}

impl PluginTuiText {
    /// Split this value into text and its explicit renderer-neutral format.
    #[must_use]
    pub fn into_parts(self) -> (String, bcode_command::CommandTextFormat) {
        match self {
            Self::Plain(text) => (text, bcode_command::CommandTextFormat::PlainText),
            Self::Formatted { text, format } => (text, format),
        }
    }
}

/// Typed outcome returned when a plugin-owned TUI surface closes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginTuiSurfaceOutcome {
    /// Optional host status text.
    #[serde(default)]
    pub status: Option<String>,
    /// Optional transcript text. Legacy strings remain plain text.
    #[serde(default)]
    pub append_text: Option<PluginTuiText>,
    /// Optional plugin command to invoke after this surface closes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invoke_command: Option<bcode_command::CommandAction>,
    /// Arguments supplied to `invoke_command`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub command_args: std::collections::BTreeMap<String, String>,
    /// Optional working directory to attach to the active session.
    #[serde(default)]
    pub set_session_working_directory: Option<String>,
}

/// Renderer-neutral update delivered to a plugin-owned surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginTuiSurfaceUpdate {
    /// Report that the catalog is being fetched. Stale content may remain visible.
    WorkflowCatalogLoading { stale: bool },
    /// Replace workflow presentation from one authoritative bounded projection.
    WorkflowRun(Box<bcode_workflow_view_models::WorkflowRunView>),
    /// Replace workflow catalog presentation from one authoritative bounded projection.
    WorkflowCatalog(bcode_workflow_view_models::WorkflowCatalogView),
    /// Append one exact continuation page to the current catalog query.
    WorkflowCatalogPage(bcode_workflow_view_models::WorkflowCatalogView),
    /// Report a bounded catalog request failure without implying observer disconnection.
    WorkflowCatalogError { message: String },
    /// Report that selected-run detail is being fetched.
    WorkflowRunLoading { run_id: String },
    /// Report a selected-run detail failure without implying observer disconnection.
    WorkflowRunError { run_id: String, message: String },
    /// Select one exact run whose bounded detail should be loaded by the host.
    SelectWorkflowRun { run_id: String },
    /// The observer requires bounded snapshot replacement.
    ResyncRequired,
    /// Live observation stopped or entered a degraded state.
    Disconnected { message: String },
}

/// Sender retained by a host observer for one plugin surface.
pub type PluginTuiSurfaceUpdateSender = mpsc::Sender<PluginTuiSurfaceUpdate>;

/// Receiver owned by one plugin surface.
pub type PluginTuiSurfaceUpdateReceiver = mpsc::Receiver<PluginTuiSurfaceUpdate>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginTuiAction {
    /// No host action is needed.
    None,
    /// Redraw the terminal.
    Redraw,
    /// Close the current surface.
    Close { outcome: Option<serde_json::Value> },
    /// Open an ordinary Bcode session and resume this surface when the native viewer returns.
    OpenSession { session_id: SessionId },
    /// Open another registered surface with renderer-neutral options.
    OpenSurface {
        plugin_id: String,
        surface_id: String,
        options: serde_json::Value,
    },
    /// Subscribe this retained surface to bounded workflow snapshots and live invalidation.
    SubscribeWorkflowRuns,
    /// Load bounded detail for one exact selected workflow run.
    SelectWorkflowRun { run_id: String },
    /// Replace the active workflow catalog query and fetch its first bounded page.
    UpdateWorkflowCatalogQuery {
        filter: bcode_workflow_view_models::WorkflowCatalogFilter,
        sort: bcode_workflow_view_models::WorkflowCatalogSort,
        group: bcode_workflow_view_models::WorkflowCatalogGroup,
        search: Option<String>,
    },
    /// Request another bounded workflow catalog page.
    LoadMoreWorkflowRuns {
        cursor: bcode_workflow_view_models::WorkflowCatalogCursor,
    },
    /// Invoke a plugin-owned command through the host application without closing this surface.
    InvokePluginCommand {
        plugin_id: String,
        command_id: String,
        arguments: Option<String>,
    },
    /// Run a host command.
    RunCommand { command: String },
}

impl PluginTuiAction {
    /// Return whether this action requests a redraw.
    #[must_use]
    pub const fn requests_redraw(&self) -> bool {
        matches!(self, Self::Redraw | Self::OpenSession { .. })
    }
}

/// Host-owned transcript header hints supplied by a plugin visual adapter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginTuiTranscriptHeader {
    /// Plugin-selected title for host transcript chrome.
    pub title: Option<String>,
    /// Configured invocation timeout in milliseconds, when known.
    pub timeout_ms: Option<u64>,
}

/// How a visual adapter's rows should be composed into the host transcript block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginTuiVisualRenderMode {
    /// Rows are rendered inside the host-provided transcript block chrome/header.
    Inline,
    /// Rows are rendered inside host transcript block chrome with a plugin-selected title.
    TranscriptBlock,
    /// Rows replace the host-provided transcript block chrome/header.
    FullBlock,
}

/// Diff layout preference supplied by the TUI host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginTuiDiffLayout {
    Auto { breakpoint: u16 },
    Unified,
    SideBySide,
}

/// Compact terminal syntax color supplied by the TUI host without palette conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginTuiSyntaxColor(u32);

impl PluginTuiSyntaxColor {
    const DEFAULT: u32 = 0;
    const ANSI_BASE: u32 = 1;
    const INDEXED_BASE: u32 = 17;
    const RGB_BASE: u32 = 273;

    /// Preserve a terminal backend color in the plugin presentation contract.
    #[must_use]
    pub const fn from_tui(color: bmux_tui::style::Color) -> Self {
        use bmux_tui::style::Color;
        let value = match color {
            Color::Default => Self::DEFAULT,
            Color::Black => Self::ANSI_BASE,
            Color::Red => Self::ANSI_BASE + 1,
            Color::Green => Self::ANSI_BASE + 2,
            Color::Yellow => Self::ANSI_BASE + 3,
            Color::Blue => Self::ANSI_BASE + 4,
            Color::Magenta => Self::ANSI_BASE + 5,
            Color::Cyan => Self::ANSI_BASE + 6,
            Color::White => Self::ANSI_BASE + 7,
            Color::BrightBlack => Self::ANSI_BASE + 8,
            Color::BrightRed => Self::ANSI_BASE + 9,
            Color::BrightGreen => Self::ANSI_BASE + 10,
            Color::BrightYellow => Self::ANSI_BASE + 11,
            Color::BrightBlue => Self::ANSI_BASE + 12,
            Color::BrightMagenta => Self::ANSI_BASE + 13,
            Color::BrightCyan => Self::ANSI_BASE + 14,
            Color::BrightWhite => Self::ANSI_BASE + 15,
            Color::Indexed(index) => Self::INDEXED_BASE + index as u32,
            Color::Rgb(r, g, b) => {
                Self::RGB_BASE + ((r as u32) << 16) + ((g as u32) << 8) + b as u32
            }
        };
        Self(value)
    }

    /// Construct an explicit RGB syntax color.
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::from_tui(bmux_tui::style::Color::Rgb(r, g, b))
    }

    /// Restore the exact terminal backend color.
    #[must_use]
    pub fn to_tui(self) -> bmux_tui::style::Color {
        use bmux_tui::style::Color;
        match self.0 {
            Self::DEFAULT => Color::Default,
            1 => Color::Black,
            2 => Color::Red,
            3 => Color::Green,
            4 => Color::Yellow,
            5 => Color::Blue,
            6 => Color::Magenta,
            7 => Color::Cyan,
            8 => Color::White,
            9 => Color::BrightBlack,
            10 => Color::BrightRed,
            11 => Color::BrightGreen,
            12 => Color::BrightYellow,
            13 => Color::BrightBlue,
            14 => Color::BrightMagenta,
            15 => Color::BrightCyan,
            16 => Color::BrightWhite,
            value if value < Self::RGB_BASE => {
                Color::Indexed(u8::try_from(value - Self::INDEXED_BASE).unwrap_or(u8::MAX))
            }
            value => {
                let rgb = value - Self::RGB_BASE;
                Color::Rgb(
                    u8::try_from((rgb >> 16) & 0xff).unwrap_or_default(),
                    u8::try_from((rgb >> 8) & 0xff).unwrap_or_default(),
                    u8::try_from(rgb & 0xff).unwrap_or_default(),
                )
            }
        }
    }
}

impl From<PluginTuiSyntaxColor> for bmux_tui::style::Color {
    fn from(color: PluginTuiSyntaxColor) -> Self {
        color.to_tui()
    }
}

/// Semantic syntax palette supplied by the TUI host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginTuiSyntaxTheme {
    pub text: PluginTuiSyntaxColor,
    pub comment: PluginTuiSyntaxColor,
    pub keyword: PluginTuiSyntaxColor,
    pub function: PluginTuiSyntaxColor,
    pub variable: PluginTuiSyntaxColor,
    pub string: PluginTuiSyntaxColor,
    pub number: PluginTuiSyntaxColor,
    pub type_name: PluginTuiSyntaxColor,
    pub operator: PluginTuiSyntaxColor,
    pub punctuation: PluginTuiSyntaxColor,
}

/// Semantic styles for source-code cards supplied by the TUI host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginTuiSourceTheme {
    pub source: Style,
    pub border: Style,
    pub gutter: Style,
    pub truncated: Style,
}

/// Semantic styles for diff presentation supplied by the TUI host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginTuiDiffTheme {
    pub text: Style,
    pub muted: Style,
    pub title: Style,
    pub label: Style,
    pub added: Style,
    pub removed: Style,
    pub hunk: Style,
    pub added_row: Style,
    pub removed_row: Style,
    pub added_emphasis: Style,
    pub removed_emphasis: Style,
}

/// Current host component-theme contract version.
pub const PLUGIN_TUI_COMPONENT_THEME_VERSION: u16 = 1;

/// Renderer-owned semantic presentation passed to native TUI plugin adapters.
///
/// This context contains presentation only. It must not affect plugin routing,
/// authorization, dispatch, or persisted outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginTuiTheme {
    /// Version of the host component-theme contract.
    pub component_theme_version: u16,
    /// Base canvas style.
    pub canvas: Style,
    /// Primary text style.
    pub text: Style,
    /// Secondary text style.
    pub muted: Style,
    /// Default border/separator style.
    pub border: Style,
    /// Focused control/accent style.
    pub focused: Style,
    /// Active selection style.
    pub selection: Style,
    /// Source-code card presentation.
    pub source: PluginTuiSourceTheme,
    /// Diff presentation.
    pub diff: PluginTuiDiffTheme,
    /// Syntax presentation.
    pub syntax: PluginTuiSyntaxTheme,
}

impl PluginTuiTheme {
    /// Return a compatible generic component theme when one was supplied.
    #[must_use]
    pub const fn component_theme(&self) -> Option<bmux_tui_components::theme::ComponentTheme> {
        if self.component_theme_version == PLUGIN_TUI_COMPONENT_THEME_VERSION {
            Some(bmux_tui_components::theme::ComponentTheme {
                canvas: self.canvas,
                surfaces: bmux_tui_components::theme::ComponentSurfaces {
                    normal: self.canvas,
                    raised: self.canvas,
                    overlay: self.canvas,
                    scrim: None,
                },
                text: self.text,
                focused: self.focused,
                selected: self.selection,
                disabled: self.muted.add_modifier(bmux_tui::style::Modifier::DIM),
                muted: self.muted,
                info: self.focused,
                success: self.focused,
                warning: self.focused,
                error: self.focused,
                border: self.border,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod component_theme_tests {
    use super::*;

    fn theme(version: u16) -> PluginTuiTheme {
        let style = Style::new();
        let syntax = PluginTuiSyntaxTheme {
            text: PluginTuiSyntaxColor::from_tui(bmux_tui::style::Color::Default),
            comment: PluginTuiSyntaxColor::from_tui(bmux_tui::style::Color::Default),
            keyword: PluginTuiSyntaxColor::from_tui(bmux_tui::style::Color::Default),
            function: PluginTuiSyntaxColor::from_tui(bmux_tui::style::Color::Default),
            variable: PluginTuiSyntaxColor::from_tui(bmux_tui::style::Color::Default),
            string: PluginTuiSyntaxColor::from_tui(bmux_tui::style::Color::Default),
            number: PluginTuiSyntaxColor::from_tui(bmux_tui::style::Color::Default),
            type_name: PluginTuiSyntaxColor::from_tui(bmux_tui::style::Color::Default),
            operator: PluginTuiSyntaxColor::from_tui(bmux_tui::style::Color::Default),
            punctuation: PluginTuiSyntaxColor::from_tui(bmux_tui::style::Color::Default),
        };
        PluginTuiTheme {
            component_theme_version: version,
            canvas: style,
            text: style,
            muted: style,
            border: style,
            focused: style,
            selection: style,
            source: PluginTuiSourceTheme {
                source: style,
                border: style,
                gutter: style,
                truncated: style,
            },
            diff: PluginTuiDiffTheme {
                text: style,
                muted: style,
                title: style,
                label: style,
                added: style,
                removed: style,
                hunk: style,
                added_row: style,
                removed_row: style,
                added_emphasis: style,
                removed_emphasis: style,
            },
            syntax,
        }
    }

    #[test]
    fn component_theme_is_available_only_for_matching_contract_version() {
        assert!(
            theme(PLUGIN_TUI_COMPONENT_THEME_VERSION)
                .component_theme()
                .is_some()
        );
        assert!(theme(u16::MAX).component_theme().is_none());
    }
}

/// Host-owned presentation context for visual adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginTuiVisualRenderContext {
    width: u16,
    diff_layout: PluginTuiDiffLayout,
    working_directory: Option<PathBuf>,
    theme: Option<PluginTuiTheme>,
}

impl PluginTuiVisualRenderContext {
    /// Construct a complete visual presentation context.
    #[must_use]
    pub const fn new(
        width: u16,
        diff_layout: PluginTuiDiffLayout,
        working_directory: Option<PathBuf>,
    ) -> Self {
        Self {
            width,
            diff_layout,
            working_directory,
            theme: None,
        }
    }

    /// Attach renderer-owned semantic presentation to the context.
    #[must_use]
    pub const fn with_theme(mut self, theme: PluginTuiTheme) -> Self {
        self.theme = Some(theme);
        self
    }

    /// Return the renderer-owned semantic presentation, when supplied.
    #[must_use]
    pub const fn theme(&self) -> Option<PluginTuiTheme> {
        self.theme
    }

    /// Return the width assigned to the visual.
    #[must_use]
    pub const fn width(&self) -> u16 {
        self.width
    }

    /// Return the effective diff viewer policy.
    #[must_use]
    pub const fn diff_layout(&self) -> PluginTuiDiffLayout {
        self.diff_layout
    }

    /// Return the invocation working directory supplied by the host.
    #[must_use]
    pub fn working_directory(&self) -> Option<&Path> {
        self.working_directory.as_deref()
    }

    /// Format a path against the invocation working directory when known.
    #[must_use]
    pub fn display_path(&self, path: impl AsRef<Path>) -> crate::path::DisplayPath {
        self.working_directory.as_deref().map_or_else(
            || crate::path::display_without_base(path.as_ref()),
            |working_directory| crate::path::display(path.as_ref(), working_directory),
        )
    }
}

/// Opaque bounded artifact bytes delivered asynchronously to a plugin-owned visual adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginTuiArtifactChunk {
    pub tool_call_id: String,
    pub artifact_id: String,
    pub reference_key: String,
    pub producer_plugin_id: String,
    pub schema: String,
    pub schema_version: u32,
    pub content_type: Option<String>,
    pub offset: u64,
    pub total_bytes: u64,
    pub revision: u64,
    pub finalized: bool,
    pub bytes: Vec<u8>,
}

/// Maximum diagnostic observations drained from one adapter at a time.
pub const MAX_PLUGIN_TUI_DIAGNOSTICS_PER_DRAIN: usize = 64;

/// One bounded numeric diagnostic emitted by a plugin-owned visual adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginTuiDiagnostic {
    /// Stable adapter-owned diagnostic name.
    pub name: String,
    /// Non-negative observation value.
    pub value: u64,
}

/// Native Rust plugin artifact/view renderer for inline transcript content.
pub trait PluginTuiVisualAdapter: Send + Sync {
    /// Return whether this adapter can render the artifact/view kind.
    fn supports(&self, kind: &str) -> bool;

    /// Return how this visual should be composed by the host.
    fn render_mode(&self, _kind: &str, _payload: &serde_json::Value) -> PluginTuiVisualRenderMode {
        PluginTuiVisualRenderMode::Inline
    }

    /// Return plugin-owned hints for the host transcript header.
    fn transcript_header(
        &self,
        _kind: &str,
        _payload: &serde_json::Value,
    ) -> PluginTuiTranscriptHeader {
        PluginTuiTranscriptHeader::default()
    }

    /// Convert a renderer event into neutral input for an active invocation.
    fn invocation_event_input(
        &self,
        _invocation_id: &str,
        _kind: &str,
        _payload: &serde_json::Value,
        _event: &Event,
    ) -> Option<bcode_tool::ToolInvocationInput> {
        None
    }

    /// Return whether this adapter consumes streamed bytes for one artifact reference.
    ///
    /// Hosts use this before scheduling range reads so unrelated references on the same artifact
    /// are not fetched or retried through an adapter that cannot interpret them.
    fn accepts_artifact_reference(
        &self,
        _kind: &str,
        _reference_key: &str,
        _content_type: Option<&str>,
    ) -> bool {
        false
    }

    /// Consume one ordered opaque artifact range fetched by the host outside rendering.
    ///
    /// The host may redeliver metadata revisions but does not redeliver byte ranges. Adapters must
    /// interpret bytes only for artifact schemas they own.
    ///
    /// # Errors
    ///
    /// Returns an error when the chunk metadata, byte range, or plugin-owned payload is invalid.
    fn artifact_chunk(&self, _chunk: &PluginTuiArtifactChunk) -> Result<(), String> {
        Ok(())
    }

    /// Drain a bounded snapshot of adapter-owned work diagnostics.
    ///
    /// The default implementation emits no diagnostics. Hosts validate names and cap every drain;
    /// adapters must report only low-cardinality numeric work observations.
    fn drain_diagnostics(&self) -> Vec<PluginTuiDiagnostic> {
        Vec::new()
    }

    /// Build transcript rows for the artifact/view payload at the given width.
    fn rows(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        context: &PluginTuiVisualRenderContext,
    ) -> Vec<Line>;
}

/// Native Rust plugin surface rendered directly with `bmux_tui`.
pub trait PluginTuiSurface: Send {
    /// Stable surface identifier.
    fn id(&self) -> &'static str;

    /// Human-readable surface title.
    fn title(&self) -> &'static str;

    /// Return preferred height for this surface at the given width.
    #[must_use]
    fn preferred_height(&mut self, _width: u16) -> u16 {
        1
    }

    /// Render this surface inside the host-assigned area.
    fn render(&mut self, area: Rect, frame: &mut Frame<'_>);

    /// Render one bounded logical row slice into a host-assigned destination.
    ///
    /// The default treats the destination as a complete standalone surface. Surfaces whose logical
    /// extent can exceed the destination should override this method and preserve logical
    /// coordinates across slices.
    fn render_slice(
        &mut self,
        _logical_height: u16,
        _logical_row_offset: u16,
        destination: Rect,
        frame: &mut Frame<'_>,
    ) {
        self.render(destination, frame);
    }

    /// Return the focused control's logical row range, when the surface can provide one.
    #[must_use]
    fn focused_row_range(&mut self, _width: u16) -> Option<std::ops::Range<u16>> {
        None
    }

    /// Render this surface with optional renderer-owned semantic presentation.
    ///
    /// The default preserves compatibility for surfaces that do not consume themes.
    fn render_with_theme(
        &mut self,
        area: Rect,
        frame: &mut Frame<'_>,
        _theme: Option<PluginTuiTheme>,
    ) {
        self.render(area, frame);
    }

    /// Handle routed terminal input.
    fn handle_event(&mut self, event: &Event, host: &dyn PluginTuiHost) -> PluginTuiAction;

    /// Attach a renderer-neutral update stream to this retained surface.
    ///
    /// The default drops the stream, preserving compatibility for static surfaces.
    fn attach_updates(&mut self, _updates: PluginTuiSurfaceUpdateReceiver) {}

    /// Notify a retained surface that native-session navigation finished or failed.
    fn session_navigation_finished(&mut self, _session_id: SessionId, _result: Result<(), String>) {
    }

    /// Poll internal async completions without blocking.
    fn poll(&mut self, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        PluginTuiAction::None
    }

    /// Drain effectful asynchronous work that was queued by synchronous input handling.
    ///
    /// This compatibility hook borrows the surface for the duration of the future. Hosts must
    /// await it while retaining exclusive surface ownership; it cannot be scheduled as an owned
    /// BMUX runtime command. Runtime-driven surfaces should instead model effect work as owned
    /// request/result messages so application mutation remains serialized in `Program::update`.
    fn drain_effects<'a>(
        &'a mut self,
        _host: &'a dyn PluginTuiHost,
    ) -> Pin<Box<dyn Future<Output = PluginTuiAction> + Send + 'a>> {
        Box::pin(async { PluginTuiAction::None })
    }
}

/// Result of translating one terminal event for a typed interaction renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalInteractionInput {
    /// The renderer did not handle the event, so the host may route it elsewhere.
    Ignored,
    /// The renderer handled local state without producing semantic controller input.
    Consumed,
    /// The renderer translated the event into semantic controller input.
    Semantic(InteractionInput),
}

/// Terminal renderer/adapter for a typed renderer-neutral interaction.
pub trait TerminalInteractionRenderer<C>: Default + Send + 'static
where
    C: PluginInteraction,
{
    /// Native terminal surface kind.
    const SURFACE_KIND: &'static str;

    /// Stable surface identifier.
    fn id(&self) -> &'static str;

    /// Human-readable surface title.
    fn title(&self) -> &'static str;

    /// Return preferred height for a snapshot at the given width.
    #[must_use]
    fn preferred_height(&mut self, snapshot: &C::Snapshot, width: u16) -> u16;

    /// Render the snapshot.
    fn render(&mut self, snapshot: &C::Snapshot, area: Rect, frame: &mut Frame<'_>);

    /// Render one bounded logical row slice.
    fn render_slice(
        &mut self,
        snapshot: &C::Snapshot,
        logical_height: u16,
        logical_row_offset: u16,
        destination: Rect,
        frame: &mut Frame<'_>,
    ) {
        let _ = (logical_height, logical_row_offset);
        self.render(snapshot, destination, frame);
    }

    /// Return the focused control's logical row range, when available.
    #[must_use]
    fn focused_row_range(
        &mut self,
        _snapshot: &C::Snapshot,
        _width: u16,
    ) -> Option<std::ops::Range<u16>> {
        None
    }

    /// Render the snapshot with optional renderer-owned semantic presentation.
    ///
    /// The default preserves compatibility for renderers that do not consume themes.
    fn render_with_theme(
        &mut self,
        snapshot: &C::Snapshot,
        area: Rect,
        frame: &mut Frame<'_>,
        _theme: Option<PluginTuiTheme>,
    ) {
        self.render(snapshot, area, frame);
    }

    /// Translate terminal input and report whether it was consumed.
    fn input(
        &mut self,
        event: &Event,
        snapshot: &C::Snapshot,
        _host: &dyn PluginTuiHost,
    ) -> TerminalInteractionInput;
}

struct TypedTerminalInteractionSurface<C, R>
where
    C: PluginInteraction,
    R: TerminalInteractionRenderer<C>,
{
    controller: C,
    renderer: R,
}

impl<C, R> TypedTerminalInteractionSurface<C, R>
where
    C: PluginInteraction,
    R: TerminalInteractionRenderer<C>,
{
    const fn new(controller: C, renderer: R) -> Self {
        Self {
            controller,
            renderer,
        }
    }
}

impl<C, R> PluginTuiSurface for TypedTerminalInteractionSurface<C, R>
where
    C: PluginInteraction,
    R: TerminalInteractionRenderer<C>,
{
    fn id(&self) -> &'static str {
        self.renderer.id()
    }

    fn title(&self) -> &'static str {
        self.renderer.title()
    }

    fn preferred_height(&mut self, width: u16) -> u16 {
        self.renderer
            .preferred_height(&self.controller.snapshot(), width)
    }

    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        self.renderer
            .render(&self.controller.snapshot(), area, frame);
    }

    fn render_slice(
        &mut self,
        logical_height: u16,
        logical_row_offset: u16,
        destination: Rect,
        frame: &mut Frame<'_>,
    ) {
        self.renderer.render_slice(
            &self.controller.snapshot(),
            logical_height,
            logical_row_offset,
            destination,
            frame,
        );
    }

    fn focused_row_range(&mut self, width: u16) -> Option<std::ops::Range<u16>> {
        self.renderer
            .focused_row_range(&self.controller.snapshot(), width)
    }

    fn render_with_theme(
        &mut self,
        area: Rect,
        frame: &mut Frame<'_>,
        theme: Option<PluginTuiTheme>,
    ) {
        self.renderer
            .render_with_theme(&self.controller.snapshot(), area, frame, theme);
    }

    fn handle_event(&mut self, event: &Event, host: &dyn PluginTuiHost) -> PluginTuiAction {
        match self
            .renderer
            .input(event, &self.controller.snapshot(), host)
        {
            TerminalInteractionInput::Ignored => PluginTuiAction::None,
            TerminalInteractionInput::Consumed => PluginTuiAction::Redraw,
            TerminalInteractionInput::Semantic(input) => {
                plugin_tui_action_from_interaction_output(self.controller.handle_input(input))
            }
        }
    }
}

/// Factory for typed terminal interaction surfaces.
pub struct TypedTerminalInteractionSurfaceFactory<C, R>
where
    C: PluginInteraction,
    R: TerminalInteractionRenderer<C>,
{
    marker: PhantomData<fn() -> (C, R)>,
}

impl<C, R> TypedTerminalInteractionSurfaceFactory<C, R>
where
    C: PluginInteraction,
    R: TerminalInteractionRenderer<C>,
{
    /// Create a typed terminal interaction surface factory.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            marker: PhantomData,
        }
    }
}

impl<C, R> Default for TypedTerminalInteractionSurfaceFactory<C, R>
where
    C: PluginInteraction,
    R: TerminalInteractionRenderer<C>,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<C, R> PluginTuiSurfaceFactory for TypedTerminalInteractionSurfaceFactory<C, R>
where
    C: PluginInteraction,
    R: TerminalInteractionRenderer<C>,
{
    fn surface_kind(&self) -> &'static str {
        R::SURFACE_KIND
    }

    fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        Box::pin(async move {
            let request = serde_json::from_value::<C::Request>(request.options)?;
            Ok(Box::new(TypedTerminalInteractionSurface::<C, R>::new(
                C::new(request),
                R::default(),
            )) as BoxedPluginTuiSurface)
        })
    }
}

fn plugin_tui_action_from_interaction_output(output: InteractionOutput) -> PluginTuiAction {
    match output {
        InteractionOutput::None => PluginTuiAction::None,
        InteractionOutput::Redraw => PluginTuiAction::Redraw,
        InteractionOutput::Submitted { payload } => PluginTuiAction::Close {
            outcome: Some(payload),
        },
        InteractionOutput::Cancelled => PluginTuiAction::Close { outcome: None },
    }
}

/// Parameters used to open a native plugin TUI surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginTuiSurfaceOpenRequest {
    /// Host-assigned surface instance id.
    pub instance_id: String,
    /// Repository path or workspace path associated with the surface.
    pub repo_path: Option<PathBuf>,
    /// Plugin-defined target identifier.
    pub target: Option<String>,
    /// Plugin-defined JSON options.
    #[serde(default)]
    pub options: serde_json::Value,
}

/// Factory for plugin-owned native TUI surfaces.
pub trait PluginTuiSurfaceFactory: Send + Sync {
    /// Stable surface kind identifier.
    fn surface_kind(&self) -> &'static str;

    /// Open a new surface instance.
    ///
    /// # Errors
    ///
    /// Returns an error when the requested surface cannot be opened.
    fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture;
}

/// Factory for one statically linked plugin TUI registry.
pub type PluginTuiRegistryFactory = fn() -> PluginTuiRegistry;

/// Distribution-provided static TUI extension registration.
#[derive(Clone, Copy)]
pub struct StaticPluginTuiExtension {
    plugin_id: &'static str,
    registry_factory: PluginTuiRegistryFactory,
}

impl std::fmt::Debug for StaticPluginTuiExtension {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StaticPluginTuiExtension")
            .field("plugin_id", &self.plugin_id)
            .finish_non_exhaustive()
    }
}

impl StaticPluginTuiExtension {
    /// Register one static plugin's native TUI extension factory.
    #[must_use]
    pub const fn new(plugin_id: &'static str, registry_factory: PluginTuiRegistryFactory) -> Self {
        Self {
            plugin_id,
            registry_factory,
        }
    }

    /// Return the owning plugin ID.
    #[must_use]
    pub const fn plugin_id(self) -> &'static str {
        self.plugin_id
    }

    /// Return the native TUI registry factory.
    #[must_use]
    pub const fn registry_factory(self) -> PluginTuiRegistryFactory {
        self.registry_factory
    }

    /// Construct an independent native TUI registry.
    #[must_use]
    pub fn registry(self) -> PluginTuiRegistry {
        (self.registry_factory)()
    }
}

/// Registry of native TUI surfaces contributed by one plugin.
#[derive(Default)]
pub struct PluginTuiRegistry {
    factories: BTreeMap<String, Box<dyn PluginTuiSurfaceFactory>>,
    visual_adapters: BTreeMap<String, std::sync::Arc<dyn PluginTuiVisualAdapter>>,
}

impl std::fmt::Debug for PluginTuiRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginTuiRegistry")
            .field("surface_kinds", &self.factories.keys().collect::<Vec<_>>())
            .field(
                "visual_adapters",
                &self.visual_adapters.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl PluginTuiRegistry {
    /// Register a native TUI surface factory.
    pub fn register_factory(&mut self, factory: Box<dyn PluginTuiSurfaceFactory>) {
        self.factories
            .insert(factory.surface_kind().to_string(), factory);
    }

    /// Register a typed terminal renderer for a renderer-neutral interaction.
    pub fn register_interactive_surface<C, R>(&mut self)
    where
        C: PluginInteraction,
        R: TerminalInteractionRenderer<C>,
    {
        self.register_factory(Box::new(
            TypedTerminalInteractionSurfaceFactory::<C, R>::new(),
        ));
    }

    /// Register a native TUI visual adapter under one or more manifest adapter IDs.
    pub fn register_visual_adapter<I, S>(
        &mut self,
        adapter_ids: I,
        adapter: Box<dyn PluginTuiVisualAdapter>,
    ) where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let adapter: std::sync::Arc<dyn PluginTuiVisualAdapter> = adapter.into();
        for adapter_id in adapter_ids {
            self.visual_adapters
                .insert(adapter_id.into(), std::sync::Arc::clone(&adapter));
        }
    }

    fn visual_adapter(&self, adapter_id: &str, kind: &str) -> Option<&dyn PluginTuiVisualAdapter> {
        self.visual_adapters
            .get(adapter_id)
            .filter(|adapter| adapter.supports(kind))
            .map(std::convert::AsRef::as_ref)
    }

    /// Return the number of native visual adapters in this registry.
    #[must_use]
    pub fn visual_adapter_count(&self) -> usize {
        self.visual_adapters.len()
    }

    /// Return whether the exact registered adapter supports this payload kind.
    #[must_use]
    pub fn supports_visual_adapter(&self, adapter_id: &str, kind: &str) -> bool {
        self.visual_adapter(adapter_id, kind).is_some()
    }

    /// Return whether any native visual adapter supports this payload kind.
    #[must_use]
    pub fn supports_visual(&self, kind: &str) -> bool {
        self.visual_adapters
            .values()
            .any(|adapter| adapter.supports(kind))
    }

    /// Return how a native visual adapter wants this payload composed.
    #[must_use]
    pub fn visual_render_mode(
        &self,
        adapter_id: &str,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Option<PluginTuiVisualRenderMode> {
        self.visual_adapter(adapter_id, kind)
            .map(|adapter| adapter.render_mode(kind, payload))
    }

    /// Return host transcript header hints from a matching visual adapter.
    #[must_use]
    pub fn visual_transcript_header(
        &self,
        adapter_id: &str,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Option<PluginTuiTranscriptHeader> {
        self.visual_adapter(adapter_id, kind)
            .map(|adapter| adapter.transcript_header(kind, payload))
    }

    /// Convert a renderer event through a matching visual adapter.
    #[must_use]
    pub fn visual_invocation_event_input(
        &self,
        adapter_id: &str,
        invocation_id: &str,
        kind: &str,
        payload: &serde_json::Value,
        event: &Event,
    ) -> Option<bcode_tool::ToolInvocationInput> {
        self.visual_adapter(adapter_id, kind)
            .and_then(|adapter| adapter.invocation_event_input(invocation_id, kind, payload, event))
    }

    /// Return whether the owning visual adapter consumes one artifact reference.
    #[must_use]
    pub fn visual_accepts_artifact_reference(
        &self,
        adapter_id: &str,
        kind: &str,
        reference_key: &str,
        content_type: Option<&str>,
    ) -> bool {
        self.visual_adapter(adapter_id, kind)
            .is_some_and(|adapter| {
                adapter.accepts_artifact_reference(kind, reference_key, content_type)
            })
    }

    /// Deliver opaque artifact bytes through the adapter that owns the artifact schema.
    ///
    /// # Errors
    ///
    /// Returns an error when the owning adapter rejects malformed or non-contiguous bytes.
    pub fn visual_artifact_chunk(
        &self,
        adapter_id: &str,
        chunk: &PluginTuiArtifactChunk,
    ) -> Result<bool, String> {
        let Some(adapter) = self.visual_adapter(adapter_id, &chunk.schema) else {
            return Ok(false);
        };
        adapter.artifact_chunk(chunk)?;
        Ok(true)
    }

    /// Drain bounded diagnostics from every registered visual adapter.
    #[must_use]
    pub fn drain_visual_diagnostics(&self) -> Vec<PluginTuiDiagnostic> {
        self.visual_adapters
            .values()
            .flat_map(|adapter| {
                adapter
                    .drain_diagnostics()
                    .into_iter()
                    .take(MAX_PLUGIN_TUI_DIAGNOSTICS_PER_DRAIN)
            })
            .take(MAX_PLUGIN_TUI_DIAGNOSTICS_PER_DRAIN)
            .collect()
    }

    /// Build transcript rows with host-owned presentation preferences.
    #[must_use]
    pub fn visual_rows(
        &self,
        adapter_id: &str,
        kind: &str,
        payload: &serde_json::Value,
        context: &PluginTuiVisualRenderContext,
    ) -> Option<Vec<Line>> {
        self.visual_adapter(adapter_id, kind)
            .map(|adapter| adapter.rows(kind, payload, context))
    }

    /// Open a registered surface.
    ///
    /// # Errors
    ///
    /// Returns an error when no factory exists or the factory fails to open the surface.
    pub async fn open(
        &self,
        surface_kind: &str,
        request: PluginTuiSurfaceOpenRequest,
    ) -> Result<BoxedPluginTuiSurface, PluginTuiError> {
        let factory = self
            .factories
            .get(surface_kind)
            .ok_or_else(|| format!("unsupported TUI surface kind: {surface_kind}"))?;
        factory.open(request).await
    }
}

#[derive(Clone)]
pub struct TokioPluginTuiHost {
    handle: tokio::runtime::Handle,
    redraw_sender: mpsc::Sender<()>,
    text_input: Option<Arc<dyn PluginTuiTextInputResolver>>,
}

impl std::fmt::Debug for TokioPluginTuiHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokioPluginTuiHost")
            .field(
                "text_input",
                &self.text_input.as_ref().map(|_| "configured"),
            )
            .finish_non_exhaustive()
    }
}

/// Host resolver for configured composer-like text input intent.
pub trait PluginTuiTextInputResolver: Send + Sync {
    /// Resolve one configured edit command.
    fn edit_command(&self, stroke: KeyStroke) -> Option<TextEditCommand>;

    /// Resolve one configured selection motion.
    fn selection_motion(&self, stroke: KeyStroke) -> Option<TextMotion>;

    /// Return whether this stroke submits composer-like input.
    fn submits(&self, stroke: KeyStroke) -> bool;
}

impl TokioPluginTuiHost {
    /// Create a host handle from the current Tokio runtime.
    ///
    /// # Panics
    ///
    /// Panics if called outside a Tokio runtime.
    #[must_use]
    pub fn current(redraw_sender: mpsc::Sender<()>) -> Self {
        Self {
            handle: tokio::runtime::Handle::current(),
            redraw_sender,
            text_input: None,
        }
    }

    /// Attach a host-configured composer-like text input resolver.
    #[must_use]
    pub fn with_text_input_resolver(
        mut self,
        resolver: Arc<dyn PluginTuiTextInputResolver>,
    ) -> Self {
        self.text_input = Some(resolver);
        self
    }

    /// Replace the host-configured composer-like text input resolver.
    pub fn set_text_input_resolver(&mut self, resolver: Arc<dyn PluginTuiTextInputResolver>) {
        self.text_input = Some(resolver);
    }
}

impl PluginTuiHost for TokioPluginTuiHost {
    fn spawn(&self, task: PluginTask) {
        let redraw_sender = self.redraw_sender.clone();
        drop(self.handle.spawn(async move {
            task.await;
            let _ = redraw_sender.try_send(());
        }));
    }

    fn spawn_blocking(&self, task: Box<dyn FnOnce() + Send + 'static>) {
        let redraw_sender = self.redraw_sender.clone();
        drop(self.handle.spawn_blocking(move || {
            task();
            let _ = redraw_sender.try_send(());
        }));
    }

    fn request_redraw(&self) {
        let _ = self.redraw_sender.try_send(());
    }

    fn text_edit_command(&self, stroke: KeyStroke) -> Option<TextEditCommand> {
        self.text_input.as_ref()?.edit_command(stroke)
    }

    fn text_selection_motion(&self, stroke: KeyStroke) -> Option<TextMotion> {
        self.text_input.as_ref()?.selection_motion(stroke)
    }

    fn text_submit(&self, stroke: KeyStroke) -> bool {
        self.text_input
            .as_ref()
            .is_some_and(|resolver| resolver.submits(stroke))
    }
}

#[cfg(test)]
mod workflow_package_tests {
    use super::*;

    #[test]
    fn portable_package_export_start_round_trips() {
        let request = PluginWorkflowPackageExportStartRequest {
            package_export: bcode_workflow::WorkflowPackageExportIdentity {
                package_id: "example/package".to_string(),
                export: "main".to_string(),
                package_lock_digest_sha256: Some("a".repeat(64)),
            },
            run_id: Some("run-1".to_string()),
            parent_session_id: SessionId::new(),
            workspace_snapshot: Some("workspace".to_string()),
            parent_session_generation: Some(1),
            configuration: Some(serde_json::json!({"mode": "safe"})),
            input: Some(serde_json::json!({"subject": "change"})),
        };
        let encoded = serde_json::to_vec(&request).expect("encode request");
        let decoded: PluginWorkflowPackageExportStartRequest =
            serde_json::from_slice(&encoded).expect("decode request");
        assert_eq!(decoded, request);
    }
}
