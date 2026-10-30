//! Provider-owned JSON DTO projections for aws-sdk-bedrockruntime 1.148.0.
//!
//! These project the final SDK input and decoded stream unions, not HTTP bytes,
//! signing headers, SDK configuration, HTTP chunk boundaries, or SDK retry attempts.
//! Field names follow the locked SDK protocol serializer. Unknown union variants
//! fail capture explicitly rather than being presented as a complete known event.
//! Content (including binary content as base64) remains sensitive opt-in data.
use aws_sdk_bedrockruntime::operation::converse_stream::builders::ConverseStreamInputBuilder;
use aws_sdk_bedrockruntime::types::{
    AnyToolChoice, AppliedGuardrailDetails, AudioBlock, AudioFormat, AudioSource, AutoToolChoice,
    CacheDetail, CachePointBlock, CachePointType, CacheTtl, Citation, CitationGeneratedContent,
    CitationLocation, CitationSourceContent, CitationSourceContentDelta, CitationsConfig,
    CitationsContentBlock, CitationsDelta, ContentBlock, ContentBlockDelta, ContentBlockDeltaEvent,
    ContentBlockStart, ContentBlockStartEvent, ContentBlockStopEvent, ConversationRole,
    ConverseStreamMetadataEvent, ConverseStreamMetrics, ConverseStreamOutput, ConverseStreamTrace,
    DocumentBlock, DocumentCharLocation, DocumentChunkLocation, DocumentContentBlock,
    DocumentFormat, DocumentPageLocation, DocumentSource, ErrorBlock, GuardrailAssessment,
    GuardrailAutomatedReasoningFinding, GuardrailAutomatedReasoningImpossibleFinding,
    GuardrailAutomatedReasoningInputTextReference, GuardrailAutomatedReasoningInvalidFinding,
    GuardrailAutomatedReasoningLogicWarning, GuardrailAutomatedReasoningLogicWarningType,
    GuardrailAutomatedReasoningNoTranslationsFinding, GuardrailAutomatedReasoningPolicyAssessment,
    GuardrailAutomatedReasoningRule, GuardrailAutomatedReasoningSatisfiableFinding,
    GuardrailAutomatedReasoningScenario, GuardrailAutomatedReasoningStatement,
    GuardrailAutomatedReasoningTooComplexFinding, GuardrailAutomatedReasoningTranslation,
    GuardrailAutomatedReasoningTranslationAmbiguousFinding,
    GuardrailAutomatedReasoningTranslationOption, GuardrailAutomatedReasoningValidFinding,
    GuardrailContentFilter, GuardrailContentFilterConfidence, GuardrailContentFilterStrength,
    GuardrailContentFilterType, GuardrailContentPolicyAction, GuardrailContentPolicyAssessment,
    GuardrailContextualGroundingFilter, GuardrailContextualGroundingFilterType,
    GuardrailContextualGroundingPolicyAction, GuardrailContextualGroundingPolicyAssessment,
    GuardrailConverseContentBlock, GuardrailConverseContentQualifier, GuardrailConverseImageBlock,
    GuardrailConverseImageFormat, GuardrailConverseImageSource, GuardrailConverseTextBlock,
    GuardrailCoverage, GuardrailCustomWord, GuardrailImageCoverage, GuardrailInvocationMetrics,
    GuardrailManagedWord, GuardrailManagedWordType, GuardrailOrigin, GuardrailOwnership,
    GuardrailPiiEntityFilter, GuardrailPiiEntityType, GuardrailRegexFilter,
    GuardrailSensitiveInformationPolicyAction, GuardrailSensitiveInformationPolicyAssessment,
    GuardrailStreamConfiguration, GuardrailStreamProcessingMode, GuardrailTextCharactersCoverage,
    GuardrailTopic, GuardrailTopicPolicyAction, GuardrailTopicPolicyAssessment, GuardrailTopicType,
    GuardrailTrace, GuardrailTraceAssessment, GuardrailUsage, GuardrailWordPolicyAction,
    GuardrailWordPolicyAssessment, ImageBlock, ImageBlockDelta, ImageBlockStart, ImageFormat,
    ImageSource, InferenceConfiguration, JsonSchemaDefinition, Message, MessageStartEvent,
    MessageStopEvent, OutputConfig, OutputFormat, OutputFormatStructure, OutputFormatType,
    PerformanceConfigLatency, PerformanceConfiguration, PromptRouterTrace, PromptVariableValues,
    ReasoningContentBlock, ReasoningContentBlockDelta, ReasoningTextBlock, S3Location,
    SearchResultBlock, SearchResultContentBlock, SearchResultLocation, ServiceTier,
    ServiceTierType, SpecificToolChoice, StopReason, SystemContentBlock, SystemTool, TokenUsage,
    Tool, ToolAdditionBlock, ToolChoice, ToolConfiguration, ToolInputSchema, ToolReference,
    ToolRemovalBlock, ToolResultBlock, ToolResultBlockDelta, ToolResultBlockStart,
    ToolResultContentBlock, ToolResultStatus, ToolSpecification, ToolUseBlock, ToolUseBlockDelta,
    ToolUseBlockStart, ToolUseType, VideoBlock, VideoFormat, VideoSource, WebLocation,
};
use aws_smithy_types::{Blob, Document, Number};
use base64::Engine as _;
use serde::ser::{SerializeMap, SerializeStruct};
use serde::{Serialize, Serializer};
use std::collections::HashMap;

#[cfg(test)]
#[path = "converse_diagnostics_tests.rs"]
mod tests;

/// Borrowed provider DTO. No SDK Debug output or authentication objects enter capture.
pub struct Wire<'a, T: ?Sized>(pub &'a T);

macro_rules! scalar {
    ($($ty:ty),* $(,)?) => {$(
        impl Serialize for Wire<'_, $ty> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.0.serialize(serializer)
            }
        }
    )*};
}
scalar!(String, bool, i32, i64, u64, f32, f64);

impl<T> Serialize for Wire<'_, Vec<T>>
where
    for<'a> Wire<'a, T>: Serialize,
{
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.iter().map(Wire))
    }
}
impl<T> Serialize for Wire<'_, HashMap<String, T>>
where
    for<'a> Wire<'a, T>: Serialize,
{
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in self.0 {
            map.serialize_entry(key, &Wire(value))?;
        }
        map.end()
    }
}
impl Serialize for Wire<'_, Blob> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.0.as_ref().len() > super::diagnostics::MAX_PAYLOAD_BYTES {
            return Err(serde::ser::Error::custom("binary diagnostic size limit"));
        }
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(self.0.as_ref()))
    }
}
impl Serialize for Wire<'_, Document> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Document::Object(value) => Wire(value).serialize(serializer),
            Document::Array(value) => Wire(value).serialize(serializer),
            Document::Number(Number::PosInt(value)) => serializer.serialize_u64(*value),
            Document::Number(Number::NegInt(value)) => serializer.serialize_i64(*value),
            Document::Number(Number::Float(value)) => serializer.serialize_f64(*value),
            Document::String(value) => serializer.serialize_str(value),
            Document::Bool(value) => serializer.serialize_bool(*value),
            Document::Null => serializer.serialize_none(),
        }
    }
}

macro_rules! object {
    ($ty:ty, required { }, optional { }) => {
        impl Serialize for Wire<'_, $ty> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_map(Some(0))?.end()
            }
        }
    };
    ($ty:ty, required { $($field:ident => $key:literal),* $(,)? }, optional { $($opt:ident => $opt_key:literal),* $(,)? }) => {
        impl Serialize for Wire<'_, $ty> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut map = serializer.serialize_map(None)?;
                $(map.serialize_entry($key, &Wire(&self.0.$field))?;)*
                $(if let Some(value) = &self.0.$opt { map.serialize_entry($opt_key, &Wire(value))?; })*
                map.end()
            }
        }
    };
}
macro_rules! string_enum {
    ($ty:ty) => {
        impl Serialize for Wire<'_, $ty> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.0.as_str())
            }
        }
    };
}
macro_rules! union {
    ($ty:ident { $($variant:ident => $key:literal),* $(,)? }) => {
        impl Serialize for Wire<'_, $ty> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut map = serializer.serialize_map(Some(1))?;
                match self.0 {
                    $($ty::$variant(value) => map.serialize_entry($key, &Wire(value))?,)*
                    _ => return Err(serde::ser::Error::custom(concat!("unsupported AWS union: ", stringify!($ty)))),
                }
                map.end()
            }
        }
    };
}

/// Exact final fluent-builder DTO, read immediately before send. Model ID belongs
/// to the HTTP route, not to the JSON body. No `config_override` or credentials included.
impl Serialize for Wire<'_, ConverseStreamInputBuilder> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut dto = serializer.serialize_struct("BedrockConverseRequestV1", 4)?;
        dto.serialize_field("dto_format", "bcode.bedrock.converse.request")?;
        dto.serialize_field("dto_version", &1_u16)?;
        dto.serialize_field("model_id", self.0.get_model_id())?;
        dto.serialize_field("body", &RequestBody(self.0))?;
        dto.end()
    }
}
struct RequestBody<'a>(&'a ConverseStreamInputBuilder);

/// Received SDK stream DTO. This is decoded provider evidence, not a normalized
/// model event and not the raw AWS event-stream framing.
pub struct StreamRecord<'a>(pub &'a ConverseStreamOutput);
impl Serialize for StreamRecord<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut dto = serializer.serialize_struct("BedrockConverseStreamV1", 3)?;
        dto.serialize_field("dto_format", "bcode.bedrock.converse.stream")?;
        dto.serialize_field("dto_version", &1_u16)?;
        dto.serialize_field("event", &Wire(self.0))?;
        dto.end()
    }
}

/// The SDK's initial response exposes only a receiver and an optional request ID.
/// Do not attempt to serialize a receiver or pretend it contains received events.
#[derive(Serialize)]
pub struct ResponseRecord {
    dto_format: &'static str,
    dto_version: u16,
    representation: &'static str,
}
impl Default for ResponseRecord {
    fn default() -> Self {
        Self {
            dto_format: "bcode.bedrock.converse.response",
            dto_version: 1,
            representation: "sdk_stream_receiver_opened",
        }
    }
}

impl Serialize for RequestBody<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        if let Some(value) = self.0.get_messages() {
            map.serialize_entry("messages", &Wire(value))?;
        }
        if let Some(value) = self.0.get_system() {
            map.serialize_entry("system", &Wire(value))?;
        }
        if let Some(value) = self.0.get_inference_config() {
            map.serialize_entry("inferenceConfig", &Wire(value))?;
        }
        if let Some(value) = self.0.get_tool_config() {
            map.serialize_entry("toolConfig", &Wire(value))?;
        }
        if let Some(value) = self.0.get_guardrail_config() {
            map.serialize_entry("guardrailConfig", &Wire(value))?;
        }
        if let Some(value) = self.0.get_additional_model_request_fields() {
            map.serialize_entry("additionalModelRequestFields", &Wire(value))?;
        }
        if let Some(value) = self.0.get_prompt_variables() {
            map.serialize_entry("promptVariables", &Wire(value))?;
        }
        if let Some(value) = self.0.get_additional_model_response_field_paths() {
            map.serialize_entry("additionalModelResponseFieldPaths", &Wire(value))?;
        }
        if let Some(value) = self.0.get_request_metadata() {
            map.serialize_entry("requestMetadata", &Wire(value))?;
        }
        if let Some(value) = self.0.get_performance_config() {
            map.serialize_entry("performanceConfig", &Wire(value))?;
        }
        if let Some(value) = self.0.get_service_tier() {
            map.serialize_entry("serviceTier", &Wire(value))?;
        }
        if let Some(value) = self.0.get_output_config() {
            map.serialize_entry("outputConfig", &Wire(value))?;
        }
        map.end()
    }
}

object!(AnyToolChoice, required {}, optional {});
object!(AppliedGuardrailDetails, required {  }, optional { guardrail_id => "guardrailId", guardrail_version => "guardrailVersion", guardrail_arn => "guardrailArn", guardrail_origin => "guardrailOrigin", guardrail_ownership => "guardrailOwnership" });
object!(AudioBlock, required { format => "format" }, optional { source => "source", error => "error" });
string_enum!(AudioFormat);
union!(AudioSource { Bytes => "bytes", S3Location => "s3Location" });
object!(AutoToolChoice, required {}, optional {});
object!(CacheDetail, required { ttl => "ttl", input_tokens => "inputTokens" }, optional {  });
object!(CachePointBlock, required { r#type => "type" }, optional { ttl => "ttl" });
string_enum!(CachePointType);
string_enum!(CacheTtl);
object!(Citation, required {  }, optional { title => "title", source => "source", source_content => "sourceContent", location => "location" });
union!(CitationGeneratedContent { Text => "text" });
union!(CitationLocation { DocumentChar => "documentChar", DocumentChunk => "documentChunk", DocumentPage => "documentPage", SearchResultLocation => "searchResultLocation", Web => "web" });
union!(CitationSourceContent { Text => "text" });
object!(CitationSourceContentDelta, required {  }, optional { text => "text" });
object!(CitationsConfig, required { enabled => "enabled" }, optional {  });
object!(CitationsContentBlock, required {  }, optional { content => "content", citations => "citations" });
object!(CitationsDelta, required {  }, optional { title => "title", source => "source", source_content => "sourceContent", location => "location" });
union!(ContentBlock { Audio => "audio", CachePoint => "cachePoint", CitationsContent => "citationsContent", Document => "document", GuardContent => "guardContent", Image => "image", ReasoningContent => "reasoningContent", SearchResult => "searchResult", Text => "text", ToolAddition => "toolAddition", ToolRemoval => "toolRemoval", ToolResult => "toolResult", ToolUse => "toolUse", Video => "video" });
union!(ContentBlockDelta { Citation => "citation", Image => "image", ReasoningContent => "reasoningContent", Text => "text", ToolResult => "toolResult", ToolUse => "toolUse" });
object!(ContentBlockDeltaEvent, required { content_block_index => "contentBlockIndex" }, optional { delta => "delta" });
union!(ContentBlockStart { Image => "image", ToolResult => "toolResult", ToolUse => "toolUse" });
object!(ContentBlockStartEvent, required { content_block_index => "contentBlockIndex" }, optional { start => "start" });
object!(ContentBlockStopEvent, required { content_block_index => "contentBlockIndex" }, optional {  });
string_enum!(ConversationRole);
object!(ConverseStreamMetadataEvent, required {  }, optional { usage => "usage", metrics => "metrics", trace => "trace", performance_config => "performanceConfig", service_tier => "serviceTier" });
object!(ConverseStreamMetrics, required { latency_ms => "latencyMs" }, optional {  });
union!(ConverseStreamOutput { ContentBlockDelta => "contentBlockDelta", ContentBlockStart => "contentBlockStart", ContentBlockStop => "contentBlockStop", MessageStart => "messageStart", MessageStop => "messageStop", Metadata => "metadata" });
object!(ConverseStreamTrace, required {  }, optional { guardrail => "guardrail", prompt_router => "promptRouter" });
object!(DocumentBlock, required { format => "format", name => "name" }, optional { source => "source", context => "context", citations => "citations" });
object!(DocumentCharLocation, required {  }, optional { document_index => "documentIndex", start => "start", end => "end" });
object!(DocumentChunkLocation, required {  }, optional { document_index => "documentIndex", start => "start", end => "end" });
union!(DocumentContentBlock { Text => "text" });
string_enum!(DocumentFormat);
object!(DocumentPageLocation, required {  }, optional { document_index => "documentIndex", start => "start", end => "end" });
union!(DocumentSource { Bytes => "bytes", Content => "content", S3Location => "s3Location", Text => "text" });
object!(ErrorBlock, required {  }, optional { message => "message" });
object!(GuardrailAssessment, required {  }, optional { topic_policy => "topicPolicy", content_policy => "contentPolicy", word_policy => "wordPolicy", sensitive_information_policy => "sensitiveInformationPolicy", contextual_grounding_policy => "contextualGroundingPolicy", automated_reasoning_policy => "automatedReasoningPolicy", invocation_metrics => "invocationMetrics", applied_guardrail_details => "appliedGuardrailDetails" });
union!(GuardrailAutomatedReasoningFinding { Impossible => "impossible", Invalid => "invalid", NoTranslations => "noTranslations", Satisfiable => "satisfiable", TooComplex => "tooComplex", TranslationAmbiguous => "translationAmbiguous", Valid => "valid" });
object!(GuardrailAutomatedReasoningImpossibleFinding, required {  }, optional { translation => "translation", contradicting_rules => "contradictingRules", logic_warning => "logicWarning" });
object!(GuardrailAutomatedReasoningInputTextReference, required {  }, optional { text => "text" });
object!(GuardrailAutomatedReasoningInvalidFinding, required {  }, optional { translation => "translation", contradicting_rules => "contradictingRules", logic_warning => "logicWarning" });
object!(GuardrailAutomatedReasoningLogicWarning, required {  }, optional { r#type => "type", premises => "premises", claims => "claims" });
string_enum!(GuardrailAutomatedReasoningLogicWarningType);
object!(
    GuardrailAutomatedReasoningNoTranslationsFinding,
    required {},
    optional {}
);
object!(GuardrailAutomatedReasoningPolicyAssessment, required {  }, optional { findings => "findings" });
object!(GuardrailAutomatedReasoningRule, required {  }, optional { identifier => "identifier", policy_version_arn => "policyVersionArn" });
object!(GuardrailAutomatedReasoningSatisfiableFinding, required {  }, optional { translation => "translation", claims_true_scenario => "claimsTrueScenario", claims_false_scenario => "claimsFalseScenario", logic_warning => "logicWarning" });
object!(GuardrailAutomatedReasoningScenario, required {  }, optional { statements => "statements" });
object!(GuardrailAutomatedReasoningStatement, required {  }, optional { logic => "logic", natural_language => "naturalLanguage" });
object!(
    GuardrailAutomatedReasoningTooComplexFinding,
    required {},
    optional {}
);
object!(GuardrailAutomatedReasoningTranslation, required {  }, optional { premises => "premises", claims => "claims", untranslated_premises => "untranslatedPremises", untranslated_claims => "untranslatedClaims", confidence => "confidence" });
object!(GuardrailAutomatedReasoningTranslationAmbiguousFinding, required {  }, optional { options => "options", difference_scenarios => "differenceScenarios" });
object!(GuardrailAutomatedReasoningTranslationOption, required {  }, optional { translations => "translations" });
object!(GuardrailAutomatedReasoningValidFinding, required {  }, optional { translation => "translation", claims_true_scenario => "claimsTrueScenario", supporting_rules => "supportingRules", logic_warning => "logicWarning" });
object!(GuardrailContentFilter, required { r#type => "type", confidence => "confidence", action => "action" }, optional { filter_strength => "filterStrength", detected => "detected" });
string_enum!(GuardrailContentFilterConfidence);
string_enum!(GuardrailContentFilterStrength);
string_enum!(GuardrailContentFilterType);
string_enum!(GuardrailContentPolicyAction);
object!(GuardrailContentPolicyAssessment, required { filters => "filters" }, optional {  });
object!(GuardrailContextualGroundingFilter, required { r#type => "type", threshold => "threshold", score => "score", action => "action" }, optional { detected => "detected" });
string_enum!(GuardrailContextualGroundingFilterType);
string_enum!(GuardrailContextualGroundingPolicyAction);
object!(GuardrailContextualGroundingPolicyAssessment, required {  }, optional { filters => "filters" });
union!(GuardrailConverseContentBlock { Image => "image", Text => "text" });
string_enum!(GuardrailConverseContentQualifier);
object!(GuardrailConverseImageBlock, required { format => "format" }, optional { source => "source" });
string_enum!(GuardrailConverseImageFormat);
union!(GuardrailConverseImageSource { Bytes => "bytes" });
object!(GuardrailConverseTextBlock, required { text => "text" }, optional { qualifiers => "qualifiers" });
object!(GuardrailCoverage, required {  }, optional { text_characters => "textCharacters", images => "images" });
object!(GuardrailCustomWord, required { r#match => "match", action => "action" }, optional { detected => "detected" });
object!(GuardrailImageCoverage, required {  }, optional { guarded => "guarded", total => "total" });
object!(GuardrailInvocationMetrics, required {  }, optional { guardrail_processing_latency => "guardrailProcessingLatency", usage => "usage", guardrail_coverage => "guardrailCoverage" });
object!(GuardrailManagedWord, required { r#match => "match", r#type => "type", action => "action" }, optional { detected => "detected" });
string_enum!(GuardrailManagedWordType);
string_enum!(GuardrailOrigin);
string_enum!(GuardrailOwnership);
object!(GuardrailPiiEntityFilter, required { r#match => "match", r#type => "type", action => "action" }, optional { detected => "detected" });
string_enum!(GuardrailPiiEntityType);
object!(GuardrailRegexFilter, required { action => "action" }, optional { name => "name", r#match => "match", regex => "regex", detected => "detected" });
string_enum!(GuardrailSensitiveInformationPolicyAction);
object!(GuardrailSensitiveInformationPolicyAssessment, required { pii_entities => "piiEntities", regexes => "regexes" }, optional {  });
object!(GuardrailStreamConfiguration, required { guardrail_identifier => "guardrailIdentifier", guardrail_version => "guardrailVersion", trace => "trace", stream_processing_mode => "streamProcessingMode" }, optional {  });
string_enum!(GuardrailStreamProcessingMode);
object!(GuardrailTextCharactersCoverage, required {  }, optional { guarded => "guarded", total => "total" });
object!(GuardrailTopic, required { name => "name", r#type => "type", action => "action" }, optional { detected => "detected" });
string_enum!(GuardrailTopicPolicyAction);
object!(GuardrailTopicPolicyAssessment, required { topics => "topics" }, optional {  });
string_enum!(GuardrailTopicType);
string_enum!(GuardrailTrace);
object!(GuardrailTraceAssessment, required {  }, optional { model_output => "modelOutput", input_assessment => "inputAssessment", output_assessments => "outputAssessments", action_reason => "actionReason" });
object!(GuardrailUsage, required { topic_policy_units => "topicPolicyUnits", content_policy_units => "contentPolicyUnits", word_policy_units => "wordPolicyUnits", sensitive_information_policy_units => "sensitiveInformationPolicyUnits", sensitive_information_policy_free_units => "sensitiveInformationPolicyFreeUnits", contextual_grounding_policy_units => "contextualGroundingPolicyUnits" }, optional { content_policy_image_units => "contentPolicyImageUnits", automated_reasoning_policy_units => "automatedReasoningPolicyUnits", automated_reasoning_policies => "automatedReasoningPolicies" });
string_enum!(GuardrailWordPolicyAction);
object!(GuardrailWordPolicyAssessment, required { custom_words => "customWords", managed_word_lists => "managedWordLists" }, optional {  });
object!(ImageBlock, required { format => "format" }, optional { source => "source", error => "error" });
object!(ImageBlockDelta, required {  }, optional { source => "source", error => "error" });
object!(ImageBlockStart, required { format => "format" }, optional {  });
string_enum!(ImageFormat);
union!(ImageSource { Bytes => "bytes", S3Location => "s3Location" });
object!(InferenceConfiguration, required {  }, optional { max_tokens => "maxTokens", temperature => "temperature", top_p => "topP", stop_sequences => "stopSequences" });
object!(JsonSchemaDefinition, required { schema => "schema" }, optional { name => "name", description => "description" });
object!(Message, required { role => "role", content => "content" }, optional {  });
object!(MessageStartEvent, required { role => "role" }, optional {  });
object!(MessageStopEvent, required { stop_reason => "stopReason" }, optional { additional_model_response_fields => "additionalModelResponseFields" });
object!(OutputConfig, required {  }, optional { text_format => "textFormat", effort => "effort" });
object!(OutputFormat, required { r#type => "type" }, optional { structure => "structure" });
union!(OutputFormatStructure { JsonSchema => "jsonSchema" });
string_enum!(OutputFormatType);
string_enum!(PerformanceConfigLatency);
object!(PerformanceConfiguration, required { latency => "latency" }, optional {  });
object!(PromptRouterTrace, required {  }, optional { invoked_model_id => "invokedModelId" });
union!(PromptVariableValues { Text => "text" });
union!(ReasoningContentBlock { ReasoningText => "reasoningText", RedactedContent => "redactedContent" });
union!(ReasoningContentBlockDelta { RedactedContent => "redactedContent", Signature => "signature", Text => "text" });
object!(ReasoningTextBlock, required { text => "text" }, optional { signature => "signature" });
object!(S3Location, required { uri => "uri" }, optional { bucket_owner => "bucketOwner" });
object!(SearchResultBlock, required { source => "source", title => "title", content => "content" }, optional { citations => "citations" });
object!(SearchResultContentBlock, required { text => "text" }, optional {  });
object!(SearchResultLocation, required {  }, optional { search_result_index => "searchResultIndex", start => "start", end => "end" });
object!(ServiceTier, required { r#type => "type" }, optional {  });
string_enum!(ServiceTierType);
object!(SpecificToolChoice, required { name => "name" }, optional {  });
string_enum!(StopReason);
union!(SystemContentBlock { CachePoint => "cachePoint", GuardContent => "guardContent", Text => "text" });
object!(SystemTool, required { name => "name" }, optional {  });
object!(TokenUsage, required { input_tokens => "inputTokens", output_tokens => "outputTokens", total_tokens => "totalTokens" }, optional { cache_read_input_tokens => "cacheReadInputTokens", cache_write_input_tokens => "cacheWriteInputTokens", cache_details => "cacheDetails" });
union!(Tool { CachePoint => "cachePoint", SystemTool => "systemTool", ToolSpec => "toolSpec" });
object!(ToolAdditionBlock, required {  }, optional { tool => "tool" });
union!(ToolChoice { Any => "any", Auto => "auto", Tool => "tool" });
object!(ToolConfiguration, required { tools => "tools" }, optional { tool_choice => "toolChoice" });
union!(ToolInputSchema { Json => "json" });
object!(ToolReference, required {  }, optional { r#type => "type", name => "name", server_name => "serverName" });
object!(ToolRemovalBlock, required {  }, optional { tool => "tool" });
object!(ToolResultBlock, required { tool_use_id => "toolUseId", content => "content" }, optional { status => "status", r#type => "type" });
union!(ToolResultBlockDelta { Json => "json", Text => "text" });
object!(ToolResultBlockStart, required { tool_use_id => "toolUseId" }, optional { r#type => "type", status => "status" });
union!(ToolResultContentBlock { Document => "document", Image => "image", Json => "json", SearchResult => "searchResult", Text => "text", Video => "video" });
string_enum!(ToolResultStatus);
object!(ToolSpecification, required { name => "name" }, optional { description => "description", input_schema => "inputSchema", strict => "strict" });
object!(ToolUseBlock, required { tool_use_id => "toolUseId", name => "name", input => "input" }, optional { r#type => "type" });
object!(ToolUseBlockDelta, required { input => "input" }, optional {  });
object!(ToolUseBlockStart, required { tool_use_id => "toolUseId", name => "name" }, optional { r#type => "type" });
string_enum!(ToolUseType);
object!(VideoBlock, required { format => "format" }, optional { source => "source" });
string_enum!(VideoFormat);
union!(VideoSource { Bytes => "bytes", S3Location => "s3Location" });
object!(WebLocation, required {  }, optional { url => "url", domain => "domain" });
