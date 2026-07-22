#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Model-provider service contracts for Bcode.
//!
//! Context management has two distinct provider capabilities:
//!
//! * explicit native compaction replaces a host-selected, structurally complete prefix;
//! * provider-managed compaction emits replacement context while serving a normal request.
//!
//! Opaque replacement context is replayable only on a provider surface whose
//! [`ProviderContextFormat`] exactly matches the format that produced it. Providers must preserve
//! opaque items losslessly; hosts must retain a portable summary for incompatible surfaces.
//!
//! The normative required/optional operation and lifecycle semantics are documented in
//! [`docs/model-provider-contract.md`](https://github.com/BSteffaniak/bcode/blob/master/docs/model-provider-contract.md).

use bcode_session_models::SessionId;
use hyperchad_docs_config_derive::{ConfigDoc, ConfigDocEnum};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, BTreeSet};

mod context_management;
pub use context_management::{
    CompactContextRequest, CompactContextResponse, ContextManagementCapabilities,
    ContextManagementCapabilitiesRequest, ContextManagementRequest, ProviderContextFormat,
};

/// Provider context setting for explicit catalog provider mapping.
pub const CATALOG_PROVIDER_ID_SETTING: &str = "catalog_provider_id";

/// Plugin service interface for model providers.
pub const MODEL_PROVIDER_INTERFACE_ID: &str = "bcode.model-provider/v1";

/// Operation for provider capability discovery.
pub const OP_CAPABILITIES: &str = "capabilities";

/// Operation for context-management capability discovery for an active provider surface.
pub const OP_CONTEXT_MANAGEMENT_CAPABILITIES: &str = "context_management_capabilities";

/// Operation for provider-native context compaction.
pub const OP_COMPACT_CONTEXT: &str = "compact_context";

/// Operation for model listing.
pub const OP_MODELS: &str = "models";

/// Operation for validating provider configuration.
pub const OP_VALIDATE_CONFIG: &str = "validate_config";

/// Operation for starting a model turn.
pub const OP_START_TURN: &str = "start_turn";

/// Operation for model verification.
pub const OP_VERIFY_MODEL: &str = "verify_model";

/// Operation for polling model turn stream events.
pub const OP_POLL_TURN_EVENTS: &str = "poll_turn_events";

/// Operation for cancelling a model turn.
pub const OP_CANCEL_TURN: &str = "cancel_turn";

/// Operation for provider-native web search.
pub const OP_NATIVE_WEB_SEARCH: &str = "native_web_search";

/// Operation for provider turn cleanup.
pub const OP_FINISH_TURN: &str = "finish_turn";

/// Operation for provider-confirmed auth usage window discovery.
pub const OP_AUTH_USAGE: &str = "auth_usage";

/// Operation for explicitly priming provider auth usage windows.
pub const OP_AUTH_PRIME: &str = "auth_prime";

/// Operation for listing provider auth rate-limit reset credits.
pub const OP_AUTH_RESET_CREDITS: &str = "auth_reset_credits";

/// Operation for consuming one provider auth rate-limit reset credit.
pub const OP_AUTH_RESET_CREDIT_CONSUME: &str = "auth_reset_credit_consume";

/// Whether an operation is part of the baseline provider contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderOperationRequirement {
    /// Every `bcode.model-provider/v1` implementation must implement the operation.
    Required,
    /// The operation is required only when the provider advertises the associated capability.
    CapabilityGated(ProviderCapability),
    /// The operation is an optional extension that callers must probe before use.
    Optional,
}

/// Static description of one typed `bcode.model-provider/v1` operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderOperationContract {
    /// Stable operation name sent through the plugin service boundary.
    pub operation: &'static str,
    /// Rust request payload type. `()` means an empty JSON request.
    pub request_type: &'static str,
    /// Rust success response payload type.
    pub response_type: &'static str,
    /// Baseline, capability-gated, or optional requirement.
    pub requirement: ProviderOperationRequirement,
    /// Concise behavioral requirement beyond successful typed decoding.
    pub behavior: &'static str,
}

/// Complete operation inventory for [`MODEL_PROVIDER_INTERFACE_ID`].
///
/// Baseline providers must implement every [`ProviderOperationRequirement::Required`] operation.
/// Capability-gated operations must be implemented when the corresponding capability is
/// advertised. Optional operations are extensions and callers must tolerate an
/// `unsupported_operation` service error.
pub const MODEL_PROVIDER_OPERATIONS: &[ProviderOperationContract] = &[
    ProviderOperationContract {
        operation: OP_CAPABILITIES,
        request_type: "()",
        response_type: "ProviderCapabilities",
        requirement: ProviderOperationRequirement::Required,
        behavior: "return stable provider identity and truthful capability declarations",
    },
    ProviderOperationContract {
        operation: OP_MODELS,
        request_type: "ModelListRequest",
        response_type: "ModelList",
        requirement: ProviderOperationRequirement::Required,
        behavior: "return unique model ids and truthful per-model capabilities",
    },
    ProviderOperationContract {
        operation: OP_VALIDATE_CONFIG,
        request_type: "ValidateConfigRequest",
        response_type: "ValidateConfigResponse",
        requirement: ProviderOperationRequirement::Required,
        behavior: "report invalid configuration as typed validation data without panicking",
    },
    ProviderOperationContract {
        operation: OP_START_TURN,
        request_type: "ModelTurnRequest",
        response_type: "StartTurnResponse",
        requirement: ProviderOperationRequirement::Required,
        behavior: "allocate a unique active turn and enqueue TurnStarted before content",
    },
    ProviderOperationContract {
        operation: OP_POLL_TURN_EVENTS,
        request_type: "PollTurnEventsRequest",
        response_type: "PollTurnEventsResponse",
        requirement: ProviderOperationRequirement::Required,
        behavior: "drain ordered normalized events without replaying previously drained events",
    },
    ProviderOperationContract {
        operation: OP_CANCEL_TURN,
        request_type: "CancelTurnRequest",
        response_type: "AckResponse",
        requirement: ProviderOperationRequirement::Required,
        behavior: "idempotently request cancellation; completed-turn races remain valid",
    },
    ProviderOperationContract {
        operation: OP_FINISH_TURN,
        request_type: "FinishTurnRequest",
        response_type: "AckResponse",
        requirement: ProviderOperationRequirement::Required,
        behavior: "idempotently release all host-visible state for the provider turn id",
    },
    ProviderOperationContract {
        operation: OP_CONTEXT_MANAGEMENT_CAPABILITIES,
        request_type: "ContextManagementCapabilitiesRequest",
        response_type: "ContextManagementCapabilities",
        requirement: ProviderOperationRequirement::Optional,
        behavior: "describe context behavior for the active provider surface",
    },
    ProviderOperationContract {
        operation: OP_COMPACT_CONTEXT,
        request_type: "CompactContextRequest",
        response_type: "CompactContextResponse",
        requirement: ProviderOperationRequirement::CapabilityGated(
            ProviderCapability::NativeContextCompaction,
        ),
        behavior: "return lossless replayable replacement context in its declared format",
    },
    ProviderOperationContract {
        operation: OP_VERIFY_MODEL,
        request_type: "VerifyModelRequest",
        response_type: "VerifyModelResponse",
        requirement: ProviderOperationRequirement::Optional,
        behavior: "perform a bounded model probe and normalize the outcome",
    },
    ProviderOperationContract {
        operation: OP_NATIVE_WEB_SEARCH,
        request_type: "NativeWebSearchRequest",
        response_type: "NativeWebSearchResponse",
        requirement: ProviderOperationRequirement::CapabilityGated(
            ProviderCapability::NativeWebSearch,
        ),
        behavior: "return normalized search results or a typed partial result",
    },
    ProviderOperationContract {
        operation: OP_AUTH_USAGE,
        request_type: "AuthUsageRequest",
        response_type: "AuthUsageResponse",
        requirement: ProviderOperationRequirement::Optional,
        behavior: "return normalized auth-backed usage snapshots",
    },
    ProviderOperationContract {
        operation: OP_AUTH_PRIME,
        request_type: "AuthPrimeRequest",
        response_type: "AuthPrimeResponse",
        requirement: ProviderOperationRequirement::Optional,
        behavior: "prime provider auth state without starting a model turn",
    },
    ProviderOperationContract {
        operation: OP_AUTH_RESET_CREDITS,
        request_type: "AuthResetCreditsRequest",
        response_type: "AuthResetCreditsResponse",
        requirement: ProviderOperationRequirement::Optional,
        behavior: "list normalized auth reset-credit state",
    },
    ProviderOperationContract {
        operation: OP_AUTH_RESET_CREDIT_CONSUME,
        request_type: "AuthResetCreditConsumeRequest",
        response_type: "AuthResetCreditConsumeResponse",
        requirement: ProviderOperationRequirement::Optional,
        behavior: "consume one reset credit with typed outcome data",
    },
];

/// Look up the contract for one model-provider operation.
#[must_use]
pub fn provider_operation_contract(operation: &str) -> Option<&'static ProviderOperationContract> {
    MODEL_PROVIDER_OPERATIONS
        .iter()
        .find(|contract| contract.operation == operation)
}

/// Provider-level capability report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub provider_id: String,
    pub display_name: String,
    #[serde(default)]
    pub capabilities: BTreeSet<ProviderCapability>,
    /// Granular provider-surface claims. Missing claims are unknown and never guaranteed.
    #[serde(default)]
    pub feature_support: ModelFeatureSupport,
    /// Provider-supported auth scheme identifiers, for example `api_key` or `chatgpt`.
    #[serde(default)]
    pub auth_schemes: BTreeSet<String>,
    /// Provider-supplied default retry rules for known ephemeral errors.
    #[serde(default)]
    pub retry_rules: Vec<ProviderRetryRule>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Provider-supplied or user-configured provider error retry rule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "retry_rule")]
pub struct ProviderRetryRule {
    /// Stable retry rule identifier.
    pub id: String,
    /// Enable this retry rule. `None` inherits from provider defaults or final defaults.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Exact provider plugin id scope.
    #[serde(default)]
    pub provider_plugin_id: Option<String>,
    /// Provider plugin id substring scope.
    #[serde(default)]
    pub provider_plugin_id_contains: Option<String>,
    /// Exact model id scope.
    #[serde(default)]
    pub model_id: Option<String>,
    /// Model id substring scope.
    #[serde(default)]
    pub model_id_contains: Option<String>,
    /// Maximum retry attempts when this rule matches.
    #[serde(default)]
    pub max_retries: Option<u8>,
    /// Initial retry delay in milliseconds.
    #[serde(default)]
    pub initial_delay_ms: Option<u64>,
    /// Maximum retry delay in milliseconds.
    #[serde(default)]
    pub max_delay_ms: Option<u64>,
    /// Use provider retry hints when present.
    #[serde(default)]
    pub use_provider_retry_hint: Option<bool>,
    /// Error match conditions.
    #[config_doc(nested)]
    #[serde(default)]
    pub r#match: ProviderRetryRuleMatch,
}

/// Provider error retry rule match conditions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "retry_rule_match")]
pub struct ProviderRetryRuleMatch {
    /// Provider error category to match.
    #[serde(default)]
    pub category: Option<ProviderErrorCategory>,
    /// Provider error code to match exactly.
    #[serde(default)]
    pub code: Option<String>,
    /// Provider error message to match exactly.
    #[serde(default)]
    pub message_equals: Option<String>,
    /// Provider error message substring to match.
    #[serde(default)]
    pub message_contains: Option<String>,
    /// Provider-native error message to match exactly.
    #[serde(default)]
    pub provider_message_equals: Option<String>,
    /// Provider-native error message substring to match.
    #[serde(default)]
    pub provider_message_contains: Option<String>,
}

impl ProviderRetryRuleMatch {
    /// Return whether this matcher has at least one configured condition.
    #[must_use]
    pub const fn has_conditions(&self) -> bool {
        self.category.is_some()
            || self.code.is_some()
            || self.message_equals.is_some()
            || self.message_contains.is_some()
            || self.provider_message_equals.is_some()
            || self.provider_message_contains.is_some()
    }
}

impl ProviderRetryRule {
    /// Deep-merge another rule with field-level override precedence.
    pub fn merge_override(&mut self, override_rule: Self) {
        if override_rule.enabled.is_some() {
            self.enabled = override_rule.enabled;
        }
        if override_rule.provider_plugin_id.is_some() {
            self.provider_plugin_id = override_rule.provider_plugin_id;
        }
        if override_rule.provider_plugin_id_contains.is_some() {
            self.provider_plugin_id_contains = override_rule.provider_plugin_id_contains;
        }
        if override_rule.model_id.is_some() {
            self.model_id = override_rule.model_id;
        }
        if override_rule.model_id_contains.is_some() {
            self.model_id_contains = override_rule.model_id_contains;
        }
        if override_rule.max_retries.is_some() {
            self.max_retries = override_rule.max_retries;
        }
        if override_rule.initial_delay_ms.is_some() {
            self.initial_delay_ms = override_rule.initial_delay_ms;
        }
        if override_rule.max_delay_ms.is_some() {
            self.max_delay_ms = override_rule.max_delay_ms;
        }
        if override_rule.use_provider_retry_hint.is_some() {
            self.use_provider_retry_hint = override_rule.use_provider_retry_hint;
        }
        self.r#match.merge_override(override_rule.r#match);
    }
}

impl ProviderRetryRuleMatch {
    /// Deep-merge another matcher with field-level override precedence.
    pub fn merge_override(&mut self, override_match: Self) {
        if override_match.category.is_some() {
            self.category = override_match.category;
        }
        if override_match.code.is_some() {
            self.code = override_match.code;
        }
        if override_match.message_equals.is_some() {
            self.message_equals = override_match.message_equals;
        }
        if override_match.message_contains.is_some() {
            self.message_contains = override_match.message_contains;
        }
        if override_match.provider_message_equals.is_some() {
            self.provider_message_equals = override_match.provider_message_equals;
        }
        if override_match.provider_message_contains.is_some() {
            self.provider_message_contains = override_match.provider_message_contains;
        }
    }
}

/// Provenance for a granular provider/model capability claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySource {
    /// Reported directly by the provider API for the selected model/surface.
    ProviderApi,
    /// Maintained in Bcode's bundled compatibility catalog.
    BundledCatalog,
    /// Explicit application or user configuration.
    Configuration,
    /// Learned from a bounded compatibility probe or persisted incompatibility observation.
    Probe,
    /// Deterministic local test provider contract.
    TestContract,
}

/// One truthful support claim with its provenance.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CapabilitySupport {
    /// No trustworthy claim is available. This must never be presented as guaranteed support.
    #[default]
    Unknown,
    /// The feature is supported according to the stated source.
    Supported { source: CapabilitySource },
    /// The feature is unsupported according to the stated source.
    Unsupported {
        source: CapabilitySource,
        reason: String,
    },
}

impl CapabilitySupport {
    /// Return whether this is an affirmative, provenance-bearing support claim.
    #[must_use]
    pub const fn is_guaranteed(&self) -> bool {
        matches!(self, Self::Supported { .. })
    }

    /// Return this claim's provenance when known.
    #[must_use]
    pub const fn source(&self) -> Option<CapabilitySource> {
        match self {
            Self::Unknown => None,
            Self::Supported { source } | Self::Unsupported { source, .. } => Some(*source),
        }
    }
}

/// Scope at which granular feature negotiation was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityScope {
    Provider,
    Model,
}

/// Result of intersecting provider-surface and selected-model claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum NegotiatedFeatureSupport {
    /// Both scopes affirm support and preserve their independent provenance.
    Guaranteed {
        provider_source: CapabilitySource,
        model_source: CapabilitySource,
    },
    /// One scope explicitly rejects the feature.
    Unsupported {
        scope: CapabilityScope,
        source: CapabilitySource,
        reason: String,
    },
    /// One scope has no trustworthy claim, so support cannot be guaranteed.
    Unknown { scope: CapabilityScope },
}

impl NegotiatedFeatureSupport {
    /// Return whether both provider and model affirm support.
    #[must_use]
    pub const fn is_guaranteed(&self) -> bool {
        matches!(self, Self::Guaranteed { .. })
    }
}

/// Provider-neutral model parameter keys used in capability negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelParameterKey {
    Temperature,
    MaxOutputTokens,
    TopP,
    StopSequences,
    ReasoningBudgetTokens,
    ReasoningEffort,
    ReasoningEffortValue,
    ReasoningSummary,
}

/// Structured-output modes negotiated independently from broad JSON capability flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredOutputMode {
    JsonSchema,
    StrictJsonSchema,
}

/// Tool selection modes negotiated independently from basic tool transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoiceMode {
    Auto,
    None,
    Required,
    Named,
    Parallel,
}

/// Prompt-cache hint families negotiated independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheFeature {
    ConversationPrefix,
    ExplicitSystem,
    ExplicitTools,
    ExplicitMessage,
    Ttl,
}

/// Media-input families negotiated independently from text input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaInputFeature {
    UserImage,
    SystemImage,
    AssistantImage,
    ToolMessageImage,
    ImageReference,
    ToolResultImage,
}

/// Granular provider/model feature claims.
///
/// Absent entries are [`CapabilitySupport::Unknown`]. Callers must require affirmative claims from
/// both the provider surface and selected model before presenting behavior as guaranteed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFeatureSupport {
    #[serde(default)]
    pub parameters: BTreeMap<ModelParameterKey, CapabilitySupport>,
    #[serde(default)]
    pub structured_output: BTreeMap<StructuredOutputMode, CapabilitySupport>,
    #[serde(default)]
    pub tool_choice: BTreeMap<ToolChoiceMode, CapabilitySupport>,
    #[serde(default)]
    pub prompt_cache: BTreeMap<PromptCacheFeature, CapabilitySupport>,
    #[serde(default)]
    pub media_input: BTreeMap<MediaInputFeature, CapabilitySupport>,
}

impl ModelFeatureSupport {
    /// Return the parameter claim, defaulting to unknown.
    #[must_use]
    pub fn parameter(&self, parameter: ModelParameterKey) -> &CapabilitySupport {
        self.parameters
            .get(&parameter)
            .unwrap_or(&CapabilitySupport::Unknown)
    }

    /// Return the structured-output claim, defaulting to unknown.
    #[must_use]
    pub fn structured_output(&self, mode: StructuredOutputMode) -> &CapabilitySupport {
        self.structured_output
            .get(&mode)
            .unwrap_or(&CapabilitySupport::Unknown)
    }

    /// Return the tool-choice claim, defaulting to unknown.
    #[must_use]
    pub fn tool_choice(&self, mode: ToolChoiceMode) -> &CapabilitySupport {
        self.tool_choice
            .get(&mode)
            .unwrap_or(&CapabilitySupport::Unknown)
    }

    /// Return the prompt-cache claim, defaulting to unknown.
    #[must_use]
    pub fn prompt_cache(&self, feature: PromptCacheFeature) -> &CapabilitySupport {
        self.prompt_cache
            .get(&feature)
            .unwrap_or(&CapabilitySupport::Unknown)
    }

    /// Return the media-input claim, defaulting to unknown.
    #[must_use]
    pub fn media_input(&self, feature: MediaInputFeature) -> &CapabilitySupport {
        self.media_input
            .get(&feature)
            .unwrap_or(&CapabilitySupport::Unknown)
    }
    /// Return whether every currently defined granular feature has an explicit claim.
    ///
    /// This is useful for provider-surface reports. Model reports may intentionally remain sparse
    /// when model-specific evidence is unavailable.
    #[must_use]
    pub fn has_complete_inventory(&self) -> bool {
        [
            ModelParameterKey::Temperature,
            ModelParameterKey::MaxOutputTokens,
            ModelParameterKey::TopP,
            ModelParameterKey::StopSequences,
            ModelParameterKey::ReasoningBudgetTokens,
            ModelParameterKey::ReasoningEffort,
            ModelParameterKey::ReasoningEffortValue,
            ModelParameterKey::ReasoningSummary,
        ]
        .into_iter()
        .all(|key| self.parameters.contains_key(&key))
            && [
                StructuredOutputMode::JsonSchema,
                StructuredOutputMode::StrictJsonSchema,
            ]
            .into_iter()
            .all(|mode| self.structured_output.contains_key(&mode))
            && [
                ToolChoiceMode::Auto,
                ToolChoiceMode::None,
                ToolChoiceMode::Required,
                ToolChoiceMode::Named,
                ToolChoiceMode::Parallel,
            ]
            .into_iter()
            .all(|mode| self.tool_choice.contains_key(&mode))
            && [
                PromptCacheFeature::ConversationPrefix,
                PromptCacheFeature::ExplicitSystem,
                PromptCacheFeature::ExplicitTools,
                PromptCacheFeature::ExplicitMessage,
                PromptCacheFeature::Ttl,
            ]
            .into_iter()
            .all(|feature| self.prompt_cache.contains_key(&feature))
            && [
                MediaInputFeature::UserImage,
                MediaInputFeature::SystemImage,
                MediaInputFeature::AssistantImage,
                MediaInputFeature::ToolMessageImage,
                MediaInputFeature::ImageReference,
                MediaInputFeature::ToolResultImage,
            ]
            .into_iter()
            .all(|feature| self.media_input.contains_key(&feature))
    }

    /// Intersect provider-surface and selected-model claims for one requested feature.
    #[must_use]
    pub fn negotiate(
        &self,
        model: &Self,
        feature: RequestedModelFeature,
    ) -> NegotiatedFeatureSupport {
        negotiate_feature_claims(feature.support_in(self), feature.support_in(model))
    }
}

fn negotiate_feature_claims(
    provider: &CapabilitySupport,
    model: &CapabilitySupport,
) -> NegotiatedFeatureSupport {
    if let CapabilitySupport::Unsupported { source, reason } = provider {
        return NegotiatedFeatureSupport::Unsupported {
            scope: CapabilityScope::Provider,
            source: *source,
            reason: reason.clone(),
        };
    }
    if matches!(provider, CapabilitySupport::Unknown) {
        return NegotiatedFeatureSupport::Unknown {
            scope: CapabilityScope::Provider,
        };
    }
    if let CapabilitySupport::Unsupported { source, reason } = model {
        return NegotiatedFeatureSupport::Unsupported {
            scope: CapabilityScope::Model,
            source: *source,
            reason: reason.clone(),
        };
    }
    if matches!(model, CapabilitySupport::Unknown) {
        return NegotiatedFeatureSupport::Unknown {
            scope: CapabilityScope::Model,
        };
    }
    NegotiatedFeatureSupport::Guaranteed {
        provider_source: provider.source().expect("provider support source"),
        model_source: model.source().expect("model support source"),
    }
}

/// Provider-level capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCapability {
    Streaming,
    Tools,
    /// Provider transport can request multiple tool calls in one model response.
    ParallelToolCalls,
    Cancellation,
    JsonMode,
    PromptCaching,
    ConversationReuse,
    /// Provider can manage context compaction without a host-authored summary.
    ProviderManagedContext,
    /// Provider exposes an explicit native context-compaction operation.
    NativeContextCompaction,
    NativeWebSearch,
    CodeSearch,
}

/// Model listing request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelListRequest {
    #[serde(default)]
    pub provider_context: ProviderRequestContext,
    #[serde(default)]
    pub selected_model_id: Option<String>,
}

/// Authority represented by a provider model listing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelListAuthority {
    /// User configuration explicitly controls membership.
    Explicit,
    /// Provider discovery authoritatively controls membership.
    Authoritative,
    /// Provider discovery may be supplemented by catalog data.
    Partial,
    /// Provider returned fallback candidates only.
    #[default]
    Fallback,
}

/// Provider-neutral catalog resolution policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ModelCatalogPolicy {
    /// Provider has no catalog mapping.
    #[default]
    Unmapped,
    /// Enrich existing models without expanding membership.
    EnrichOnly {
        provider_id: String,
        target: Option<ModelCatalogSupportHint>,
        authority: ModelListAuthority,
    },
    /// Expand with models matching a support target.
    ExpandSupported {
        provider_id: String,
        target: ModelCatalogSupportHint,
        authority: ModelListAuthority,
    },
    /// Expand with every model in the provider catalog.
    ExpandAll { provider_id: String },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum HumanReadableModelCatalogPolicy {
    Unmapped,
    EnrichOnly {
        provider_id: String,
        #[serde(default)]
        target: Option<ModelCatalogSupportHint>,
        authority: ModelListAuthority,
    },
    ExpandSupported {
        provider_id: String,
        target: ModelCatalogSupportHint,
        authority: ModelListAuthority,
    },
    ExpandAll {
        provider_id: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireModelCatalogPolicy {
    Unmapped,
    EnrichOnly {
        provider_id: String,
        target: Option<ModelCatalogSupportHint>,
        authority: ModelListAuthority,
    },
    ExpandSupported {
        provider_id: String,
        target: ModelCatalogSupportHint,
        authority: ModelListAuthority,
    },
    ExpandAll {
        provider_id: String,
    },
}

impl From<&ModelCatalogPolicy> for HumanReadableModelCatalogPolicy {
    fn from(policy: &ModelCatalogPolicy) -> Self {
        match policy {
            ModelCatalogPolicy::Unmapped => Self::Unmapped,
            ModelCatalogPolicy::EnrichOnly {
                provider_id,
                target,
                authority,
            } => Self::EnrichOnly {
                provider_id: provider_id.clone(),
                target: target.clone(),
                authority: *authority,
            },
            ModelCatalogPolicy::ExpandSupported {
                provider_id,
                target,
                authority,
            } => Self::ExpandSupported {
                provider_id: provider_id.clone(),
                target: target.clone(),
                authority: *authority,
            },
            ModelCatalogPolicy::ExpandAll { provider_id } => Self::ExpandAll {
                provider_id: provider_id.clone(),
            },
        }
    }
}

impl From<&ModelCatalogPolicy> for WireModelCatalogPolicy {
    fn from(policy: &ModelCatalogPolicy) -> Self {
        match policy {
            ModelCatalogPolicy::Unmapped => Self::Unmapped,
            ModelCatalogPolicy::EnrichOnly {
                provider_id,
                target,
                authority,
            } => Self::EnrichOnly {
                provider_id: provider_id.clone(),
                target: target.clone(),
                authority: *authority,
            },
            ModelCatalogPolicy::ExpandSupported {
                provider_id,
                target,
                authority,
            } => Self::ExpandSupported {
                provider_id: provider_id.clone(),
                target: target.clone(),
                authority: *authority,
            },
            ModelCatalogPolicy::ExpandAll { provider_id } => Self::ExpandAll {
                provider_id: provider_id.clone(),
            },
        }
    }
}

macro_rules! impl_model_catalog_policy_from_helper {
    ($helper:ident) => {
        impl From<$helper> for ModelCatalogPolicy {
            fn from(policy: $helper) -> Self {
                match policy {
                    $helper::Unmapped => Self::Unmapped,
                    $helper::EnrichOnly {
                        provider_id,
                        target,
                        authority,
                    } => Self::EnrichOnly {
                        provider_id,
                        target,
                        authority,
                    },
                    $helper::ExpandSupported {
                        provider_id,
                        target,
                        authority,
                    } => Self::ExpandSupported {
                        provider_id,
                        target,
                        authority,
                    },
                    $helper::ExpandAll { provider_id } => Self::ExpandAll { provider_id },
                }
            }
        }
    };
}

impl_model_catalog_policy_from_helper!(HumanReadableModelCatalogPolicy);
impl_model_catalog_policy_from_helper!(WireModelCatalogPolicy);

impl Serialize for ModelCatalogPolicy {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if serializer.is_human_readable() {
            HumanReadableModelCatalogPolicy::from(self).serialize(serializer)
        } else {
            WireModelCatalogPolicy::from(self).serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for ModelCatalogPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            HumanReadableModelCatalogPolicy::deserialize(deserializer).map(Self::from)
        } else {
            WireModelCatalogPolicy::deserialize(deserializer).map(Self::from)
        }
    }
}

/// Provider-neutral catalog support target.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalogSupportHint {
    pub provider: String,
    pub auth_mode: String,
    pub api_surface: String,
    #[serde(default)]
    pub integration: Option<String>,
}

/// Provider hints consumed by the host catalog resolver.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalogHints {
    #[serde(default)]
    pub policy: ModelCatalogPolicy,
}

/// Model listing response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelList {
    pub models: Vec<ModelInfo>,
    #[serde(default)]
    pub catalog: ModelCatalogHints,
}

/// Model metadata exposed by a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub model_id: String,
    pub display_name: String,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub capabilities: BTreeSet<ModelCapability>,
    /// Granular model claims. Missing claims are unknown and never guaranteed.
    #[serde(default)]
    pub feature_support: ModelFeatureSupport,
    #[serde(default)]
    pub reasoning: Option<ModelReasoningInfo>,
    #[serde(default)]
    pub cache: ModelCacheInfo,
    #[serde(default)]
    pub metadata_source: Option<ModelMetadataSource>,
    #[serde(default)]
    pub pricing: Option<ModelPricingInfo>,
    #[serde(default, skip)]
    pub visibility: ModelVisibility,
}

/// Model picker/list visibility metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelVisibility {
    #[default]
    Visible,
    Ignored {
        source: ModelVisibilitySource,
        rule: String,
    },
    Unsupported {
        reason: String,
    },
}

/// Source of a model visibility decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelVisibilitySource {
    Config,
    State,
    Both,
}

/// Model token pricing metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPricingInfo {
    /// ISO 4217 currency code, for example `USD`.
    pub currency: String,
    /// Pricing unit for each token bucket.
    pub unit: ModelPricingUnit,
    /// Price for uncached input tokens.
    #[serde(default)]
    pub input: Option<ModelTokenPrice>,
    /// Price for cached input tokens.
    #[serde(default)]
    pub cached_input: Option<ModelTokenPrice>,
    /// Price for input tokens written to an explicit prompt cache.
    #[serde(default)]
    pub cache_write_input: Option<ModelTokenPrice>,
    /// Price for generated output tokens.
    #[serde(default)]
    pub output: Option<ModelTokenPrice>,
    /// Price source.
    pub source: ModelPricingSource,
}

/// Model pricing unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPricingUnit {
    /// Prices are expressed per one million tokens.
    PerMillionTokens,
}

/// Price for a token bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTokenPrice {
    /// Price in micros of the pricing currency.
    pub micros: u64,
}

impl ModelTokenPrice {
    /// Construct a token price from currency micros.
    #[must_use]
    pub const fn from_micros(micros: u64) -> Self {
        Self { micros }
    }
}

/// Source used by a provider to resolve model pricing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPricingSource {
    UserOverride,
    ProviderApi,
    RemoteCatalog,
    BundledCatalog,
    PatternMatch,
    Unknown,
}

/// Estimated model-call cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCostEstimate {
    /// ISO 4217 currency code.
    pub currency: String,
    /// Total estimated cost in micros of the pricing currency.
    pub total_micros: u64,
    /// Price metadata source used for the estimate.
    pub source: ModelPricingSource,
}

impl ModelPricingInfo {
    /// Estimate cost for provider-reported token usage.
    /// Returns `Some` only when the provider reported complete input/output usage and every
    /// non-zero separately priced cache bucket has corresponding pricing. Legitimate zero-cost or
    /// sub-micro estimates remain `Some(0)`. Returns `None` rather than silently producing a
    /// partial total when usage or pricing coverage is incomplete.
    #[must_use]
    pub fn estimate_cost(&self, usage: &TokenUsage) -> Option<ModelCostEstimate> {
        let input = usage.input_tokens?;
        let output = usage.output_tokens?;
        let cached = usage.cached_input_tokens.unwrap_or_default();
        let cache_write = usage.cache_write_input_tokens.unwrap_or_default();
        if self.cached_input.is_some() && usage.cached_input_tokens.is_none()
            || self.cache_write_input.is_some() && usage.cache_write_input_tokens.is_none()
        {
            return None;
        }
        let uncached_input = input.saturating_sub(cached);
        if uncached_input > 0 && self.input.is_none()
            || cached > 0 && self.cached_input.is_none()
            || cache_write > 0 && self.cache_write_input.is_none()
            || output > 0 && self.output.is_none()
        {
            return None;
        }
        let mut total_micros = 0_u64;
        total_micros = total_micros.saturating_add(price_bucket_micros(uncached_input, self.input));
        total_micros = total_micros.saturating_add(price_bucket_micros(cached, self.cached_input));
        total_micros =
            total_micros.saturating_add(price_bucket_micros(cache_write, self.cache_write_input));
        total_micros = total_micros.saturating_add(price_bucket_micros(output, self.output));
        Some(ModelCostEstimate {
            currency: self.currency.clone(),
            total_micros,
            source: self.source,
        })
    }
}

fn price_bucket_micros(tokens: u32, price: Option<ModelTokenPrice>) -> u64 {
    price.map_or(0, |price| {
        u64::from(tokens).saturating_mul(price.micros) / 1_000_000
    })
}

/// Source used by a provider to resolve model metadata such as token limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelMetadataSource {
    ConfigOverride,
    ProviderApi,
    /// Remote model catalog.
    RemoteCatalog,
    /// Catalog embedded in the Bcode binary.
    BundledCatalog,
    PatternMatch,
    ProviderDefault,
    ProviderLive,
    Unknown,
}

/// Per-model/provider cache and continuation capabilities.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCacheInfo {
    #[serde(default)]
    pub capabilities: BTreeSet<ModelCacheCapability>,
}

/// Provider cache/continuation capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCacheCapability {
    PromptCacheKey,
    AutomaticPrefixCache,
    ExplicitCachePoints,
    CacheUsageReporting,
    PreviousResponseId,
}

/// Source of model reasoning capability metadata.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelReasoningCapabilitySource {
    /// No source was provided.
    #[default]
    Unknown,
    /// User or administrator configuration override.
    ConfigOverride,
    /// Provider model metadata explicitly declared the values.
    ProviderMetadata,
    /// Bcode inferred values from a maintained provider/model compatibility table.
    KnownModelTable,
    /// Generic compatibility fallback; the provider may reject unsupported values.
    GenericFallback,
}

/// Per-model reasoning/thinking controls exposed by a provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelReasoningInfo {
    /// Provider-native effort values accepted by the model.
    #[serde(default)]
    pub effort_values: Vec<String>,
    /// Provider-native default effort value, when known.
    #[serde(default)]
    pub default_effort: Option<String>,
    /// Whether provider-visible reasoning summaries can be requested.
    #[serde(default)]
    pub visible_summary_supported: bool,
    /// Provider-native summary/detail values accepted by the model.
    #[serde(default)]
    pub summary_values: Vec<String>,
    /// Provider-native default summary/detail value, when known.
    #[serde(default)]
    pub default_summary: Option<String>,
    /// Whether raw provider reasoning text can be requested.
    #[serde(default)]
    pub raw_reasoning_supported: bool,
    /// Source of this capability metadata.
    #[serde(default)]
    pub source: ModelReasoningCapabilitySource,
}

/// Per-model capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    StreamingText,
    ToolCalls,
    ParallelToolCalls,
    JsonMode,
    Reasoning,
    ImageInput,
    PromptCaching,
    NativeWebSearch,
    CodeSearch,
}

/// User-facing thinking / reasoning effort level for models that support it.
/// Maps to provider-specific parameters (e.g. `reasoning_effort` or budget).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

/// Provider configuration validation request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidateConfigRequest {
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub config: BTreeMap<String, String>,
    /// Provider context to validate, including transient auth and endpoint settings.
    #[serde(default)]
    pub provider_context: ProviderRequestContext,
}

/// Binary-codec-safe provider-native request option value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderRequestValue {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Self>),
    Object(BTreeMap<String, Self>),
}

impl From<serde_json::Value> for ProviderRequestValue {
    fn from(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(value) => Self::Bool(value),
            serde_json::Value::Number(value) => Self::Number(value.to_string()),
            serde_json::Value::String(value) => Self::String(value),
            serde_json::Value::Array(values) => {
                Self::Array(values.into_iter().map(Self::from).collect())
            }
            serde_json::Value::Object(values) => Self::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (key, Self::from(value)))
                    .collect(),
            ),
        }
    }
}

impl From<ProviderRequestValue> for serde_json::Value {
    fn from(value: ProviderRequestValue) -> Self {
        match value {
            ProviderRequestValue::Null => Self::Null,
            ProviderRequestValue::Bool(value) => Self::Bool(value),
            ProviderRequestValue::Number(value) => value
                .parse::<serde_json::Number>()
                .map_or_else(|_| Self::String(value), Self::Number),
            ProviderRequestValue::String(value) => Self::String(value),
            ProviderRequestValue::Array(values) => {
                Self::Array(values.into_iter().map(Self::from).collect())
            }
            ProviderRequestValue::Object(values) => Self::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (key, Self::from(value)))
                    .collect(),
            ),
        }
    }
}

/// Auth/security diagnostic surfaced while resolving provider credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAuthDiagnostic {
    /// Severity level, for example `info`, `warning`, or `error`.
    pub severity: String,
    /// Stable diagnostic code.
    pub code: String,
    /// Human-readable diagnostic message.
    pub message: String,
    /// Optional remediation guidance.
    #[serde(default)]
    pub remediation: Option<String>,
}

/// Provider-neutral authentication material resolved by the client from the selected auth profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAuthContext {
    /// Auth profile ID selected by model/profile resolution.
    #[serde(default)]
    pub profile: Option<String>,
    /// Auth backend used to materialize the credentials, for example `sshenv`.
    #[serde(default)]
    pub backend: Option<String>,
    /// Provider/plugin-specific auth scheme, for example `api_key` or `chatgpt`.
    #[serde(default)]
    pub scheme: Option<String>,
    /// Canonical secret names to secret values, for example `api_key`.
    #[serde(default)]
    pub credentials: BTreeMap<String, ProviderAuthCredential>,
    /// Non-secret auth attributes, for example region/profile/base URL hints.
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
    /// Optional persistence references for credentials that can be refreshed/updated.
    #[serde(default)]
    pub storage: BTreeMap<String, ProviderAuthStorageRef>,
    /// Non-secret diagnostics produced while materializing auth.
    #[serde(default)]
    pub diagnostics: Vec<ProviderAuthDiagnostic>,
}

/// Secret credential value supplied to a provider plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAuthCredential {
    pub value: String,
    #[serde(default)]
    pub source: Option<String>,
}

/// Location where a credential can be updated after refresh/login.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAuthStorageRef {
    pub backend: String,
    pub profile: String,
    pub key: String,
    #[serde(default)]
    pub vault: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAuthCandidate {
    /// Auth profile name for this candidate.
    #[serde(default)]
    pub profile: Option<String>,
    /// Semantic auth material for this candidate.
    #[serde(default)]
    pub auth: ProviderAuthContext,
    /// Compatibility environment values scoped to this candidate.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Auth pool routing settings resolved from declarative configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAuthPoolRouting {
    /// Strategy used after pre-routing modifiers such as priming are satisfied.
    #[serde(default)]
    pub strategy: Option<String>,
    /// Whether to route once to unprimed profiles before normal strategy selection.
    #[serde(default)]
    pub priming_enabled: bool,
    /// Whether priming should include the primary selected auth profile.
    #[serde(default)]
    pub priming_include_primary: bool,
    /// Optional duration after which local fallback priming should be attempted again.
    #[serde(default)]
    pub priming_reprime_after: Option<String>,
    /// Whether provider-confirmed usage windows should drive priming when available.
    #[serde(default)]
    pub priming_provider_windows: bool,
    /// Local fallback duration used when provider usage windows are unavailable.
    #[serde(default)]
    pub priming_fallback_reprime_after: Option<String>,
    /// Required provider usage windows grouped by meter id.
    #[serde(default)]
    pub priming_required_windows: BTreeMap<String, Vec<String>>,
}

/// Request for provider-confirmed auth usage window discovery.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthUsageRequest {
    /// Provider context identifying the auth profile to inspect.
    #[serde(default)]
    pub provider_context: ProviderRequestContext,
    /// Optional usage meter ids the caller is interested in.
    #[serde(default)]
    pub meter_ids: Vec<String>,
}

/// Provider-confirmed auth usage window discovery response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthUsageResponse {
    /// Whether this provider/auth mode supports usage window discovery.
    #[serde(default)]
    pub supported: bool,
    /// Optional degraded-state explanation when discovery could not produce complete data.
    #[serde(default)]
    pub degraded_reason: Option<String>,
    /// Optional debug metadata useful for provider integration diagnostics.
    #[serde(default)]
    pub debug: BTreeMap<String, String>,
    /// Optional provider capability metadata for this usage response.
    #[serde(default)]
    pub capabilities: AuthUsageCapabilities,
    /// Usage meters returned by the provider.
    #[serde(default)]
    pub meters: Vec<AuthUsageMeterSnapshot>,
    /// Summary of banked rate-limit reset credits returned with usage, when available.
    #[serde(default)]
    pub reset_credits: Option<AuthResetCreditsSummary>,
}

/// Provider capability metadata for auth usage discovery.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthUsageCapabilities {
    /// Provider-supported usage capability flags.
    #[serde(default)]
    pub features: BTreeSet<AuthUsageCapability>,
}

/// Individual provider capability for auth usage discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthUsageCapability {
    /// The provider can fetch fresh usage from an upstream service.
    Refresh,
    /// Returned windows may include reset timestamps.
    WindowReset,
    /// Returned windows may include percent usage.
    UsedPercent,
    /// Returned windows may include absolute usage/limit amounts.
    AbsoluteAmounts,
    /// The provider also supports explicit auth priming.
    Priming,
    /// The provider can report banked rate-limit reset credits.
    ResetCredits,
}

/// Provider usage meter containing one or more windows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthUsageMeterSnapshot {
    /// Stable provider meter id, for example `codex` or `tokens`.
    pub meter_id: String,
    /// Human-readable meter name.
    #[serde(default)]
    pub meter_name: Option<String>,
    /// Provider windows for this meter.
    #[serde(default)]
    pub windows: Vec<AuthUsageWindowSnapshot>,
}

/// Provider-confirmed usage state for one metering window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthUsageWindowSnapshot {
    /// Stable window id within the meter, for example `primary`, `secondary`, `5h`, or `7d`.
    pub window_id: String,
    /// Window length in seconds when known.
    #[serde(default)]
    pub window_duration_secs: Option<u64>,
    /// Provider reset timestamp in Unix seconds when known.
    #[serde(default)]
    pub resets_at_unix: Option<u64>,
    /// Usage percentage when known. Integer to preserve provider-rounded values exactly.
    #[serde(default)]
    pub used_percent: Option<u32>,
    /// Absolute usage amount when known.
    #[serde(default)]
    pub used_amount: Option<u64>,
    /// Absolute limit amount when known.
    #[serde(default)]
    pub limit_amount: Option<u64>,
    /// Local observation timestamp in Unix seconds.
    #[serde(default)]
    pub observed_at_unix: u64,
    /// Provider/source identifier for this observation.
    #[serde(default)]
    pub source: Option<String>,
}

/// Summary of banked rate-limit reset credits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthResetCreditsSummary {
    /// Number of reset credits currently available.
    #[serde(default)]
    pub available_count: u32,
}

/// Request for listing provider auth rate-limit reset credits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthResetCreditsRequest {
    /// Provider context identifying the auth profile to inspect.
    #[serde(default)]
    pub provider_context: ProviderRequestContext,
}

/// Provider auth rate-limit reset credit listing response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthResetCreditsResponse {
    /// Whether this provider/auth mode supports reset credits.
    #[serde(default)]
    pub supported: bool,
    /// Optional degraded-state explanation when discovery could not produce complete data.
    #[serde(default)]
    pub degraded_reason: Option<String>,
    /// Number of reset credits currently available.
    #[serde(default)]
    pub available_count: u32,
    /// Detailed reset credit rows when available.
    #[serde(default)]
    pub credits: Vec<AuthResetCreditSnapshot>,
    /// Optional debug metadata useful for provider integration diagnostics.
    #[serde(default)]
    pub debug: BTreeMap<String, String>,
}

/// One banked rate-limit reset credit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthResetCreditSnapshot {
    /// Opaque provider credit id.
    pub credit_id: String,
    /// Provider reset type, for example `codex_rate_limits`.
    pub reset_type: String,
    /// Provider status, for example `available`, `redeeming`, or `redeemed`.
    pub status: String,
    /// Provider-granted timestamp, usually RFC 3339.
    pub granted_at: String,
    /// Provider expiration timestamp, usually RFC 3339.
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Provider display title.
    #[serde(default)]
    pub title: Option<String>,
    /// Provider display description.
    #[serde(default)]
    pub description: Option<String>,
}

/// Request for consuming one provider auth rate-limit reset credit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthResetCreditConsumeRequest {
    /// Provider context identifying the auth profile to mutate.
    #[serde(default)]
    pub provider_context: ProviderRequestContext,
    /// Idempotency key for one logical reset attempt.
    pub redeem_request_id: String,
    /// Optional opaque provider credit id to consume.
    #[serde(default)]
    pub credit_id: Option<String>,
}

/// Result of consuming one provider auth rate-limit reset credit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthResetCreditConsumeResponse {
    /// Consume operation status.
    pub status: AuthResetCreditConsumeStatus,
    /// Provider code returned by the consume endpoint.
    #[serde(default)]
    pub provider_code: Option<String>,
    /// Number of rate-limit windows reset by the provider.
    #[serde(default)]
    pub windows_reset: Option<u32>,
    /// Optional human-readable detail.
    #[serde(default)]
    pub message: Option<String>,
    /// Optional reset credits after consuming one, when refresh succeeds.
    #[serde(default)]
    pub after: Option<AuthResetCreditsResponse>,
    /// Optional usage snapshot after consuming one, when refresh succeeds.
    #[serde(default)]
    pub usage_after: Option<AuthUsageResponse>,
    /// Optional debug metadata useful for provider integration diagnostics.
    #[serde(default)]
    pub debug: BTreeMap<String, String>,
}

/// Consume operation status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthResetCreditConsumeStatus {
    /// Provider/auth mode does not support reset credits.
    #[default]
    Unsupported,
    /// A reset credit was consumed and one or more windows were reset.
    Reset,
    /// No current rate-limit window is eligible for reset.
    NothingToReset,
    /// No earned reset credits are available.
    NoCredit,
    /// The idempotency key already completed a reset successfully.
    AlreadyRedeemed,
    /// Provider returned an unknown result.
    Failed,
}

/// Request for explicitly priming provider auth usage windows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthPrimeRequest {
    /// Provider context identifying the auth profile to prime.
    #[serde(default)]
    pub provider_context: ProviderRequestContext,
    /// Required usage windows grouped by provider meter id.
    #[serde(default)]
    pub required_windows: BTreeMap<String, Vec<String>>,
    /// Optional model id to use for providers that prime by sending a small model request.
    #[serde(default)]
    pub model_id: Option<String>,
    /// Request timeout in seconds.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    /// Prime even when provider usage appears already active.
    #[serde(default)]
    pub force: bool,
}

/// Result of explicitly priming provider auth usage windows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthPrimeResponse {
    /// Prime operation status.
    pub status: AuthPrimeStatus,
    /// Usage snapshot before priming, when available.
    #[serde(default)]
    pub before: Option<AuthUsageResponse>,
    /// Usage snapshot after priming, when available.
    #[serde(default)]
    pub after: Option<AuthUsageResponse>,
    /// Windows touched by the priming request.
    #[serde(default)]
    pub touched: Vec<AuthUsageWindowRef>,
    /// Optional human-readable detail.
    #[serde(default)]
    pub message: Option<String>,
}

/// Prime operation status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthPrimeStatus {
    #[default]
    Unsupported,
    AlreadyPrimed,
    Primed,
    Failed,
}

/// Reference to one provider usage window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthUsageWindowRef {
    /// Provider meter id.
    pub meter_id: String,
    /// Provider window id.
    pub window_id: String,
}

/// Provider-owned typed request extension.
///
/// Extension payload types belong to the provider crate that interprets them. The provider ID
/// scopes serialized data so unrelated providers never receive or interpret the payload.
pub trait ProviderRequestExtension: Serialize + DeserializeOwned {
    /// Provider/plugin ID that owns this extension payload.
    const PROVIDER_ID: &'static str;
}

/// Provider-neutral request context resolved by the host from model/provider profiles.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRequestContext {
    #[serde(default)]
    pub model_profile: Option<String>,
    #[serde(default)]
    pub auth_profile: Option<String>,
    #[serde(default)]
    pub auth_pool: Option<String>,
    /// Auth pool routing settings resolved from declarative configuration.
    #[serde(default)]
    pub auth_pool_routing: ProviderAuthPoolRouting,
    /// Reason the host selected the active auth-pool candidate.
    #[serde(default)]
    pub auth_pool_selection_reason: Option<String>,
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
    /// Semantic auth material resolved from the selected auth profile.
    #[serde(default)]
    pub auth: Option<ProviderAuthContext>,
    /// Ordered semantic auth candidates resolved from the selected auth pool.
    #[serde(default)]
    pub auth_candidates: Vec<ProviderAuthCandidate>,
    /// Provider-native request fields merged into the outbound provider request.
    ///
    /// This is an advanced untyped escape hatch for provider fields that do not yet have a typed
    /// extension. Prefer [`Self::set_extension`] for stable application code.
    ///
    /// These values are non-secret provider-specific request options resolved from model profiles
    /// and aliases. Providers should validate/reserve core fields before merging.
    #[serde(default)]
    pub request: BTreeMap<String, ProviderRequestValue>,
    /// Transient client-supplied environment values for provider authentication/configuration.
    ///
    /// These values are carried in-memory from the initiating client connection to the provider
    /// plugin. They must not be persisted to session history or unredacted traces.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl ProviderRequestContext {
    /// Encode and store one provider-owned typed request extension.
    ///
    /// # Errors
    ///
    /// Returns an error when the typed extension cannot be represented as JSON.
    pub fn set_extension<E>(&mut self, extension: &E) -> Result<(), serde_json::Error>
    where
        E: ProviderRequestExtension,
    {
        let value = serde_json::to_value(extension)?;
        self.request.insert(
            provider_extension_key(E::PROVIDER_ID),
            ProviderRequestValue::from(value),
        );
        Ok(())
    }

    /// Decode a provider-owned typed request extension when present.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored payload does not match the extension type.
    pub fn extension<E>(&self) -> Result<Option<E>, serde_json::Error>
    where
        E: ProviderRequestExtension,
    {
        self.request
            .get(&provider_extension_key(E::PROVIDER_ID))
            .cloned()
            .map(serde_json::Value::from)
            .map(serde_json::from_value)
            .transpose()
    }

    /// Add one typed extension and return the updated context.
    ///
    /// # Errors
    ///
    /// Returns an error when the typed extension cannot be represented as JSON.
    pub fn with_extension<E>(mut self, extension: &E) -> Result<Self, serde_json::Error>
    where
        E: ProviderRequestExtension,
    {
        self.set_extension(extension)?;
        Ok(self)
    }
}

/// Return whether a provider request key contains a typed extension envelope.
#[must_use]
pub fn is_provider_extension_key(key: &str) -> bool {
    key.starts_with("bcode.extension/")
}

/// Return the provider owner encoded in a typed extension key.
#[must_use]
pub fn provider_extension_owner(key: &str) -> Option<&str> {
    key.strip_prefix("bcode.extension/")
}

fn provider_extension_key(provider_id: &str) -> String {
    format!("bcode.extension/{provider_id}")
}

/// Provider configuration validation response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidateConfigResponse {
    pub valid: bool,
    #[serde(default)]
    pub message: Option<String>,
    /// Structured auth/config failures. Empty when validation succeeded.
    #[serde(default)]
    pub failures: Vec<ProviderFailureContext>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Provider-native web search request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeWebSearchRequest {
    pub query: String,
    #[serde(default)]
    pub max_results: Option<usize>,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub freshness: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub safe_search: Option<String>,
    #[serde(default)]
    pub provider_context: ProviderRequestContext,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Provider-native web search response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeWebSearchResponse {
    pub provider: String,
    #[serde(default)]
    pub results: Vec<NativeWebSearchResult>,
    #[serde(default)]
    pub partial: bool,
    #[serde(default)]
    pub message: Option<String>,
}

/// Provider-native web search result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeWebSearchResult {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub snippet: String,
    #[serde(default)]
    pub published: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

/// Provider-neutral structured output request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredOutputRequest {
    /// Human-readable output object name.
    pub name: String,
    /// JSON schema the provider should satisfy.
    pub schema: serde_json::Value,
    /// Whether provider-native strict schema validation should be requested where supported.
    #[serde(default)]
    pub strict: bool,
}

/// Provider-neutral model tool-choice control.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ToolChoice {
    /// Let the provider decide whether to call a tool.
    #[default]
    Auto,
    /// Prevent tool calls for this request.
    None,
    /// Require at least one tool call.
    Required,
    /// Require a specific registered tool.
    Tool {
        /// Exact model-callable tool name.
        name: String,
    },
}

/// Provider-neutral policy for model-generated tool calls in one request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallRequestPolicy {
    /// Permit the provider to generate multiple independent tool calls in one response.
    #[serde(default)]
    pub parallel: bool,
    /// Control whether or which tool the provider should call when supported.
    #[serde(default)]
    pub choice: ToolChoice,
}

/// Capability inputs required before parallel tool calls may be advertised to a provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParallelToolCallCapabilities {
    /// Provider transport advertises parallel tool-call support.
    pub provider: bool,
    /// Selected model advertises parallel tool-call support.
    pub model: bool,
    /// Host runtime can safely authorize, schedule, cancel, and order a parallel tool batch.
    pub runtime: bool,
}

impl ParallelToolCallCapabilities {
    /// Negotiate provider-visible policy from requested intent and all required capabilities.
    #[must_use]
    pub const fn negotiate(self, requested: bool, choice: ToolChoice) -> ToolCallRequestPolicy {
        ToolCallRequestPolicy {
            parallel: requested && self.provider && self.model && self.runtime,
            choice,
        }
    }
}

/// Start a provider model turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelTurnRequest {
    pub session_id: SessionId,
    pub turn_id: String,
    /// Selected model ID. Empty means the provider should use its configured default.
    pub model_id: String,
    #[serde(default)]
    pub provider_context: ProviderRequestContext,
    #[serde(default)]
    pub system_prompt: Option<String>,
    pub messages: Vec<ModelMessage>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    /// Host-resolved policy for provider-generated tool calls.
    #[serde(default)]
    pub tool_call_policy: ToolCallRequestPolicy,
    #[serde(default)]
    pub parameters: ModelParameters,
    #[serde(default)]
    pub structured_output: Option<StructuredOutputRequest>,
    #[serde(default)]
    pub context_management: ContextManagementRequest,
    #[serde(default)]
    pub prompt_cache: PromptCacheHints,
    #[serde(default)]
    pub conversation_reuse: ConversationReuseHints,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Provider-neutral conversation reuse hints for provider-native continuation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationReuseHints {
    /// Provider-native conversation reuse mode selected by the host.
    #[serde(default)]
    pub mode: ConversationReuseMode,
    /// Stable key for the reusable provider conversation state.
    #[serde(default)]
    pub key: Option<String>,
    /// Provider response/turn ID to continue from, when available.
    #[serde(default)]
    pub previous_provider_response_id: Option<String>,
    /// First message index not covered by `previous_provider_response_id`.
    #[serde(default)]
    pub new_messages_start_index: Option<usize>,
    /// Provider-private state from prior turns, such as encrypted reasoning continuation payloads.
    #[serde(default)]
    pub provider_state: Option<serde_json::Value>,
}

/// Provider-native conversation reuse policy level.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationReuseMode {
    #[default]
    Off,
    Auto,
}

impl ConversationReuseMode {
    /// Return whether provider-native continuation may be used.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        matches!(self, Self::Auto)
    }
}

/// Provider-neutral prompt cache hints for a model turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheHints {
    /// Prompt cache mode selected by the host.
    #[serde(default)]
    pub mode: PromptCacheMode,
    /// Whether the stable system prompt prefix should end with a provider cache point.
    #[serde(default)]
    pub cache_system_prompt: bool,
    /// Whether provider tool definitions should end with a provider cache point.
    #[serde(default)]
    pub cache_tools: bool,
}

/// Prompt caching policy level.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheMode {
    Off,
    #[default]
    Auto,
    Aggressive,
}

impl PromptCacheMode {
    /// Return whether provider cache hints should be emitted.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Return whether conversation-prefix cache points should be emitted.
    #[must_use]
    pub const fn cache_conversation_prefix(self) -> bool {
        matches!(self, Self::Aggressive)
    }
}

/// Provider response after starting a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartTurnResponse {
    pub provider_turn_id: String,
}

/// Verify that a provider can answer a tiny request with one model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyModelRequest {
    /// Selected model ID to verify.
    pub model_id: String,
    /// Prompt sent to the model.
    pub prompt: String,
    /// Request timeout in seconds.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub provider_context: ProviderRequestContext,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Model verification response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyModelResponse {
    pub status: VerifyModelStatus,
    #[serde(default)]
    pub latency_ms: Option<u128>,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Verification status for one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyModelStatus {
    Working,
    Unauthorized,
    NotFound,
    RateLimited,
    Timeout,
    ProviderError,
    NetworkError,
}

/// Poll queued provider turn events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollTurnEventsRequest {
    pub provider_turn_id: String,
}

/// Provider turn event batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollTurnEventsResponse {
    pub events: Vec<ProviderTurnEvent>,
}

/// Cancel an active provider turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelTurnRequest {
    pub provider_turn_id: String,
}

/// Finish or clean up a provider turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishTurnRequest {
    pub provider_turn_id: String,
}

/// Empty acknowledgement response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckResponse {}

/// Model message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelMessage {
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
}

/// Message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// Provider-neutral content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        image: ImageContent,
    },
    ToolCall {
        call: ToolCall,
    },
    ToolResult {
        result: ToolResult,
    },
    /// Provider-neutral cache point hint. Providers that do not support explicit prompt caching should ignore it.
    CachePoint {
        hint: PromptCachePoint,
    },
    ProviderExtension {
        value: serde_json::Value,
    },
}

/// Provider-neutral image content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageContent {
    pub mime_type: String,
    pub data_base64: String,
    #[serde(default)]
    pub metadata: ImageMetadata,
}

/// Image metadata useful for diagnostics and transcript display.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageMetadata {
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub byte_len: Option<u64>,
    #[serde(default)]
    pub source_path: Option<String>,
}

/// Provider-neutral prompt cache point.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCachePoint {
    /// Optional provider-neutral label for diagnostics.
    #[serde(default)]
    pub label: Option<String>,
    /// Optional provider-neutral TTL in seconds.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

/// Model parameters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelParameters {
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub stop_sequences: Vec<String>,
    #[serde(default)]
    pub reasoning_budget_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub reasoning_effort_value: Option<String>,
    #[serde(default)]
    pub reasoning_summary: Option<String>,
}

/// Tool definition supplied to a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Tool call emitted by a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Tool result supplied back to a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub output: String,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub content: Vec<ToolResultContent>,
}

/// Structured model-visible tool result content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContent {
    Text { text: String },
    Image { image: ImageContent },
    ImageRef { image: ImageRefContent },
}

/// Provider-neutral image reference content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRefContent {
    pub path: String,
    pub mime_type: String,
    #[serde(default)]
    pub metadata: ImageMetadata,
}

/// Normalized provider stream event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderTurnEvent {
    TurnStarted,
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolCallStarted {
        call_id: String,
        name: String,
    },
    ToolCallDelta {
        call_id: String,
        delta: String,
    },
    ToolCallFinished {
        call: ToolCall,
    },
    Usage {
        usage: TokenUsage,
    },
    /// Provider-confirmed complete input context for this exact request.
    ExactRequestInputTokens {
        tokens: ExactRequestInputTokens,
    },
    /// Provider reported actual request projection/sending metadata.
    RequestProjection {
        projection: ProviderRequestProjection,
    },
    /// Provider compacted the active conversation while serving this turn.
    ContextCompacted {
        /// Lossless provider-native replacement output items.
        messages: Vec<ModelMessage>,
        /// Provider-owned format required to replay the opaque messages.
        context_format: ProviderContextFormat,
    },
    /// Provider-specific metadata that the host may use for invisible optimization state.
    ProviderMetadata {
        key: String,
        value: String,
    },
    Warning {
        message: String,
    },
    RetryScheduled {
        message: String,
        retry_at_unix: u64,
    },
    Error {
        error: ProviderError,
    },
    TurnFinished {
        stop_reason: StopReason,
    },
    Cancelled,
}

/// Provider-reported request projection metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRequestProjection {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub api_shape: Option<String>,
    #[serde(default)]
    pub input_item_count: Option<usize>,
    #[serde(default)]
    pub message_count: Option<usize>,
    #[serde(default)]
    pub original_message_count: Option<usize>,
    #[serde(default)]
    pub sent_message_count: Option<usize>,
    #[serde(default)]
    pub omitted_message_count: Option<usize>,
    #[serde(default)]
    pub cache_point_count: Option<usize>,
    #[serde(default)]
    pub emitted_cache_point_count: Option<usize>,
    #[serde(default)]
    pub dropped_cache_point_count: Option<usize>,
    #[serde(default)]
    pub used_previous_response_id: bool,
    #[serde(default)]
    pub detail: Option<String>,
}

/// Provider-confirmed complete input-context token count for one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExactRequestInputTokens(u64);

impl ExactRequestInputTokens {
    /// Create a provider-confirmed request input token count.
    #[must_use]
    pub const fn new(tokens: u64) -> Self {
        Self(tokens)
    }

    /// Return the provider-confirmed token count.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Provider-neutral token usage metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Tokens supplied to the model for this turn or provider round.
    #[serde(default)]
    pub input_tokens: Option<u32>,
    /// Tokens generated by the model for this turn or provider round.
    #[serde(default)]
    pub output_tokens: Option<u32>,
    /// Provider-reported total tokens, when available.
    #[serde(default)]
    pub total_tokens: Option<u32>,
    /// Input tokens served from a provider cache, when available.
    #[serde(default)]
    pub cached_input_tokens: Option<u32>,
    /// Input tokens written to a provider prompt cache, when available.
    #[serde(default)]
    pub cache_write_input_tokens: Option<u32>,
    /// Reasoning tokens reported separately by a provider, when available.
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
}

impl TokenUsage {
    /// Return the most reliable total token count for spend/session metering.
    ///
    /// Uses the provider-reported total when present; otherwise sums input and
    /// output tokens. Cached and reasoning token fields are metadata and are not
    /// added separately because providers commonly include them in input/output
    /// totals.
    #[must_use]
    pub fn metered_total_tokens(&self) -> Option<u32> {
        self.total_tokens.or_else(|| {
            let input = self.input_tokens.unwrap_or_default();
            let output = self.output_tokens.unwrap_or_default();
            (self.input_tokens.is_some() || self.output_tokens.is_some())
                .then_some(input.saturating_add(output))
        })
    }

    /// Return uncached input tokens when both input and cached counts are known.
    #[must_use]
    pub const fn uncached_input_tokens(&self) -> Option<u32> {
        match (self.input_tokens, self.cached_input_tokens) {
            (Some(input), Some(cached)) => Some(input.saturating_sub(cached)),
            _ => self.input_tokens,
        }
    }
}

/// Provider turn stop reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolCall,
    MaxTokens,
    StopSequence,
    Cancelled,
    Error,
}

/// Granular feature requested by one model turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "family", content = "feature", rename_all = "snake_case")]
pub enum RequestedModelFeature {
    Parameter(ModelParameterKey),
    StructuredOutput(StructuredOutputMode),
    ToolChoice(ToolChoiceMode),
    PromptCache(PromptCacheFeature),
    MediaInput(MediaInputFeature),
}

impl RequestedModelFeature {
    /// Return the support claim for this feature.
    #[must_use]
    pub fn support_in(self, support: &ModelFeatureSupport) -> &CapabilitySupport {
        match self {
            Self::Parameter(parameter) => support.parameter(parameter),
            Self::StructuredOutput(mode) => support.structured_output(mode),
            Self::ToolChoice(mode) => support.tool_choice(mode),
            Self::PromptCache(feature) => support.prompt_cache(feature),
            Self::MediaInput(feature) => support.media_input(feature),
        }
    }
}

impl ModelTurnRequest {
    /// Return every granular feature explicitly exercised by this request.
    #[must_use]
    pub fn requested_features(&self) -> BTreeSet<RequestedModelFeature> {
        let mut features = BTreeSet::new();
        collect_parameter_features(&self.parameters, &mut features);
        collect_output_and_tool_features(self, &mut features);
        collect_cache_features(&self.prompt_cache, &mut features);
        collect_content_features(&self.messages, &mut features);
        features
    }

    /// Negotiate every feature exercised by this request across provider and model scopes.
    #[must_use]
    pub fn negotiate_requested_features(
        &self,
        provider: &ModelFeatureSupport,
        model: &ModelFeatureSupport,
    ) -> BTreeMap<RequestedModelFeature, NegotiatedFeatureSupport> {
        self.requested_features()
            .into_iter()
            .map(|feature| (feature, provider.negotiate(model, feature)))
            .collect()
    }

    /// Return requested features that have an explicit unsupported claim.
    #[must_use]
    pub fn explicitly_unsupported_features(
        &self,
        support: &ModelFeatureSupport,
    ) -> BTreeSet<RequestedModelFeature> {
        self.requested_features()
            .into_iter()
            .filter(|feature| {
                matches!(
                    feature.support_in(support),
                    CapabilitySupport::Unsupported { .. }
                )
            })
            .collect()
    }

    /// Return requested features that are affirmatively guaranteed by both provider and model.
    #[must_use]
    pub fn guaranteed_features(
        &self,
        provider: &ModelFeatureSupport,
        model: &ModelFeatureSupport,
    ) -> BTreeSet<RequestedModelFeature> {
        self.requested_features()
            .into_iter()
            .filter(|feature| {
                feature.support_in(provider).is_guaranteed()
                    && feature.support_in(model).is_guaranteed()
            })
            .collect()
    }
}

fn collect_parameter_features(
    parameters: &ModelParameters,
    features: &mut BTreeSet<RequestedModelFeature>,
) {
    for (requested, key) in [
        (
            parameters.temperature.is_some(),
            ModelParameterKey::Temperature,
        ),
        (
            parameters.max_output_tokens.is_some(),
            ModelParameterKey::MaxOutputTokens,
        ),
        (parameters.top_p.is_some(), ModelParameterKey::TopP),
        (
            !parameters.stop_sequences.is_empty(),
            ModelParameterKey::StopSequences,
        ),
        (
            parameters.reasoning_budget_tokens.is_some(),
            ModelParameterKey::ReasoningBudgetTokens,
        ),
        (
            parameters.reasoning_effort.is_some(),
            ModelParameterKey::ReasoningEffort,
        ),
        (
            parameters.reasoning_effort_value.is_some(),
            ModelParameterKey::ReasoningEffortValue,
        ),
        (
            parameters.reasoning_summary.is_some(),
            ModelParameterKey::ReasoningSummary,
        ),
    ] {
        if requested {
            features.insert(RequestedModelFeature::Parameter(key));
        }
    }
}

fn collect_output_and_tool_features(
    request: &ModelTurnRequest,
    features: &mut BTreeSet<RequestedModelFeature>,
) {
    if let Some(structured) = &request.structured_output {
        features.insert(RequestedModelFeature::StructuredOutput(
            if structured.strict {
                StructuredOutputMode::StrictJsonSchema
            } else {
                StructuredOutputMode::JsonSchema
            },
        ));
    }
    if request.tools.is_empty()
        && matches!(request.tool_call_policy.choice, ToolChoice::Auto)
        && !request.tool_call_policy.parallel
    {
        return;
    }
    let mode = match request.tool_call_policy.choice {
        ToolChoice::Auto => ToolChoiceMode::Auto,
        ToolChoice::None => ToolChoiceMode::None,
        ToolChoice::Required => ToolChoiceMode::Required,
        ToolChoice::Tool { .. } => ToolChoiceMode::Named,
    };
    features.insert(RequestedModelFeature::ToolChoice(mode));
    if request.tool_call_policy.parallel {
        features.insert(RequestedModelFeature::ToolChoice(ToolChoiceMode::Parallel));
    }
}

fn collect_cache_features(
    cache: &PromptCacheHints,
    features: &mut BTreeSet<RequestedModelFeature>,
) {
    if cache.mode.cache_conversation_prefix() {
        features.insert(RequestedModelFeature::PromptCache(
            PromptCacheFeature::ConversationPrefix,
        ));
    }
    if cache.cache_system_prompt {
        features.insert(RequestedModelFeature::PromptCache(
            PromptCacheFeature::ExplicitSystem,
        ));
    }
    if cache.cache_tools {
        features.insert(RequestedModelFeature::PromptCache(
            PromptCacheFeature::ExplicitTools,
        ));
    }
}

fn collect_content_features(
    messages: &[ModelMessage],
    features: &mut BTreeSet<RequestedModelFeature>,
) {
    for message in messages {
        for block in &message.content {
            match block {
                ContentBlock::Image { .. } => {
                    let feature = match message.role {
                        MessageRole::User => MediaInputFeature::UserImage,
                        MessageRole::System => MediaInputFeature::SystemImage,
                        MessageRole::Assistant => MediaInputFeature::AssistantImage,
                        MessageRole::Tool => MediaInputFeature::ToolMessageImage,
                    };
                    features.insert(RequestedModelFeature::MediaInput(feature));
                }
                ContentBlock::CachePoint { hint } => {
                    features.insert(RequestedModelFeature::PromptCache(
                        PromptCacheFeature::ExplicitMessage,
                    ));
                    if hint.ttl_seconds.is_some() {
                        features
                            .insert(RequestedModelFeature::PromptCache(PromptCacheFeature::Ttl));
                    }
                }
                ContentBlock::ToolResult { result } => {
                    for content in &result.content {
                        let feature = match content {
                            ToolResultContent::Image { .. } => MediaInputFeature::ToolResultImage,
                            ToolResultContent::ImageRef { .. } => MediaInputFeature::ImageReference,
                            ToolResultContent::Text { .. } => continue,
                        };
                        features.insert(RequestedModelFeature::MediaInput(feature));
                    }
                }
                ContentBlock::Text { .. }
                | ContentBlock::ToolCall { .. }
                | ContentBlock::ProviderExtension { .. } => {}
            }
        }
    }
}

/// Provider capability or operation affected by an auth/config failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFailureCapability {
    Authentication,
    Configuration,
    ModelDiscovery,
    ModelInvocation,
    ModelVerification,
    TokenRefresh,
    CredentialStorage,
}

/// Non-secret source kind for an auth/config failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFailureSourceKind {
    Environment,
    AuthProfile,
    ModelProfile,
    ConfigKey,
    Credential,
    CredentialStore,
    ProviderResponse,
    Runtime,
}

/// Structured, actionable, secret-safe provider auth/config failure context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderFailureContext {
    /// Provider/plugin ID responsible for remediation.
    pub provider_id: String,
    /// Kind of source that is missing or invalid.
    pub source_kind: ProviderFailureSourceKind,
    /// Non-secret source identifier such as an environment variable, profile, or config key.
    pub source: String,
    /// Capability or operation blocked by the failure.
    pub capability: ProviderFailureCapability,
    /// Concrete remediation without secret values.
    pub remediation: String,
}

impl ProviderFailureContext {
    /// Validate required context fields.
    #[must_use]
    pub fn is_actionable(&self) -> bool {
        !self.provider_id.trim().is_empty()
            && !self.source.trim().is_empty()
            && !self.remediation.trim().is_empty()
    }
}

/// Structured provider error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderError {
    pub code: String,
    pub category: ProviderErrorCategory,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default)]
    pub provider_message: Option<Box<str>>,
    /// Structured provider/auth/config failure context when applicable.
    #[serde(default)]
    pub failure: Option<Box<ProviderFailureContext>>,
    /// Provider-assigned request/correlation ID when available.
    #[serde(default)]
    pub request_id: Option<Box<str>>,
    /// Allowlisted non-secret diagnostic fields such as HTTP status or upstream error type.
    #[serde(default)]
    pub diagnostic_context: Box<BTreeMap<String, String>>,
    /// Ordered original error sources, from the provider API toward inner transports.
    #[serde(default)]
    pub sources: Box<Vec<ProviderErrorSource>>,
    #[serde(default)]
    pub retry: Option<Box<ProviderRetryHint>>,
}

impl ProviderError {
    /// Attach actionable auth/config context.
    #[must_use]
    pub fn with_failure(mut self, failure: ProviderFailureContext) -> Self {
        self.failure = Some(Box::new(failure));
        self
    }
}

/// Safe preserved source information for a normalized provider error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderErrorSource {
    /// Source subsystem, protocol, or upstream provider API.
    pub source: String,
    /// Source-native error code when available.
    #[serde(default)]
    pub code: Option<String>,
    /// Source-native message only when the adapter has determined it is safe to expose.
    #[serde(default)]
    pub message: Option<String>,
}

/// Provider-reported retry timing metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRetryHint {
    #[serde(default)]
    pub retry_after_ms: Option<u64>,
    #[serde(default)]
    pub retry_at_unix: Option<u64>,
    #[serde(default)]
    pub source: Option<String>,
}

/// Provider error category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorCategory {
    Config,
    Auth,
    RateLimit,
    Network,
    Timeout,
    ModelNotFound,
    ContextLength,
    InvalidRequest,
    UnsupportedFeature,
    ProviderInternal,
    Overloaded,
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::{
        ModelCatalogPolicy, ModelCatalogSupportHint, ModelFeatureSupport, ModelInfo, ModelList,
        ModelListAuthority, ModelParameterKey, ModelPricingInfo, ModelPricingSource,
        ModelPricingUnit, ModelTokenPrice, ModelTurnRequest, ModelVisibility,
        ModelVisibilitySource, NegotiatedFeatureSupport, ParallelToolCallCapabilities,
        ProviderError, ProviderErrorCategory, ProviderErrorSource, ProviderOperationRequirement,
        ProviderRequestContext, ProviderRequestExtension, RequestedModelFeature,
        StructuredOutputMode, TokenUsage, ToolCallRequestPolicy, ToolChoice, ToolChoiceMode,
    };

    #[test]
    fn provider_error_diagnostics_round_trip_and_default_for_older_payloads() {
        let error = ProviderError {
            code: "rate_limit".to_string(),
            category: ProviderErrorCategory::RateLimit,
            message: "limited".to_string(),
            retryable: true,
            provider_message: Some("upstream limited".into()),
            failure: Some(Box::new(super::ProviderFailureContext {
                provider_id: "provider".to_string(),
                source_kind: super::ProviderFailureSourceKind::Environment,
                source: "PROVIDER_API_KEY".to_string(),
                capability: super::ProviderFailureCapability::Authentication,
                remediation: "set PROVIDER_API_KEY".to_string(),
            })),
            request_id: Some("req_123".into()),
            diagnostic_context: Box::new(
                std::iter::once(("http_status".to_string(), "429".to_string())).collect(),
            ),
            sources: Box::new(vec![ProviderErrorSource {
                source: "provider_api".to_string(),
                code: Some("limit".to_string()),
                message: Some("upstream limited".to_string()),
            }]),
            retry: None,
        };
        let value = serde_json::to_value(&error).expect("error should encode");
        assert_eq!(
            serde_json::from_value::<ProviderError>(value).expect("error should decode"),
            error
        );

        let old = serde_json::json!({
            "code": "old",
            "category": "network",
            "message": "old payload"
        });
        let decoded: ProviderError = serde_json::from_value(old).expect("old error should decode");
        assert!(decoded.failure.is_none());
        assert!(decoded.request_id.is_none());
        assert!(decoded.diagnostic_context.is_empty());
        assert!(decoded.sources.is_empty());
    }

    #[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct TestProviderExtension {
        priority: bool,
    }

    impl ProviderRequestExtension for TestProviderExtension {
        const PROVIDER_ID: &'static str = "test.provider";
    }

    #[test]
    fn typed_provider_extensions_are_scoped_and_round_trip() {
        let mut context = ProviderRequestContext::default();
        context
            .set_extension(&TestProviderExtension { priority: true })
            .expect("extension should encode");

        let encoded = serde_json::to_string(&context).expect("context should encode");
        let decoded: ProviderRequestContext =
            serde_json::from_str(&encoded).expect("context should decode");

        assert_eq!(
            decoded
                .extension::<TestProviderExtension>()
                .expect("extension should decode"),
            Some(TestProviderExtension { priority: true })
        );
        assert_eq!(
            decoded
                .request
                .keys()
                .filter(|key| super::is_provider_extension_key(key))
                .count(),
            1
        );
    }

    #[test]
    fn provider_operation_inventory_is_unique_and_covers_all_published_operations() {
        let expected = [
            super::OP_CAPABILITIES,
            super::OP_CONTEXT_MANAGEMENT_CAPABILITIES,
            super::OP_COMPACT_CONTEXT,
            super::OP_MODELS,
            super::OP_VALIDATE_CONFIG,
            super::OP_START_TURN,
            super::OP_VERIFY_MODEL,
            super::OP_POLL_TURN_EVENTS,
            super::OP_CANCEL_TURN,
            super::OP_NATIVE_WEB_SEARCH,
            super::OP_FINISH_TURN,
            super::OP_AUTH_USAGE,
            super::OP_AUTH_PRIME,
            super::OP_AUTH_RESET_CREDITS,
            super::OP_AUTH_RESET_CREDIT_CONSUME,
        ];
        let actual = super::MODEL_PROVIDER_OPERATIONS
            .iter()
            .map(|contract| contract.operation)
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(actual.len(), super::MODEL_PROVIDER_OPERATIONS.len());
        assert_eq!(actual, expected.into_iter().collect());
        assert!(super::MODEL_PROVIDER_OPERATIONS.iter().all(|contract| {
            !contract.request_type.is_empty()
                && !contract.response_type.is_empty()
                && !contract.behavior.is_empty()
        }));
    }

    #[test]
    fn provider_baseline_lifecycle_operations_are_required() {
        for operation in [
            super::OP_CAPABILITIES,
            super::OP_MODELS,
            super::OP_VALIDATE_CONFIG,
            super::OP_START_TURN,
            super::OP_POLL_TURN_EVENTS,
            super::OP_CANCEL_TURN,
            super::OP_FINISH_TURN,
        ] {
            let contract = super::provider_operation_contract(operation)
                .expect("published operation should have a contract");
            assert_eq!(contract.requirement, ProviderOperationRequirement::Required);
        }
    }

    #[test]
    fn capability_negotiation_requires_provider_and_model_evidence() {
        let feature = RequestedModelFeature::Parameter(ModelParameterKey::Temperature);
        let mut provider = ModelFeatureSupport::default();
        provider.parameters.insert(
            ModelParameterKey::Temperature,
            super::CapabilitySupport::Supported {
                source: super::CapabilitySource::BundledCatalog,
            },
        );
        let mut model = ModelFeatureSupport::default();

        assert_eq!(
            provider.negotiate(&model, feature),
            NegotiatedFeatureSupport::Unknown {
                scope: super::CapabilityScope::Model
            }
        );

        model.parameters.insert(
            ModelParameterKey::Temperature,
            super::CapabilitySupport::Unsupported {
                source: super::CapabilitySource::ProviderApi,
                reason: "model rejects temperature".to_string(),
            },
        );
        assert!(matches!(
            provider.negotiate(&model, feature),
            NegotiatedFeatureSupport::Unsupported {
                scope: super::CapabilityScope::Model,
                ..
            }
        ));

        model.parameters.insert(
            ModelParameterKey::Temperature,
            super::CapabilitySupport::Supported {
                source: super::CapabilitySource::ProviderApi,
            },
        );
        assert!(provider.negotiate(&model, feature).is_guaranteed());
    }

    #[test]
    fn granular_capability_metadata_defaults_to_unknown_for_older_payloads() {
        let provider: super::ProviderCapabilities = serde_json::from_value(serde_json::json!({
            "provider_id": "old",
            "display_name": "Old",
            "capabilities": []
        }))
        .expect("old provider capabilities should decode");
        assert_eq!(provider.feature_support, ModelFeatureSupport::default());
        assert!(
            !provider
                .feature_support
                .parameter(ModelParameterKey::Temperature)
                .is_guaranteed()
        );

        let model: ModelInfo = serde_json::from_value(serde_json::json!({
            "model_id": "old-model",
            "display_name": "Old model"
        }))
        .expect("old model info should decode");
        assert_eq!(model.feature_support, ModelFeatureSupport::default());
    }

    #[test]
    fn request_feature_inventory_distinguishes_parameters_outputs_and_tool_modes() {
        let request: ModelTurnRequest = serde_json::from_value(serde_json::json!({
            "session_id": "00000000-0000-0000-0000-000000000000",
            "turn_id": "turn",
            "model_id": "model",
            "messages": [],
            "tools": [{"name":"lookup","description":"lookup","input_schema":{"type":"object"}}],
            "tool_call_policy": {"parallel": true, "choice": {"mode":"required"}},
            "parameters": {"temperature": 0.2},
            "structured_output": {"name":"answer","schema":{"type":"object"},"strict":true}
        }))
        .expect("feature request should decode");
        let requested = request.requested_features();

        assert!(requested.contains(&RequestedModelFeature::Parameter(
            ModelParameterKey::Temperature
        )));
        assert!(requested.contains(&RequestedModelFeature::StructuredOutput(
            StructuredOutputMode::StrictJsonSchema
        )));
        assert!(requested.contains(&RequestedModelFeature::ToolChoice(ToolChoiceMode::Required)));
        assert!(requested.contains(&RequestedModelFeature::ToolChoice(ToolChoiceMode::Parallel)));
    }

    #[test]
    fn request_feature_inventory_tracks_cache_hints_and_media_roles() {
        let mut request: ModelTurnRequest = serde_json::from_value(serde_json::json!({
            "session_id": "00000000-0000-0000-0000-000000000000",
            "turn_id": "turn",
            "model_id": "model",
            "messages": [{
                "role": "system",
                "content": [{
                    "type": "image",
                    "image": {"mime_type":"image/png","data_base64":"AA=="}
                }]
            }],
            "prompt_cache": {"mode":"aggressive"}
        }))
        .expect("media/cache request should decode");
        request.messages.push(super::ModelMessage {
            role: super::MessageRole::Tool,
            content: vec![super::ContentBlock::ToolResult {
                result: super::ToolResult {
                    call_id: "call".to_string(),
                    output: String::new(),
                    is_error: false,
                    content: vec![super::ToolResultContent::ImageRef {
                        image: super::ImageRefContent {
                            path: "artifact://image".to_string(),
                            mime_type: "image/png".to_string(),
                            metadata: super::ImageMetadata::default(),
                        },
                    }],
                },
            }],
        });
        let requested = request.requested_features();

        assert!(requested.contains(&RequestedModelFeature::PromptCache(
            super::PromptCacheFeature::ConversationPrefix
        )));
        assert!(requested.contains(&RequestedModelFeature::MediaInput(
            super::MediaInputFeature::SystemImage
        )));
        assert!(requested.contains(&RequestedModelFeature::MediaInput(
            super::MediaInputFeature::ImageReference
        )));
    }

    #[test]
    fn model_turn_request_defaults_typed_tool_call_policy_when_omitted() {
        let request: ModelTurnRequest = serde_json::from_value(serde_json::json!({
            "session_id": "00000000-0000-0000-0000-000000000000",
            "turn_id": "turn",
            "model_id": "model",
            "messages": []
        }))
        .expect("request without typed policy should decode with the safe default");

        assert_eq!(request.tool_call_policy, ToolCallRequestPolicy::default());
    }

    #[test]
    fn typed_tool_call_policy_round_trips_parallel_intent() {
        let policy = ToolCallRequestPolicy {
            parallel: true,
            ..ToolCallRequestPolicy::default()
        };
        let encoded = serde_json::to_value(&policy).expect("policy should encode");
        let decoded: ToolCallRequestPolicy =
            serde_json::from_value(encoded).expect("policy should decode");

        assert_eq!(decoded, policy);
    }

    #[test]
    fn parallel_tool_policy_requires_intent_provider_model_and_runtime() {
        let ready = ParallelToolCallCapabilities {
            provider: true,
            model: true,
            runtime: true,
        };
        assert!(ready.negotiate(true, ToolChoice::Auto).parallel);
        assert!(!ready.negotiate(false, ToolChoice::Auto).parallel);
        for capabilities in [
            ParallelToolCallCapabilities {
                provider: false,
                ..ready
            },
            ParallelToolCallCapabilities {
                model: false,
                ..ready
            },
            ParallelToolCallCapabilities {
                runtime: false,
                ..ready
            },
        ] {
            assert!(!capabilities.negotiate(true, ToolChoice::Auto).parallel);
        }
    }

    #[test]
    fn token_usage_prefers_provider_reported_total() {
        let usage = TokenUsage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            total_tokens: Some(20),
            ..TokenUsage::default()
        };

        assert_eq!(usage.metered_total_tokens(), Some(20));
    }

    #[test]
    fn token_usage_falls_back_to_input_plus_output() {
        let usage = TokenUsage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            ..TokenUsage::default()
        };

        assert_eq!(usage.metered_total_tokens(), Some(15));
    }

    #[test]
    fn pricing_estimate_charges_cached_input_separately() {
        let pricing = ModelPricingInfo {
            currency: "USD".to_string(),
            unit: ModelPricingUnit::PerMillionTokens,
            input: Some(ModelTokenPrice::from_micros(1_000_000)),
            cached_input: Some(ModelTokenPrice::from_micros(100_000)),
            cache_write_input: Some(ModelTokenPrice::from_micros(1_250_000)),
            output: Some(ModelTokenPrice::from_micros(4_000_000)),
            source: ModelPricingSource::BundledCatalog,
        };
        let usage = TokenUsage {
            input_tokens: Some(2_000_000),
            cached_input_tokens: Some(500_000),
            cache_write_input_tokens: Some(100_000),
            output_tokens: Some(250_000),
            ..TokenUsage::default()
        };

        let cost = pricing.estimate_cost(&usage).expect("cost should estimate");

        assert_eq!(cost.total_micros, 2_675_000);
    }

    #[test]
    fn pricing_estimate_preserves_zero_cost_as_known() {
        let pricing = ModelPricingInfo {
            currency: "USD".to_string(),
            unit: ModelPricingUnit::PerMillionTokens,
            input: Some(ModelTokenPrice::from_micros(0)),
            cached_input: None,
            cache_write_input: None,
            output: Some(ModelTokenPrice::from_micros(0)),
            source: ModelPricingSource::UserOverride,
        };
        let usage = TokenUsage {
            input_tokens: Some(42),
            output_tokens: Some(0),
            ..TokenUsage::default()
        };

        let estimate = pricing.estimate_cost(&usage).expect("known free cost");
        assert_eq!(estimate.total_micros, 0);
    }

    #[test]
    fn pricing_estimate_preserves_sub_micro_rounding_as_known() {
        let pricing = ModelPricingInfo {
            currency: "USD".to_string(),
            unit: ModelPricingUnit::PerMillionTokens,
            input: Some(ModelTokenPrice::from_micros(1)),
            cached_input: None,
            cache_write_input: None,
            output: Some(ModelTokenPrice::from_micros(1)),
            source: ModelPricingSource::UserOverride,
        };
        let usage = TokenUsage {
            input_tokens: Some(1),
            output_tokens: Some(0),
            ..TokenUsage::default()
        };

        let estimate = pricing.estimate_cost(&usage).expect("known rounded cost");
        assert_eq!(estimate.total_micros, 0);
    }

    #[test]
    fn pricing_estimate_rejects_partial_pricing_coverage() {
        let pricing = ModelPricingInfo {
            currency: "USD".to_string(),
            unit: ModelPricingUnit::PerMillionTokens,
            input: Some(ModelTokenPrice::from_micros(1)),
            cached_input: None,
            cache_write_input: None,
            output: None,
            source: ModelPricingSource::UserOverride,
        };
        let usage = TokenUsage {
            input_tokens: Some(1),
            output_tokens: Some(1),
            ..TokenUsage::default()
        };

        assert!(pricing.estimate_cost(&usage).is_none());
    }

    #[test]
    fn pricing_estimate_rejects_unknown_separately_priced_cache_usage() {
        let pricing = ModelPricingInfo {
            currency: "USD".to_string(),
            unit: ModelPricingUnit::PerMillionTokens,
            input: Some(ModelTokenPrice::from_micros(1)),
            cached_input: Some(ModelTokenPrice::from_micros(1)),
            cache_write_input: None,
            output: Some(ModelTokenPrice::from_micros(1)),
            source: ModelPricingSource::UserOverride,
        };
        let usage = TokenUsage {
            input_tokens: Some(1),
            output_tokens: Some(1),
            cached_input_tokens: None,
            ..TokenUsage::default()
        };

        assert!(pricing.estimate_cost(&usage).is_none());
    }

    #[test]
    fn pricing_estimate_is_unknown_without_priced_buckets() {
        let pricing = ModelPricingInfo {
            currency: "USD".to_string(),
            unit: ModelPricingUnit::PerMillionTokens,
            input: None,
            cached_input: None,
            cache_write_input: None,
            output: None,
            source: ModelPricingSource::Unknown,
        };

        assert!(pricing.estimate_cost(&TokenUsage::default()).is_none());
    }

    #[test]
    fn overloaded_error_category_serializes_as_snake_case() {
        let encoded = serde_json::to_string(&ProviderErrorCategory::Overloaded)
            .expect("category should encode");
        assert_eq!(encoded, "\"overloaded\"");

        let decoded: ProviderErrorCategory =
            serde_json::from_str(&encoded).expect("category should decode");
        assert_eq!(decoded, ProviderErrorCategory::Overloaded);
    }

    #[test]
    fn model_catalog_policy_preserves_tagged_json_shape() {
        let policy = ModelCatalogPolicy::ExpandSupported {
            provider_id: "openai".to_owned(),
            target: ModelCatalogSupportHint {
                provider: "openai".to_owned(),
                auth_mode: "api_key".to_owned(),
                api_surface: "responses".to_owned(),
                integration: None,
            },
            authority: ModelListAuthority::Partial,
        };

        let encoded = serde_json::to_value(&policy).expect("policy should encode");
        assert_eq!(encoded["kind"], "expand_supported");
        assert_eq!(encoded["provider_id"], "openai");

        let decoded: ModelCatalogPolicy =
            serde_json::from_value(encoded).expect("policy should decode");
        assert_eq!(decoded, policy);
    }

    #[test]
    fn model_list_visibility_is_local_not_serialized() {
        let list = ModelList {
            models: vec![ModelInfo {
                model_id: "hidden".to_string(),
                display_name: "Hidden".to_string(),
                is_default: false,
                context_window: None,
                max_output_tokens: None,
                capabilities: std::collections::BTreeSet::new(),
                feature_support: super::ModelFeatureSupport::default(),
                reasoning: None,
                cache: super::ModelCacheInfo::default(),
                metadata_source: None,
                pricing: None,
                visibility: ModelVisibility::Ignored {
                    source: ModelVisibilitySource::State,
                    rule: "hidden".to_string(),
                },
            }],
            catalog: super::ModelCatalogHints::default(),
        };

        let encoded = serde_json::to_string(&list).expect("model list should encode");
        let decoded: ModelList = serde_json::from_str(&encoded).expect("model list should decode");

        assert_eq!(decoded.models[0].visibility, ModelVisibility::Visible);
    }
}
