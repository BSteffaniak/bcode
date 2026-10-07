//! Workflow execution-scoped, model-only contributor evidence contracts.
//!
//! These are application contracts, not public frontend events or session-store APIs.
use serde::{Deserialize, Serialize};

/// Independently compatible evidence response contract.
pub const CONTRACT_ID: &str = "bcode.workflow.execution_contributor_evidence.v1";
/// Workflow application operation; host derives all contributor authority.
pub const OPERATION: &str = "execution_contributor_evidence";

/// Exact revision and bounded action. Contributor/session selectors are forbidden.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionContributorEvidenceRequest {
    /// Current graph revision observed through authenticated execution context.
    pub expected_revision: u64,
    /// Discovery or exact request/result inspection within the proved contributor.
    pub action: ContributorEvidenceAction,
}

/// Bounded evidence operation, never an access grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContributorEvidenceAction {
    /// Scan at most 100 canonical events; short output does not imply no execution.
    Discover {
        /// Canonical history cursor; null starts at the beginning.
        after_sequence: Option<u64>,
        /// Maximum canonical events scanned, between 1 and 100.
        limit: u16,
    },
    /// Inspect an exact invocation pair, not an arbitrary session window.
    Read {
        /// Invocation identity discovered in the authorized contributor.
        invocation_id: String,
        /// Exact canonical request sequence.
        request_sequence: u64,
        /// Exact canonical result sequence, strictly after the request.
        result_sequence: u64,
        /// Combined retained text bound, between 1 and 65,536 bytes.
        byte_limit: u32,
    },
}

impl ExecutionContributorEvidenceRequest {
    /// Validate normalized bounds before preparation and dispatch.
    /// # Errors
    /// Rejects zero revisions, invalid bounds, blank/oversized invocation IDs or reversed pairs.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.expected_revision == 0 {
            return Err("expected_revision must be positive");
        }
        match &self.action {
            ContributorEvidenceAction::Discover { limit, .. } if !(1..=100).contains(limit) => {
                Err("discovery limit must be between 1 and 100")
            }
            ContributorEvidenceAction::Read {
                invocation_id,
                request_sequence,
                result_sequence,
                byte_limit,
            } if invocation_id.trim().is_empty()
                || invocation_id.len() > 1_024
                || result_sequence <= request_sequence
                || !(1..=65_536).contains(byte_limit) =>
            {
                Err("invalid exact evidence pair or byte limit")
            }
            _ => Ok(()),
        }
    }
}

/// Coverage status; unavailable evidence is never success evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContributorEvidenceStatus {
    /// Supported contributor and requested evidence were authenticated.
    Available,
    /// Generated delegation or another topology lacks the supported direct proof.
    Unsupported,
    /// Contributor provenance or requested canonical evidence is unavailable.
    ProvenanceUnavailable,
}

/// Portable identity facts selected by the host, not caller authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContributorEvidenceScope {
    /// Workflow run identity.
    pub run_id: String,
    /// Authenticated consuming activation.
    pub consumer_activation_id: String,
    /// Still-current admitted graph revision.
    pub graph_revision: u64,
    /// Contributor node identity.
    pub node_id: String,
    /// Contributor activation identity.
    pub activation_id: String,
    /// Exact successful contributor attempt.
    pub attempt: u32,
    /// Host-selected canonical session identity, represented portably.
    pub session_id: String,
    /// Exact committed output identity.
    pub output_id: String,
    /// Canonical output checksum, not a completion claim.
    pub output_checksum_sha256: String,
}

/// Canonical tool event role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContributorEventRole {
    /// Tool invocation request.
    Request,
    /// Tool invocation result.
    Result,
}

/// Exact canonical locator; it grants no independent access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContributorEventReference {
    /// Canonical event sequence.
    pub sequence: u64,
    /// Canonical invocation identity.
    pub invocation_id: String,
    /// Request or result.
    pub role: ContributorEventRole,
}

/// Bounded model-only text; completeness does not certify producer capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContributorEvidenceText {
    /// Untrusted canonical model text.
    pub text: String,
    /// Original UTF-8 byte length.
    pub original_bytes: usize,
    /// Retained UTF-8 byte length.
    pub retained_bytes: usize,
    /// True when retrieval omitted text.
    pub truncated: bool,
}

/// Recorded invocation pair, not proof of goal completion or integration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContributorInvocationEvidence {
    /// Canonical invocation identity.
    pub invocation_id: String,
    /// Canonical request sequence.
    pub request_sequence: u64,
    /// Canonical result sequence.
    pub result_sequence: u64,
    /// Optional normalized producer identity.
    pub producer_plugin_id: Option<String>,
    /// Recorded tool name.
    pub tool_name: String,
    /// Recorded request arguments, bounded and untrusted.
    pub arguments_json: ContributorEvidenceText,
    /// Recorded model result, bounded and untrusted.
    pub model_output: ContributorEvidenceText,
    /// Recorded tool error outcome.
    pub is_error: bool,
    /// Unknown unless the producer explicitly certified capture completeness.
    pub producer_capture_complete: Option<bool>,
}

/// Typed service response without persistence/provider implementation types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionContributorEvidenceResponse {
    /// Must equal [`CONTRACT_ID`]; unknown contracts must not be guessed.
    pub contract: String,
    /// Explicit proof/coverage status.
    pub status: ContributorEvidenceStatus,
    /// Host-selected contributor scope, absent when no scope was proved.
    pub scope: Option<ContributorEvidenceScope>,
    /// Discovery locators within the bounded canonical scan.
    pub events: Vec<ContributorEventReference>,
    /// Number of canonical events examined, including non-tool events.
    pub scanned_events: usize,
    /// Canonical next-page cursor; absence describes this scan, not future session EOF.
    pub next_sequence: Option<u64>,
    /// Exact recorded pair for read actions.
    pub evidence: Option<ContributorInvocationEvidence>,
}
