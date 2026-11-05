//! Portable presentation of managed external execution. These are derived views, not authority.

use serde::{Deserialize, Serialize};

/// Provenance displayed beside every external transcript item and interaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalProvenanceView {
    /// Owning Bcode session, never an inferred native child.
    pub session_id: String,
    /// Configured harness instance.
    pub instance_id: String,
    /// Owning plugin, retained even when disabled.
    pub plugin_id: String,
    /// Native session correlation, not a Bcode execution target.
    pub native_session_id: String,
    /// Ownership fence under which the observation was recorded.
    pub generation: u64,
}

/// A canonical external observation presented without native protocol payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalTranscriptItemView {
    /// Stable canonical observation identity, not text-derived.
    pub observation_id: String,
    /// Canonical session sequence used for ordering and bounded history navigation.
    pub sequence: u64,
    /// Explicit native ownership attribution.
    pub provenance: ExternalProvenanceView,
    /// Generic fallback category.
    pub category: ExternalTranscriptCategory,
    /// Normalized, secret-safe display content only.
    pub text: String,
    /// Whether the producer reported incomplete observation.
    pub incomplete: bool,
}

/// Fallback category; observed native tools are never executable Bcode tool requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalTranscriptCategory {
    /// User content submitted to the native harness.
    User,
    /// Assistant content observed from the harness.
    Assistant,
    /// Observed reasoning, subject to application policy.
    Reasoning,
    /// A native tool observation, with no Bcode dispatch affordance.
    NativeTool,
    /// A native subagent observation, not an independently owned Bcode child.
    NativeSubagent,
    /// Lifecycle, diagnostic, or other normalized observation.
    Status,
}

/// Independent evidence dimensions; none implies assignment acceptance or completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalActivityView {
    /// Connectivity evidence.
    pub connectivity: String,
    /// Execution evidence, including explicit uncertainty when applicable.
    pub execution: String,
    /// Attention evidence independent of connectivity and execution.
    pub attention: String,
    /// Native continuation availability, independent of historical readability.
    pub continuation: String,
    /// Observation coverage, independent of whether this history page has a successor.
    pub completeness: String,
}

/// Capability-aware affordance; disabled actions retain an explanation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalActionView {
    /// Intent only; never a tool-execution instruction.
    pub action: ExternalActionKind,
    /// Whether a response may currently be offered by the frontend.
    pub enabled: bool,
    /// Human-readable explanation, especially when unavailable.
    pub reason: String,
}

/// Managed-session application actions. Authority is rechecked by the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalActionKind {
    /// Submit ordinary input.
    Submit,
    /// Steer only the explicitly expected native turn.
    Steer,
    /// Answer a current question or assistance request.
    Respond,
    /// Decide a native approval request without running its tool in Bcode.
    Approve,
    /// Request interrupt; acknowledgment is not settlement.
    Interrupt,
    /// Inspect unresolved delivery without replaying a mutation.
    Reconcile,
}

/// Generic question/approval/assistance presentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalInteractionView {
    /// Native request correlation scoped by provenance and generation.
    pub request_id: String,
    /// Canonical source; parentage or display routing grants no response authority.
    pub provenance: ExternalProvenanceView,
    /// Normalized interaction kind label.
    pub kind: String,
    /// Secret-safe prompt; authentication secrets never belong here.
    pub prompt: String,
    /// Requested native scope, displayed without granting permission.
    pub scope: String,
    /// Stable choice identities and labels, in producer order.
    pub choices: Vec<ExternalChoiceView>,
    /// Response capability and freshness presentation.
    pub response: ExternalActionView,
}

/// One normalized interaction choice, not executable native input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalChoiceView {
    /// Stable native choice identity.
    pub id: String,
    /// Secret-safe label.
    pub label: String,
}

/// Non-secret launch provenance retained for offline history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalSessionMetadataView {
    /// Admitted executable selection, never an executable action.
    pub executable: String,
    /// Native compatibility/version evidence.
    pub native_version: String,
    /// Admitted non-secret configuration namespace, not credential contents.
    pub config_namespace: String,
}

/// Bounded renderer-neutral external-session page, readable without a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalSessionView {
    /// Bounded lifecycle evidence from the selected window; never execution authority.
    #[serde(default)]
    pub execution_lifecycle: crate::execution::ExecutionLifecycleView,
    /// Retained binding metadata.
    pub provenance: ExternalProvenanceView,
    /// Native executable/version/config selection retained independently of plugin loading.
    pub metadata: ExternalSessionMetadataView,
    /// Independent status evidence.
    pub activity: ExternalActivityView,
    /// Only the selected canonical history window, never a full replay.
    pub transcript: Vec<ExternalTranscriptItemView>,
    /// Only interactions included in the selected bounded projection.
    pub interactions: Vec<ExternalInteractionView>,
    /// Current capability-aware controls; display does not grant authority.
    pub actions: Vec<ExternalActionView>,
    /// Exclusive canonical forward cursor; not a native resume or mutation token.
    pub next_after_sequence: Option<u64>,
    /// Canonical indexed page reports additional history independently of completeness.
    pub has_more: bool,
    /// A presentation limit omitted content from this page.
    pub truncated: bool,
}
