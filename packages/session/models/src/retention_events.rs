//! Borrowed event admission before an observation source clones any payload.
//!
//! Inspectors count capacity, not encoded size. Nested typed payloads retain
//! their owned capacities; no encoded-length admission substitute is used.

use crate::retention::{
    ObservationRetentionExceeded, ObservationRetentionLimits, ObservationRetentionUsage,
    SessionObservationSourceLimits,
};
use crate::{
    ProviderStreamEvent, ReasoningActivityEvent, SessionEvent, SessionEventKind, SessionLiveEvent,
    SessionLiveEventKind, TextStreamOperation,
};

pub(crate) struct Inspector {
    pub(crate) usage: ObservationRetentionUsage,
    pub(crate) limits: ObservationRetentionLimits,
}

impl Inspector {
    pub(crate) fn new(
        limits: &SessionObservationSourceLimits,
    ) -> Result<Self, ObservationRetentionExceeded> {
        let limits = limits.retention_limits()?;
        let usage = ObservationRetentionUsage {
            copies: 1,
            ..ObservationRetentionUsage::default()
        };
        usage.check(limits)?;
        Ok(Self { usage, limits })
    }

    pub(crate) fn nodes(
        &mut self,
        count: usize,
        depth: usize,
    ) -> Result<(), ObservationRetentionExceeded> {
        self.usage.try_add(
            ObservationRetentionUsage {
                value_nodes: count,
                depth,
                ..ObservationRetentionUsage::default()
            },
            self.limits,
        )
    }

    pub(crate) fn text(
        &mut self,
        value: &String,
        depth: usize,
    ) -> Result<(), ObservationRetentionExceeded> {
        self.usage
            .try_add(ObservationRetentionUsage::string(value, depth), self.limits)
    }

    pub(crate) fn optional_text(
        &mut self,
        value: Option<&String>,
        depth: usize,
    ) -> Result<(), ObservationRetentionExceeded> {
        self.nodes(1, depth)?;
        if let Some(text) = value {
            self.text(text, depth + 1)?;
        }
        Ok(())
    }

    fn nested_limits(
        &self,
        depth: usize,
    ) -> Result<ObservationRetentionLimits, ObservationRetentionExceeded> {
        let mut limits = self.limits;
        limits.depth = limits
            .depth
            .checked_sub(depth - 1)
            .ok_or(ObservationRetentionExceeded)?;
        Ok(limits)
    }

    pub(crate) fn nested(
        &mut self,
        mut usage: ObservationRetentionUsage,
        depth: usize,
    ) -> Result<(), ObservationRetentionExceeded> {
        usage.depth = usage
            .depth
            .checked_add(depth - 1)
            .ok_or(ObservationRetentionExceeded)?;
        self.usage.try_add(usage, self.limits)
    }

    pub(crate) fn path(
        &mut self,
        value: &std::path::PathBuf,
        depth: usize,
    ) -> Result<(), ObservationRetentionExceeded> {
        self.usage.try_add(
            ObservationRetentionUsage {
                scalar_capacity_bytes: value.capacity(),
                value_nodes: 1,
                depth,
                ..ObservationRetentionUsage::default()
            },
            self.limits,
        )
    }

    pub(crate) fn collection(
        &mut self,
        capacity: usize,
        depth: usize,
    ) -> Result<(), ObservationRetentionExceeded> {
        self.usage.try_add(
            ObservationRetentionUsage::collection(capacity, depth),
            self.limits,
        )
    }

    pub(crate) fn json(
        &mut self,
        value: &serde_json::Value,
        depth: usize,
    ) -> Result<(), ObservationRetentionExceeded> {
        // Check the outer depth before the shared traversal allocates its stack.
        self.nodes(0, depth)?;
        let mut limits = self.limits;
        limits.depth = limits
            .depth
            .checked_sub(depth - 1)
            .ok_or(ObservationRetentionExceeded)?;
        let mut usage = ObservationRetentionUsage::json(value, limits)?;
        usage.depth = usage
            .depth
            .checked_add(depth - 1)
            .ok_or(ObservationRetentionExceeded)?;
        self.usage.try_add(usage, self.limits)
    }

    fn stream(
        &mut self,
        update: &crate::TextStreamUpdate,
    ) -> Result<(), ObservationRetentionExceeded> {
        let mut limits = self.limits;
        limits.depth = limits
            .depth
            .checked_sub(2)
            .ok_or(ObservationRetentionExceeded)?;
        let mut usage = inspect_text_stream(update, &limits)?;
        usage.depth = usage
            .depth
            .checked_add(2)
            .ok_or(ObservationRetentionExceeded)?;
        self.usage.try_add(usage, self.limits)
    }

    fn reasoning(
        &mut self,
        event: &ReasoningActivityEvent,
        depth: usize,
    ) -> Result<(), ObservationRetentionExceeded> {
        self.nodes(8, depth)?;
        match event {
            ReasoningActivityEvent::Started { activity_id, .. }
            | ReasoningActivityEvent::OpaqueObserved { activity_id, .. }
            | ReasoningActivityEvent::Finished { activity_id, .. } => {
                self.text(activity_id, depth + 1)
            }
            ReasoningActivityEvent::PartDelta {
                activity_id,
                part_id,
                text,
                ..
            }
            | ReasoningActivityEvent::PartCompleted {
                activity_id,
                part_id,
                text,
                ..
            } => {
                self.text(activity_id, depth + 1)?;
                self.text(part_id, depth + 1)?;
                self.text(text, depth + 1)
            }
        }
    }
}

/// Inspect a standalone text stream update without reserving an event copy.
///
/// The root is depth one. Containers must shift returned depth and reserve the
/// resulting usage through the shared retention contract before retaining it.
///
/// # Errors
/// Returns an error when any structural limit or checked counter is exceeded.
pub fn inspect_text_stream(
    value: &crate::TextStreamUpdate,
    limits: &ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let mut inspector = Inspector {
        usage: ObservationRetentionUsage::default(),
        limits: *limits,
    };
    inspector.nodes(4, 1)?;
    inspector.nodes(1, 2)?;
    match &value.operation {
        TextStreamOperation::Append { text, .. } => {
            inspector.nodes(1, 3)?;
            inspector.text(text, 3)?;
        }
        TextStreamOperation::Checkpoint { text, .. } => {
            inspector.nodes(3, 3)?;
            inspector.text(text, 3)?;
        }
        TextStreamOperation::Terminal { .. } => inspector.nodes(1, 3)?,
    }
    Ok(inspector.usage)
}

fn standalone(limits: &ObservationRetentionLimits) -> Inspector {
    Inspector {
        usage: ObservationRetentionUsage::default(),
        limits: *limits,
    }
}

fn artifact(
    i: &mut Inspector,
    value: &bcode_tool_models::ToolContributionArtifact,
    depth: usize,
) -> Result<(), ObservationRetentionExceeded> {
    i.nodes(4, depth)?;
    i.text(&value.artifact_id, depth + 1)?;
    i.text(&value.reference_key, depth + 1)?;
    i.optional_text(value.content_type.as_ref(), depth + 1)?;
    i.text(&value.storage_uri, depth + 1)
}

/// Inspect an invocation-owned presentation, root depth one and no event copy.
///
/// # Errors
/// Returns an error on structural resource exhaustion or arithmetic overflow.
pub fn inspect_tool_presentation(
    value: &crate::ToolPresentationUpdate,
    limits: &ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let mut i = standalone(limits);
    i.nodes(8, 1)?;
    i.text(&value.invocation_id, 2)?;
    i.text(&value.producer_id, 2)?;
    i.text(&value.schema, 2)?;
    if let bcode_tool_models::ToolPresentationIdentity::Supplemental { item_id } = &value.identity {
        i.text(item_id, 3)?;
    }
    if let Some(value) = &value.artifact {
        artifact(&mut i, value, 2)?;
    }
    i.json(&value.payload, 2)?;
    Ok(i.usage)
}

/// Inspect an opaque contribution, root depth one and no event copy.
///
/// # Errors
/// Returns an error on structural resource exhaustion or arithmetic overflow.
pub fn inspect_tool_contribution(
    value: &crate::ToolContributionEvent,
    limits: &ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let mut i = standalone(limits);
    i.nodes(7, 1)?;
    i.text(&value.invocation_id, 2)?;
    i.text(&value.contribution_id, 2)?;
    i.text(&value.producer_id, 2)?;
    i.text(&value.schema, 2)?;
    if let Some(value) = &value.artifact {
        artifact(&mut i, value, 2)?;
    }
    i.json(&value.payload, 2)?;
    Ok(i.usage)
}

/// Inspect a correlated exchange request, root depth one and no event copy.
///
/// # Errors
/// Returns an error on structural resource exhaustion or arithmetic overflow.
pub fn inspect_tool_exchange_request(
    value: &crate::ToolExchangeRequest,
    limits: &ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let mut i = standalone(limits);
    i.nodes(3, 1)?;
    i.text(&value.invocation_id, 2)?;
    i.text(&value.exchange_id, 2)?;
    i.text(&value.producer_id, 2)?;
    i.text(&value.schema, 2)?;
    i.json(&value.payload, 2)?;
    Ok(i.usage)
}

/// Inspect an exchange resolution, root depth one and no event copy.
///
/// # Errors
/// Returns an error on structural resource exhaustion or arithmetic overflow.
pub fn inspect_tool_exchange_resolution(
    value: &crate::ToolExchangeResolution,
    limits: &ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let mut i = standalone(limits);
    i.nodes(1, 1)?;
    match value {
        crate::ToolExchangeResolution::Responded { payload } => i.json(payload, 2)?,
        crate::ToolExchangeResolution::Failed { code, message } => {
            i.text(code, 2)?;
            i.text(message, 2)?;
        }
        crate::ToolExchangeResolution::Cancelled
        | crate::ToolExchangeResolution::TimedOut
        | crate::ToolExchangeResolution::NoCompatibleConsumer
        | crate::ToolExchangeResolution::ConsumerDetached => {}
    }
    Ok(i.usage)
}

/// Inspect invocation lifecycle metadata, root depth one and no event copy.
///
/// # Errors
/// Returns an error on structural resource exhaustion or arithmetic overflow.
pub fn inspect_tool_lifecycle(
    value: &crate::ToolInvocationLifecycleEvent,
    limits: &ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let mut i = standalone(limits);
    i.nodes(3, 1)?;
    i.text(&value.invocation_id, 2)?;
    i.optional_text(value.message.as_ref(), 2)?;
    i.json(&value.metadata, 2)?;
    Ok(i.usage)
}

fn tool_artifact(
    i: &mut Inspector,
    value: &crate::ToolArtifact,
    depth: usize,
) -> Result<(), ObservationRetentionExceeded> {
    i.nodes(3, depth)?;
    i.text(&value.artifact_id, depth + 1)?;
    i.text(&value.producer_plugin_id, depth + 1)?;
    i.text(&value.schema, depth + 1)?;
    i.optional_text(value.tool_call_id.as_ref(), depth + 1)?;
    i.optional_text(value.title.as_ref(), depth + 1)?;
    i.json(&value.metadata, depth + 1)?;
    i.collection(value.refs.capacity(), depth + 1)?;
    for reference in &value.refs {
        i.nodes(3, depth + 2)?;
        i.text(&reference.key, depth + 3)?;
        i.optional_text(reference.content_type.as_ref(), depth + 3)?;
        i.optional_text(reference.storage_uri.as_ref(), depth + 3)?;
        if let Some(metadata) = &reference.metadata {
            i.json(metadata, depth + 4)?;
        }
    }
    Ok(())
}

fn tool_result(
    i: &mut Inspector,
    value: &crate::ToolInvocationResult,
    depth: usize,
) -> Result<(), ObservationRetentionExceeded> {
    i.nodes(1, depth)?;
    match value {
        crate::ToolInvocationResult::Text { text } => i.text(text, depth + 1)?,
        crate::ToolInvocationResult::Json { value } => i.text(value, depth + 1)?,
        crate::ToolInvocationResult::Artifact { artifact } => {
            tool_artifact(i, artifact, depth + 1)?;
        }
    }
    Ok(())
}

/// Inspect a retained invocation projection, root depth one and no event copy.
///
/// # Errors
/// Returns an error on structural resource exhaustion or arithmetic overflow.
pub fn inspect_tool_projection(
    value: &crate::ToolInvocationProjection,
    limits: &ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let mut i = standalone(limits);
    i.nodes(9, 1)?;
    i.text(&value.tool_call_id, 2)?;
    i.optional_text(value.producer_plugin_id.as_ref(), 2)?;
    i.optional_text(value.tool_name.as_ref(), 2)?;
    i.optional_text(value.arguments_json.as_ref(), 2)?;
    i.optional_text(value.result_text.as_ref(), 2)?;
    if let Some(path) = &value.working_directory {
        i.path(path, 3)?;
    }
    if let Some(result) = &value.raw_result {
        tool_result(&mut i, result, 3)?;
    }
    if let Some(presentation) = &value.presentation {
        i.nested(
            inspect_tool_presentation(presentation, &i.nested_limits(3)?)?,
            3,
        )?;
    }
    Ok(i.usage)
}

fn execution_provenance(
    i: &mut Inspector,
    value: &crate::ExecutionSessionProvenance,
    depth: usize,
) -> Result<(), ObservationRetentionExceeded> {
    i.nodes(6, depth)?;
    i.text(&value.owner, depth + 1)?;
    i.text(&value.run_id, depth + 1)?;
    i.text(&value.node_id, depth + 1)?;
    i.optional_text(value.activation_id.as_ref(), depth + 1)?;
    i.optional_text(value.workspace_snapshot.as_ref(), depth + 1)
}

/// Inspect a session summary, root depth one and no event copy.
///
/// # Errors
/// Returns an error on structural resource exhaustion or arithmetic overflow.
pub fn inspect_session_summary(
    value: &crate::SessionSummary,
    limits: &ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let mut i = standalone(limits);
    i.nodes(12, 1)?;
    i.optional_text(value.name.as_ref(), 2)?;
    i.optional_text(value.explicit_name.as_ref(), 2)?;
    i.optional_text(value.derived_title.as_ref(), 2)?;
    i.optional_text(value.automatic_title.as_ref(), 2)?;
    i.path(&value.working_directory, 2)?;
    if let Some(import) = &value.import {
        i.nodes(2, 3)?;
        i.text(&import.source_id, 4)?;
        i.text(&import.source_display_name, 4)?;
        i.text(&import.external_session_id, 4)?;
    }
    if let Some(execution) = &value.execution {
        i.nodes(2, 3)?;
        execution_provenance(&mut i, &execution.provenance, 4)?;
    }
    if let Some(location) = &value.location {
        i.nodes(3, 3)?;
        i.text(&location.location_id, 4)?;
        i.optional_text(location.profile.as_ref(), 4)?;
    }
    Ok(i.usage)
}

fn skill_source(
    i: &mut Inspector,
    source: Option<&crate::SkillSource>,
    depth: usize,
) -> Result<(), ObservationRetentionExceeded> {
    i.nodes(1, depth)?;
    if let Some(source) = source {
        i.nodes(3, depth + 1)?;
        i.text(&source.label, depth + 2)?;
        i.optional_text(source.path.as_ref(), depth + 2)?;
    }
    Ok(())
}

fn request_identity(
    i: &mut Inspector,
    value: &crate::ModelRequestIdentity,
    depth: usize,
) -> Result<(), ObservationRetentionExceeded> {
    i.nodes(5, depth)?;
    i.text(&value.provider_plugin_id, depth + 1)?;
    i.optional_text(value.requested_model_id.as_ref(), depth + 1)?;
    i.text(&value.effective_model_id, depth + 1)?;
    i.text(&value.request_id, depth + 1)?;
    i.text(&value.model_turn_id, depth + 1)?;
    i.text(&value.request_fingerprint, depth + 1)?;
    i.optional_text(value.effective_auth_profile.as_ref(), depth + 1)?;
    i.optional_text(value.compatibility_key.as_ref(), depth + 1)
}

/// Inspect token usage, root depth one and no event copy.
///
/// # Errors
/// Returns an error on structural resource exhaustion or arithmetic overflow.
pub fn inspect_session_token_usage(
    value: &crate::SessionTokenUsage,
    limits: &ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let mut i = standalone(limits);
    i.nodes(14, 1)?;
    for text in [
        &value.request_id,
        &value.observation_id,
        &value.catalog_provider_id,
        &value.catalog_entry_id,
        &value.catalog_family,
        &value.catalog_api_surface,
    ] {
        i.optional_text(text.as_ref(), 2)?;
    }
    if let Some(request) = &value.request {
        request_identity(&mut i, request, 3)?;
    }
    if let Some(target) = &value.pricing_target {
        i.nodes(1, 3)?;
        i.text(&target.provider, 4)?;
        i.text(&target.auth_mode, 4)?;
        i.text(&target.api_surface, 4)?;
        i.optional_text(target.integration.as_ref(), 4)?;
    }
    i.nodes(3, 2)?;
    for text in [
        &value.pricing_context.service_tier,
        &value.pricing_context.invocation_class,
        &value.pricing_context.billing_scope,
    ] {
        i.optional_text(text.as_ref(), 3)?;
    }
    i.collection(value.pricing_usage_details.capacity(), 2)?;
    for detail in &value.pricing_usage_details {
        i.nodes(3, 3)?;
        i.text(&detail.bucket, 4)?;
        i.text(&detail.modality, 4)?;
    }
    if let Some(cost) = &value.cost {
        i.nodes(1, 3)?;
        match cost {
            crate::SessionCostEstimate::Unavailable { .. } => i.nodes(1, 4)?,
            crate::SessionCostEstimate::Estimated {
                currency,
                source,
                revision,
                components,
                ..
            } => {
                i.nodes(1, 4)?;
                i.text(currency, 4)?;
                i.text(source, 4)?;
                i.optional_text(revision.as_ref(), 4)?;
                i.collection(components.capacity(), 4)?;
                for component in components {
                    i.nodes(4, 5)?;
                    i.text(&component.bucket, 6)?;
                    i.optional_text(component.modality.as_ref(), 6)?;
                }
            }
        }
    }
    Ok(i.usage)
}

/// Inspect context occupancy, root depth one and no event copy.
///
/// # Errors
/// Returns an error on structural resource exhaustion or arithmetic overflow.
pub fn inspect_context_occupancy(
    value: &crate::RequestContextOccupancy,
    limits: &ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let mut i = standalone(limits);
    i.nodes(3, 1)?;
    context_observation(&mut i, &value.observation, 2)?;
    Ok(i.usage)
}

fn context_observation(
    i: &mut Inspector,
    value: &crate::RequestContextObservation,
    depth: usize,
) -> Result<(), ObservationRetentionExceeded> {
    i.nodes(4, depth)?;
    request_identity(i, &value.request, depth + 1)?;
    i.nodes(4, depth + 1)
}

fn thinking(
    i: &mut Inspector,
    value: Option<&bcode_skill_models::SkillThinkingEffort>,
    depth: usize,
) -> Result<(), ObservationRetentionExceeded> {
    i.nodes(1, depth)?;
    if let Some(value) = value {
        i.nodes(2, depth + 1)?;
        i.text(&value.source_label, depth + 2)?;
        i.optional_text(value.provider_value.as_ref(), depth + 2)?;
    }
    Ok(())
}

fn activity_presentation(
    i: &mut Inspector,
    value: &crate::ActivityPresentation,
    d: usize,
) -> Result<(), ObservationRetentionExceeded> {
    i.nodes(4, d)?;
    for text in [
        &value.producer,
        &value.activity_id,
        &value.schema,
        &value.fallback,
    ] {
        i.text(text, d + 1)?;
    }
    i.json(&value.payload, d + 1)?;
    Ok(())
}

fn admission(
    i: &mut Inspector,
    value: &crate::TurnAdmissionMetadata,
    depth: usize,
) -> Result<(), ObservationRetentionExceeded> {
    i.nodes(6, depth)?;
    i.optional_text(value.idempotency_key.as_ref(), depth + 1)?;
    if let Some(origin) = &value.origin {
        i.nodes(1, depth + 2)?;
        i.text(&origin.producer, depth + 3)?;
        i.optional_text(origin.correlation_id.as_ref(), depth + 3)?;
        i.optional_text(origin.display_label.as_ref(), depth + 3)?;
    }
    if let Some(activity) = &value.activity {
        activity_presentation(i, activity, depth + 2)?;
    }
    let execution = &value.execution;
    let d = depth + 1;
    i.nodes(14, d)?;
    for text in [
        &execution.agent_profile,
        &execution.provider_plugin_id,
        &execution.model_id,
        &execution.request_context_id,
    ] {
        i.optional_text(text.as_ref(), d + 1)?;
    }
    i.collection(execution.historical_context.capacity(), d + 1)?;
    i.nodes(
        execution
            .historical_context
            .len()
            .checked_mul(3)
            .ok_or(ObservationRetentionExceeded)?,
        d + 2,
    )?;
    if execution.interaction_route.is_some() {
        i.nodes(3, d + 2)?;
    }
    if let Some(correlation) = &execution.correlation {
        i.nodes(2, d + 2)?;
        i.text(&correlation.execution_id, d + 3)?;
        i.text(&correlation.unit_id, d + 3)?;
    }
    if let Some(tools) = &execution.tool_allowlist {
        i.collection(tools.capacity(), d + 2)?;
        for tool in tools {
            i.text(tool, d + 3)?;
        }
    }
    if let Some(reasoning) = &execution.reasoning {
        i.nodes(1, d + 2)?;
        i.optional_text(reasoning.effort.as_ref(), d + 3)?;
        i.optional_text(reasoning.summary.as_ref(), d + 3)?;
    }
    if let Some(output) = &execution.structured_output {
        i.nodes(3, d + 2)?;
        i.text(&output.name, d + 3)?;
        i.json(&output.schema, d + 3)?;
    }
    i.collection(execution.skill_contexts.capacity(), d + 1)?;
    for context in &execution.skill_contexts {
        i.nodes(4, d + 2)?;
        i.text(&context.skill_id.0, d + 3)?;
        i.text(&context.context, d + 3)?;
        skill_source(i, Some(&context.source), d + 3)?;
        if let Some(policy) = &context.model_policy {
            i.nodes(3, d + 4)?;
            thinking(i, policy.thinking_effort.as_ref(), d + 5)?;
            for request in [&policy.preferred, &policy.required].into_iter().flatten() {
                i.nodes(3, d + 6)?;
                i.text(&request.model, d + 7)?;
                i.optional_text(request.provider.as_ref(), d + 7)?;
                thinking(i, request.thinking_effort.as_ref(), d + 7)?;
            }
        }
    }
    Ok(())
}

fn milestone(
    i: &mut Inspector,
    value: &crate::SessionMilestone,
    depth: usize,
) -> Result<(), ObservationRetentionExceeded> {
    i.nodes(5, depth)?;
    i.nodes(4, depth + 1)?;
    i.text(&value.evaluation_id, depth + 1)?;
    i.text(&value.header, depth + 1)?;
    i.text(&value.description, depth + 1)?;
    i.text(&value.provenance.provider_id, depth + 2)?;
    i.text(&value.provenance.model_id, depth + 2)?;
    i.nodes(2, depth + 2)
}

fn child_creation(
    i: &mut Inspector,
    value: &crate::SessionChildCreateRequest,
    depth: usize,
) -> Result<(), ObservationRetentionExceeded> {
    i.nodes(3, depth)?;
    i.optional_text(value.name.as_ref(), depth + 1)?;
    i.path(&value.working_directory, depth + 1)
}

fn trace_blob(
    i: &mut Inspector,
    value: Option<&crate::TraceBlobRef>,
    depth: usize,
) -> Result<(), ObservationRetentionExceeded> {
    i.nodes(1, depth)?;
    if let Some(blob) = value {
        i.nodes(4, depth + 1)?;
        i.text(&blob.sha256, depth + 2)?;
        i.text(&blob.path, depth + 2)?;
        i.text(&blob.content_type, depth + 2)?;
    }
    Ok(())
}

fn provider_progress(
    i: &mut Inspector,
    value: &ProviderStreamEvent,
    depth: usize,
) -> Result<(), ObservationRetentionExceeded> {
    i.nodes(1, depth)?;
    match value {
        ProviderStreamEvent::TurnStarted => {}
        ProviderStreamEvent::ToolCallStarted {
            tool_call_id,
            tool_name,
        }
        | ProviderStreamEvent::ToolCallFinished {
            tool_call_id,
            tool_name,
        }
        | ProviderStreamEvent::ToolCallProgress {
            tool_call_id,
            tool_name,
            ..
        } => {
            i.text(tool_call_id, depth + 1)?;
            i.text(tool_name, depth + 1)?;
            i.nodes(1, depth + 1)?;
        }
        ProviderStreamEvent::RetryScheduled { message, .. } => {
            i.text(message, depth + 1)?;
            i.nodes(1, depth + 1)?;
        }
        ProviderStreamEvent::NoProgressWarning {
            active_tool_call, ..
        } => {
            i.nodes(2, depth + 1)?;
            if let Some(progress) = active_tool_call {
                i.nodes(2, depth + 2)?;
                i.text(&progress.tool_call_id, depth + 3)?;
                i.text(&progress.tool_name, depth + 3)?;
            }
        }
    }
    Ok(())
}

fn trace(
    i: &mut Inspector,
    value: &crate::SessionTraceEvent,
    depth: usize,
) -> Result<(), ObservationRetentionExceeded> {
    i.nodes(3, depth)?;
    i.optional_text(value.turn_id.as_ref(), depth + 1)?;
    let d = depth + 1;
    i.nodes(1, d)?;
    match &value.payload {
        crate::SessionTracePayload::ModelRequestBuilt {
            provider,
            model,
            agent_id,
            prompt_cache_mode,
            conversation_reuse_mode,
            metadata,
            request,
            ..
        } => {
            i.nodes(4, d + 1)?;
            for text in [
                provider,
                model,
                agent_id,
                prompt_cache_mode,
                conversation_reuse_mode,
            ] {
                i.text(text, d + 1)?;
            }
            i.collection(metadata.len(), d + 1)?;
            for (key, value) in metadata {
                i.text(key, d + 2)?;
                i.text(value, d + 2)?;
            }
            trace_blob(i, request.as_ref(), d + 1)?;
        }
        crate::SessionTracePayload::ProviderRound {
            provider_turn_id,
            provider,
            stop_reason,
            error,
            ..
        } => {
            i.nodes(2, d + 1)?;
            i.text(provider, d + 1)?;
            for text in [provider_turn_id, stop_reason, error] {
                i.optional_text(text.as_ref(), d + 1)?;
            }
        }
        crate::SessionTracePayload::ProviderEvent { event_type, detail } => {
            i.text(event_type, d + 1)?;
            i.optional_text(detail.as_ref(), d + 1)?;
        }
        payload => trace_tool_payload(i, payload, d)?,
    }
    Ok(())
}

fn durable_runtime(
    i: &mut Inspector,
    kind: &SessionEventKind,
) -> Result<(), ObservationRetentionExceeded> {
    match kind {
        SessionEventKind::RuntimeWorkStarted {
            work_id,
            label,
            tool_call_id,
            plugin_id,
            service_interface,
            operation,
            parent_work_id,
            ..
        } => {
            i.text(&work_id.0, 3)?;
            i.text(label, 3)?;
            i.optional_text(tool_call_id.as_ref(), 3)?;
            i.optional_text(plugin_id.as_ref(), 3)?;
            i.optional_text(service_interface.as_ref(), 3)?;
            i.optional_text(operation.as_ref(), 3)?;
            i.nodes(4, 3)?;
            if let Some(work) = parent_work_id {
                i.text(&work.0, 4)?;
            }
        }
        SessionEventKind::RuntimeWorkCancelRequested { work_id, .. } => {
            i.text(&work_id.0, 3)?;
            i.nodes(2, 3)?;
        }
        SessionEventKind::RuntimeWorkFinished {
            work_id, message, ..
        } => {
            i.text(&work_id.0, 3)?;
            i.optional_text(message.as_ref(), 3)?;
            i.nodes(2, 3)?;
        }
        SessionEventKind::RuntimeWorkProgress {
            work_id, message, ..
        } => {
            i.text(&work_id.0, 3)?;
            i.text(message, 3)?;
            i.nodes(3, 3)?;
        }
        SessionEventKind::SkillDeactivated { skill_id, .. } => {
            i.text(&skill_id.0, 3)?;
            i.nodes(1, 3)?;
        }
        SessionEventKind::SkillSuggested {
            skill_id, reason, ..
        } => {
            i.text(&skill_id.0, 3)?;
            i.optional_text(reason.as_ref(), 3)?;
            i.nodes(1, 3)?;
        }
        SessionEventKind::SkillInvocationFailed {
            skill_id, error, ..
        } => {
            i.text(&skill_id.0, 3)?;
            i.text(error, 3)?;
            i.nodes(1, 3)?;
        }
        kind => durable_tool_and_context(i, kind)?,
    }
    Ok(())
}

fn prompt_submission(
    i: &mut Inspector,
    value: &crate::PromptSubmission,
    depth: usize,
) -> Result<(), ObservationRetentionExceeded> {
    i.nodes(6, depth)?;
    i.text(&value.id.0, depth + 1)?;
    i.text(&value.text, depth + 1)?;
    i.text(&value.target.turn_id().0, depth + 2)?;
    admission(i, &value.admission, depth + 1)?;
    if let Some(descriptor) = &value.execution_descriptor {
        i.nodes(2, depth + 2)?;
        i.text(&descriptor.provider, depth + 3)?;
        i.text(&descriptor.model, depth + 3)?;
        i.text(&descriptor.config_identity, depth + 3)?;
        i.optional_text(descriptor.model_profile.as_ref(), depth + 3)?;
        i.optional_text(descriptor.auth_profile.as_ref(), depth + 3)?;
        if let Some(reasoning) = &descriptor.reasoning {
            i.nodes(1, depth + 4)?;
            i.optional_text(reasoning.effort.as_ref(), depth + 5)?;
            i.optional_text(reasoning.summary.as_ref(), depth + 5)?;
        }
    }
    Ok(())
}

fn durable_composition(
    i: &mut Inspector,
    kind: &SessionEventKind,
) -> Result<(), ObservationRetentionExceeded> {
    match kind {
        SessionEventKind::UserMessage {
            text,
            admission: metadata,
            ..
        } => {
            i.nodes(1, 3)?;
            i.text(text, 3)?;
            admission(i, metadata, 3)?;
        }
        SessionEventKind::SubmissionActivated {
            submission_id,
            turn_id,
            text,
            admission: metadata,
            ..
        } => {
            i.nodes(2, 3)?;
            i.text(&submission_id.0, 3)?;
            i.text(&turn_id.0, 3)?;
            i.text(text, 3)?;
            admission(i, metadata, 3)?;
        }
        SessionEventKind::SubmissionAccepted { submission } => {
            prompt_submission(i, submission, 3)?;
        }
        SessionEventKind::SubmissionContextObserved { submission_id, .. } => {
            i.nodes(2, 3)?;
            i.text(&submission_id.0, 3)?;
        }
        SessionEventKind::PermissionRequested {
            permission_id,
            tool_call_id,
            producer_plugin_id,
            tool_name,
            arguments_json,
            batch,
            policy_source,
            policy_reason,
        } => {
            for text in [permission_id, tool_call_id, tool_name, arguments_json] {
                i.text(text, 3)?;
            }
            for text in [producer_plugin_id, policy_source, policy_reason] {
                i.optional_text(text.as_ref(), 3)?;
            }
            i.nodes(1, 3)?;
            if let Some(batch) = batch {
                i.nodes(3, 4)?;
                i.text(&batch.batch_id, 5)?;
            }
        }
        SessionEventKind::ModelFeatureFidelityNegotiated { turn_id, feature } => {
            i.text(turn_id, 3)?;
            i.nodes(2, 3)?;
            for text in [
                &feature.family,
                &feature.feature,
                &feature.mechanism,
                &feature.fidelity,
                &feature.execution,
            ] {
                i.text(text, 4)?;
            }
        }
        kind => durable_child_and_results(i, kind)?,
    }
    Ok(())
}

/// Inspect a borrowed durable event before cloning it for observation.
///
/// # Errors
/// Returns an error on a structural limit, counter overflow, or a non-current
/// event schema.
pub fn inspect_durable(
    event: &SessionEvent,
    limits: &SessionObservationSourceLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    if event.schema_version != crate::CURRENT_SESSION_EVENT_SCHEMA_VERSION {
        return Err(ObservationRetentionExceeded);
    }
    let mut i = Inspector::new(limits)?;
    i.nodes(7, 1)?;
    if let Some(provenance) = &event.provenance {
        i.nodes(4, 2)?;
        i.optional_text(provenance.source_event_id.as_ref(), 3)?;
        i.optional_text(provenance.source_locator.as_ref(), 3)?;
    }
    i.nodes(1, 2)?;
    match &event.kind {
        SessionEventKind::SessionCreated {
            name,
            working_directory,
        } => {
            i.optional_text(name.as_ref(), 3)?;
            i.path(working_directory, 3)?;
        }
        SessionEventKind::ClientAttached { .. }
        | SessionEventKind::ClientDetached { .. }
        | SessionEventKind::SessionChildLinked { .. }
        | SessionEventKind::SessionChildrenReconciled { .. } => i.nodes(1, 3)?,
        SessionEventKind::SubmissionCancelled { submission_id } => i.text(&submission_id.0, 3)?,
        SessionEventKind::AssistantDelta { text }
        | SessionEventKind::AssistantMessage { text }
        | SessionEventKind::SystemMessage { text }
        | SessionEventKind::AssistantReasoningDelta { text }
        | SessionEventKind::AssistantReasoningMessage { text } => i.text(text, 3)?,
        SessionEventKind::AgentChanged { agent_id } => i.text(agent_id, 3)?,
        SessionEventKind::ModelTurnStarted { turn_id } => i.text(turn_id, 3)?,
        SessionEventKind::ModelTurnFinished {
            turn_id, message, ..
        } => {
            i.text(turn_id, 3)?;
            i.optional_text(message.as_ref(), 3)?;
            i.nodes(1, 3)?;
        }
        SessionEventKind::ModelTurnCancelRequested { turn_id, .. } => {
            i.text(turn_id, 3)?;
            i.nodes(2, 3)?;
        }
        SessionEventKind::ModelChanged {
            provider, model, ..
        } => {
            i.text(provider, 3)?;
            i.text(model, 3)?;
            i.nodes(1, 3)?;
        }
        SessionEventKind::PermissionResolved { permission_id, .. } => {
            i.text(permission_id, 3)?;
            i.nodes(1, 3)?;
        }
        SessionEventKind::ContextCompacted { summary, .. } => {
            i.text(summary, 3)?;
            i.nodes(1, 3)?;
        }
        SessionEventKind::SessionRenamed { name } => i.optional_text(name.as_ref(), 3)?,
        SessionEventKind::SessionChildPublished { title, .. } => {
            i.optional_text(title.as_ref(), 3)?;
            i.nodes(1, 3)?;
        }
        SessionEventKind::AutomaticTitleUpdated {
            evaluation_id,
            title,
        } => {
            i.text(evaluation_id, 3)?;
            i.text(title, 3)?;
        }
        kind => durable_content(&mut i, kind)?,
    }
    Ok(i.usage)
}

/// Inspect a borrowed live event before cloning it for observation.
///
/// # Errors
/// Returns an error on a structural limit or counter overflow.
pub fn inspect_live(
    event: &SessionLiveEvent,
    limits: &SessionObservationSourceLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let mut i = Inspector::new(limits)?;
    i.nodes(3, 1)?;
    match &event.kind {
        SessionLiveEventKind::AssistantTextDelta {
            turn_id,
            segment_id,
            text,
            ..
        } => {
            i.text(turn_id, 3)?;
            i.text(segment_id, 3)?;
            i.text(text, 3)?;
            i.nodes(1, 3)?;
        }
        SessionLiveEventKind::AssistantReasoningDelta { turn_id, text } => {
            i.text(turn_id, 3)?;
            i.text(text, 3)?;
        }
        SessionLiveEventKind::AssistantTextStreamUpdated {
            turn_id,
            segment_id,
            update,
            ..
        } => {
            i.text(turn_id, 3)?;
            i.text(segment_id, 3)?;
            i.nodes(2, 3)?;
            i.stream(update)?;
        }
        SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
            turn_id,
            activity_id,
            part_id,
            update,
            ..
        } => {
            i.text(turn_id, 3)?;
            i.text(activity_id, 3)?;
            i.text(part_id, 3)?;
            i.nodes(6, 3)?;
            i.stream(update)?;
        }
        SessionLiveEventKind::AssistantReasoningActivity { turn_id, event, .. } => {
            i.text(turn_id, 3)?;
            i.nodes(1, 3)?;
            i.reasoning(event, 3)?;
        }
        SessionLiveEventKind::ProviderStreamProgress { turn_id, event } => {
            i.text(turn_id, 3)?;
            provider_progress(&mut i, event, 3)?;
        }
        kind => live_tool_and_accounting(&mut i, kind)?,
    }
    Ok(i.usage)
}

fn trace_tool_payload(
    i: &mut Inspector,
    payload: &crate::SessionTracePayload,
    d: usize,
) -> Result<(), ObservationRetentionExceeded> {
    match payload {
        crate::SessionTracePayload::ToolInvocationStarted {
            tool_call_id,
            plugin_id,
            tool_name,
            side_effect,
            arguments,
            ..
        } => {
            i.nodes(1, d + 1)?;
            for text in [tool_call_id, plugin_id, tool_name, side_effect] {
                i.text(text, d + 1)?;
            }
            trace_blob(i, arguments.as_ref(), d + 1)?;
        }
        crate::SessionTracePayload::ToolPolicyEvaluated {
            tool_call_id,
            agent_id,
            decision,
            reason,
        } => {
            for text in [tool_call_id, agent_id, decision] {
                i.text(text, d + 1)?;
            }
            i.optional_text(reason.as_ref(), d + 1)?;
        }
        crate::SessionTracePayload::ToolPermissionWait {
            permission_id,
            tool_call_id,
            ..
        } => {
            i.nodes(2, d + 1)?;
            i.text(permission_id, d + 1)?;
            i.text(tool_call_id, d + 1)?;
        }
        crate::SessionTracePayload::ToolInvocationFinished {
            tool_call_id,
            output,
            ..
        } => {
            i.nodes(3, d + 1)?;
            i.text(tool_call_id, d + 1)?;
            trace_blob(i, output.as_ref(), d + 1)?;
        }
        crate::SessionTracePayload::ContextCompaction {
            reason, message, ..
        } => {
            i.nodes(2, d + 1)?;
            i.text(reason, d + 1)?;
            i.optional_text(message.as_ref(), d + 1)?;
        }
        crate::SessionTracePayload::ProviderStreamEvent(event) => {
            provider_progress(i, event, d + 1)?;
        }
        _ => return Err(ObservationRetentionExceeded),
    }
    Ok(())
}

fn durable_tool_and_context(
    i: &mut Inspector,
    kind: &SessionEventKind,
) -> Result<(), ObservationRetentionExceeded> {
    match kind {
        SessionEventKind::ToolContribution { event } => {
            i.nested(inspect_tool_contribution(event, &i.nested_limits(3)?)?, 3)?;
        }
        SessionEventKind::ToolContributionPlaced { envelope } => {
            i.nodes(3, 3)?;
            i.nested(
                inspect_tool_contribution(&envelope.contribution, &i.nested_limits(4)?)?,
                4,
            )?;
        }
        SessionEventKind::ToolExchangeRequested { request } => i.nested(
            inspect_tool_exchange_request(request, &i.nested_limits(3)?)?,
            3,
        )?,
        SessionEventKind::ToolExchangeResolved { event } => {
            i.nodes(1, 3)?;
            i.text(&event.invocation_id, 4)?;
            i.text(&event.exchange_id, 4)?;
            i.nested(
                inspect_tool_exchange_resolution(&event.resolution, &i.nested_limits(4)?)?,
                4,
            )?;
        }
        SessionEventKind::ToolInvocationLifecycle { event } => {
            i.nested(inspect_tool_lifecycle(event, &i.nested_limits(3)?)?, 3)?;
        }
        kind => durable_context(i, kind)?,
    }
    Ok(())
}

fn durable_context(
    i: &mut Inspector,
    kind: &SessionEventKind,
) -> Result<(), ObservationRetentionExceeded> {
    match kind {
        SessionEventKind::SkillInvoked {
            skill_id,
            arguments,
            source,
            ..
        } => {
            i.text(&skill_id.0, 3)?;
            i.text(arguments, 3)?;
            skill_source(i, source.as_ref(), 3)?;
            i.nodes(1, 3)?;
        }
        SessionEventKind::SkillActivated {
            skill_id, source, ..
        } => {
            i.text(&skill_id.0, 3)?;
            skill_source(i, source.as_ref(), 3)?;
            i.nodes(2, 3)?;
        }
        SessionEventKind::SkillContextLoaded {
            skill_id,
            source,
            preview,
            ..
        } => {
            i.text(&skill_id.0, 3)?;
            skill_source(i, source.as_ref(), 3)?;
            i.optional_text(preview.as_ref(), 3)?;
            i.nodes(3, 3)?;
        }
        SessionEventKind::ExecutionSessionCreated { provenance, .. } => {
            execution_provenance(i, provenance, 3)?;
            i.nodes(1, 3)?;
        }
        SessionEventKind::ReasoningChanged {
            effort,
            summary,
            model_scope,
        } => {
            i.optional_text(effort.as_ref(), 3)?;
            i.optional_text(summary.as_ref(), 3)?;
            i.nodes(1, 3)?;
            if let Some(scope) = model_scope {
                i.nodes(1, 4)?;
                i.text(&scope.provider, 5)?;
                i.text(&scope.model, 5)?;
            }
        }
        SessionEventKind::ModelUsage { turn_id, usage } => {
            i.text(turn_id, 3)?;
            i.nested(inspect_session_token_usage(usage, &i.nested_limits(3)?)?, 3)?;
        }
        SessionEventKind::RequestContextObserved { observation } => {
            context_observation(i, observation, 3)?;
        }
        SessionEventKind::ProviderContextCompacted { snapshot, .. } => {
            i.nodes(4, 3)?;
            i.optional_text(snapshot.request_fingerprint.as_ref(), 4)?;
            i.optional_text(snapshot.request_id.as_ref(), 4)?;
            i.optional_text(snapshot.auth_profile.as_ref(), 4)?;
            for text in [
                &snapshot.provider_plugin_id,
                &snapshot.model_id,
                &snapshot.compatibility_key,
                &snapshot.messages_json,
                &snapshot.portable_summary,
            ] {
                i.text(text, 4)?;
            }
        }
        kind => durable_composition(i, kind)?,
    }
    Ok(())
}

fn durable_child_and_results(
    i: &mut Inspector,
    kind: &SessionEventKind,
) -> Result<(), ObservationRetentionExceeded> {
    match kind {
        SessionEventKind::ChildSessionCreated { request, spawning } => {
            child_creation(i, request, 3)?;
            i.nodes(1, 3)?;
            if let Some(spawn) = spawning {
                i.nodes(4, 4)?;
                child_creation(i, &spawn.creation, 5)?;
                if let Some(path) = &spawn.worktree_directory {
                    i.path(path, 6)?;
                }
                i.text(&spawn.input, 5)?;
                i.optional_text(spawn.auth_profile.as_ref(), 5)?;
                admission(i, &spawn.admission, 5)?;
            }
        }
        SessionEventKind::MilestoneEvaluationRequested { request } => {
            i.nodes(4, 3)?;
            i.nodes(4, 4)?;
            i.text(&request.evaluation_id, 4)?;
            i.optional_text(request.expected_explicit_name.as_ref(), 4)?;
        }
        SessionEventKind::MilestonePublicationRequested { request } => {
            i.nodes(1, 3)?;
            milestone(i, &request.milestone, 4)?;
            i.optional_text(request.automatic_title.as_ref(), 4)?;
        }
        SessionEventKind::MilestonePublished { milestone: value } => milestone(i, value, 3)?,
        SessionEventKind::MilestoneEvaluationCompleted { completion } => {
            i.nodes(1, 3)?;
            i.text(&completion.evaluation_id, 4)?;
        }
        SessionEventKind::ToolInvocationResultRecorded { record } => {
            i.nodes(4, 3)?;
            i.text(&record.invocation_id, 4)?;
            i.text(&record.model_output, 4)?;
            i.collection(record.content.capacity(), 4)?;
            for content in &record.content {
                i.nodes(1, 5)?;
                match content {
                    crate::ToolResultContent::Text { text } => i.text(text, 6)?,
                    crate::ToolResultContent::Image { image } => {
                        i.nodes(2, 6)?;
                        i.text(&image.mime_type, 7)?;
                        i.text(&image.data_base64, 7)?;
                        i.nodes(4, 7)?;
                        i.optional_text(image.metadata.source_path.as_ref(), 8)?;
                    }
                    crate::ToolResultContent::ImageRef { image } => {
                        i.nodes(2, 6)?;
                        i.text(&image.path, 7)?;
                        i.text(&image.mime_type, 7)?;
                        i.optional_text(image.artifact_id.as_ref(), 7)?;
                        i.optional_text(image.reference_key.as_ref(), 7)?;
                        i.nodes(4, 7)?;
                        i.optional_text(image.metadata.source_path.as_ref(), 8)?;
                    }
                }
            }
            if let Some(presentation) = &record.presentation {
                i.nested(
                    inspect_tool_presentation(presentation, &i.nested_limits(5)?)?,
                    5,
                )?;
            }
            if let Some(result) = &record.result {
                tool_result(i, result, 5)?;
            }
        }
        SessionEventKind::TraceEvent { trace: value } => trace(i, value, 3)?,
        _ => return Err(ObservationRetentionExceeded),
    }
    Ok(())
}

fn durable_content(
    i: &mut Inspector,
    kind: &SessionEventKind,
) -> Result<(), ObservationRetentionExceeded> {
    match kind {
        SessionEventKind::WorkingDirectoryChanged {
            old_working_directory,
            new_working_directory,
        } => {
            i.path(old_working_directory, 3)?;
            i.path(new_working_directory, 3)?;
        }
        SessionEventKind::SessionImported {
            source_id,
            source_display_name,
            external_session_id,
            ..
        } => {
            i.text(source_id, 3)?;
            i.text(source_display_name, 3)?;
            i.text(external_session_id, 3)?;
            i.nodes(1, 3)?;
        }
        SessionEventKind::SessionDerived {
            producer,
            operation_kind,
            ..
        } => {
            i.text(producer, 3)?;
            i.text(operation_kind, 3)?;
            i.nodes(5, 3)?;
        }
        SessionEventKind::RalphLifecycle {
            loop_name,
            state_dir,
            kind,
            message,
            ..
        } => {
            i.text(loop_name, 3)?;
            i.path(state_dir, 3)?;
            i.text(kind, 3)?;
            i.text(message, 3)?;
            i.nodes(1, 3)?;
        }
        kind => durable_output(i, kind)?,
    }
    Ok(())
}

fn reasoning_activity(
    i: &mut Inspector,
    value: &crate::ReasoningActivity,
    depth: usize,
) -> Result<(), ObservationRetentionExceeded> {
    i.nodes(6, depth)?;
    i.text(&value.activity_id, depth + 1)?;
    i.collection(value.parts.capacity(), depth + 1)?;
    for part in &value.parts {
        i.nodes(4, depth + 2)?;
        i.text(&part.part_id, depth + 3)?;
        i.text(&part.text, depth + 3)?;
    }
    Ok(())
}

fn durable_output(
    i: &mut Inspector,
    kind: &SessionEventKind,
) -> Result<(), ObservationRetentionExceeded> {
    match kind {
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            producer_plugin_id,
            tool_name,
            arguments_json,
            working_directory,
        }
        | SessionEventKind::PositionedToolCallRequested {
            tool_call_id,
            producer_plugin_id,
            tool_name,
            arguments_json,
            working_directory,
            ..
        } => {
            i.text(tool_call_id, 3)?;
            i.optional_text(producer_plugin_id.as_ref(), 3)?;
            i.text(tool_name, 3)?;
            i.text(arguments_json, 3)?;
            i.nodes(2, 3)?;
            if let Some(path) = working_directory {
                i.path(path, 4)?;
            }
        }
        SessionEventKind::AssistantResponseSegment {
            turn_id,
            segment_id,
            text,
            ..
        }
        | SessionEventKind::PositionedAssistantResponseSegment {
            turn_id,
            segment_id,
            text,
            ..
        } => {
            i.text(turn_id, 3)?;
            i.text(segment_id, 3)?;
            i.text(text, 3)?;
            i.nodes(2, 3)?;
        }
        SessionEventKind::AssistantReasoningActivity { turn_id, activity }
        | SessionEventKind::PositionedAssistantReasoningActivity {
            turn_id, activity, ..
        } => {
            i.text(turn_id, 3)?;
            reasoning_activity(i, activity, 3)?;
        }
        SessionEventKind::PluginStatusNote {
            plugin_id,
            note_id,
            text,
            metadata,
        } => {
            i.text(plugin_id, 3)?;
            i.text(note_id, 3)?;
            i.text(text, 3)?;
            i.collection(metadata.len(), 3)?;
            for (key, value) in metadata {
                i.text(key, 4)?;
                i.json(value, 4)?;
            }
        }
        SessionEventKind::InertHistory {
            event_type,
            payload,
        } => {
            i.text(event_type, 3)?;
            i.json(payload, 3)?;
        }
        kind => durable_runtime(i, kind)?,
    }
    Ok(())
}

fn live_tool_and_accounting(
    i: &mut Inspector,
    kind: &SessionLiveEventKind,
) -> Result<(), ObservationRetentionExceeded> {
    match kind {
        SessionLiveEventKind::ToolContributionPlaced { envelope } => {
            i.nodes(3, 3)?;
            i.nested(
                inspect_tool_contribution(&envelope.contribution, &i.nested_limits(4)?)?,
                4,
            )?;
        }
        SessionLiveEventKind::ToolPresentationUpdated { update } => {
            i.nested(inspect_tool_presentation(update, &i.nested_limits(3)?)?, 3)?;
        }
        SessionLiveEventKind::ToolInvocationProgress { event } => {
            i.nested(inspect_tool_lifecycle(event, &i.nested_limits(3)?)?, 3)?;
        }
        SessionLiveEventKind::ToolRequestDraft { event } => {
            i.nodes(10, 3)?;
            i.text(&event.turn_id, 4)?;
            i.text(&event.tool_call_id, 4)?;
            i.text(&event.tool_name, 4)?;
            i.optional_text(event.producer_plugin_id.as_ref(), 4)?;
            i.text(&event.schema, 4)?;
            match &event.operation {
                crate::ToolRequestDraftOperation::Append { text, .. }
                | crate::ToolRequestDraftOperation::Checkpoint { text, .. } => {
                    i.nodes(2, 4)?;
                    i.text(text, 5)?;
                }
                crate::ToolRequestDraftOperation::Remove { .. } => i.nodes(2, 4)?,
            }
        }
        SessionLiveEventKind::RequestContextOccupancyChanged { occupancy } => {
            i.nodes(1, 3)?;
            if let Some(occupancy) = occupancy.as_ref() {
                i.nested(
                    inspect_context_occupancy(occupancy, &i.nested_limits(4)?)?,
                    4,
                )?;
            }
        }
        SessionLiveEventKind::UsageSummaryChanged { summary } => {
            i.nodes(10, 3)?;
            i.collection(summary.totals_micros.len(), 4)?;
            for currency in summary.totals_micros.keys() {
                i.text(currency, 5)?;
                i.nodes(1, 5)?;
            }
            if let Some(usage) = &summary.latest_usage {
                i.nested(inspect_session_token_usage(usage, &i.nested_limits(5)?)?, 5)?;
            }
        }
        _ => return Err(ObservationRetentionExceeded),
    }
    Ok(())
}

/// Inspect a borrowed `ToolArtifact`, root depth one and no event copy.
///
/// # Errors
/// Returns an error on structural resource exhaustion or checked counter overflow.
pub fn inspect_tool_artifact(
    value: &crate::ToolArtifact,
    limits: &ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let mut i = standalone(limits);
    tool_artifact(&mut i, value, 1)?;
    Ok(i.usage)
}

/// Inspect a borrowed `ActivityPresentation`, root depth one and no event copy.
///
/// # Errors
/// Returns an error on structural resource exhaustion or checked counter overflow.
pub fn inspect_activity_presentation(
    value: &crate::ActivityPresentation,
    limits: &ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let mut i = standalone(limits);
    activity_presentation(&mut i, value, 1)?;
    Ok(i.usage)
}

/// Inspect a borrowed `PromptSubmission`, root depth one and no event copy.
///
/// # Errors
/// Returns an error on structural resource exhaustion or checked counter overflow.
pub fn inspect_prompt_submission(
    value: &crate::PromptSubmission,
    limits: &ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let mut i = standalone(limits);
    prompt_submission(&mut i, value, 1)?;
    Ok(i.usage)
}

/// Inspect a borrowed `ReasoningActivity`, root depth one and no event copy.
///
/// # Errors
/// Returns an error on structural resource exhaustion or checked counter overflow.
pub fn inspect_reasoning_activity(
    value: &crate::ReasoningActivity,
    limits: &ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let mut i = standalone(limits);
    reasoning_activity(&mut i, value, 1)?;
    Ok(i.usage)
}

/// Inspect a borrowed `ReasoningActivityEvent`, root depth one and no event copy.
///
/// # Errors
/// Returns an error on structural resource exhaustion or checked counter overflow.
pub fn inspect_reasoning_event(
    value: &crate::ReasoningActivityEvent,
    limits: &ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let mut i = standalone(limits);
    i.reasoning(value, 1)?;
    Ok(i.usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> SessionObservationSourceLimits {
        SessionObservationSourceLimits {
            max_scalar_capacity_bytes: 1024,
            max_collection_entries: 1024,
            max_value_nodes: 1024,
            max_depth: 16,
            max_queued_events: 2,
            max_in_flight_events: 1,
        }
    }

    #[test]
    fn standalone_artifact_and_reasoning_count_spare_vector_capacity() {
        let artifact = crate::ToolArtifact {
            artifact_id: String::new(),
            producer_plugin_id: String::new(),
            schema: String::new(),
            schema_version: 1,
            tool_call_id: None,
            title: None,
            metadata: serde_json::Value::Null,
            refs: Vec::with_capacity(2048),
        };
        let policy = limits().retention_limits().unwrap();
        assert_eq!(
            inspect_tool_artifact(&artifact, &policy),
            Err(ObservationRetentionExceeded)
        );
        let reasoning = crate::ReasoningActivity {
            activity_id: String::new(),
            order: 0,
            status: crate::ReasoningActivityStatus::Completed,
            parts: Vec::with_capacity(2048),
            opaque: false,
        };
        assert_eq!(
            inspect_reasoning_activity(&reasoning, &policy),
            Err(ObservationRetentionExceeded)
        );
    }

    #[test]
    fn standalone_activity_submission_and_reasoning_preserve_string_capacity() {
        let policy = limits().retention_limits().unwrap();
        let activity = crate::ActivityPresentation {
            version: 1,
            producer: String::new(),
            activity_id: String::new(),
            revision: 1,
            schema: String::new(),
            schema_version: 1,
            fallback: String::with_capacity(2048),
            payload: serde_json::Value::Null,
        };
        assert_eq!(
            inspect_activity_presentation(&activity, &policy),
            Err(ObservationRetentionExceeded)
        );
        let submission = crate::PromptSubmission {
            id: crate::SubmissionId(String::new()),
            client_id: crate::ClientId::new(),
            text: String::with_capacity(2048),
            admission: crate::TurnAdmissionMetadata::default(),
            requested_placement: crate::SubmissionPlacement::FollowUp,
            target: crate::SubmissionTarget::OwnTurn {
                turn_id: crate::TurnId(String::new()),
            },
            execution_descriptor: None,
        };
        assert_eq!(
            inspect_prompt_submission(&submission, &policy),
            Err(ObservationRetentionExceeded)
        );
        let reasoning = crate::ReasoningActivityEvent::Started {
            activity_id: String::with_capacity(2048),
            order: 0,
        };
        assert_eq!(
            inspect_reasoning_event(&reasoning, &policy),
            Err(ObservationRetentionExceeded)
        );
    }

    #[test]
    fn standalone_helpers_have_no_event_copy_and_relative_root_depth() {
        let mut policy = limits().retention_limits().unwrap();
        policy.copies = 0;
        let reasoning = crate::ReasoningActivityEvent::Started {
            activity_id: "id".to_owned(),
            order: 0,
        };
        let usage = inspect_reasoning_event(&reasoning, &policy).unwrap();
        assert_eq!(usage.copies, 0);
        assert_eq!(usage.depth, 2);
        assert_eq!(usage.scalar_capacity_bytes, 2);
        let artifact = crate::ToolArtifact {
            artifact_id: String::new(),
            producer_plugin_id: String::new(),
            schema: String::new(),
            schema_version: 1,
            tool_call_id: None,
            title: None,
            metadata: serde_json::Value::Null,
            refs: Vec::new(),
        };
        let usage = inspect_tool_artifact(&artifact, &policy).unwrap();
        assert_eq!(usage.copies, 0);
        assert_eq!(usage.depth, 2);
        policy.depth = 1;
        assert_eq!(
            inspect_tool_artifact(&artifact, &policy),
            Err(ObservationRetentionExceeded)
        );
    }

    #[test]
    fn admission_vector_spare_capacity_is_checked_without_encoding() {
        let mut metadata = crate::TurnAdmissionMetadata::default();
        metadata.execution.tool_allowlist = Some(Vec::with_capacity(2048));
        let event = SessionEvent {
            schema_version: crate::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 0,
            timestamp_ms: 0,
            session_id: crate::SessionId::new(),
            provenance: None,
            kind: SessionEventKind::UserMessage {
                client_id: crate::ClientId::new(),
                text: String::new(),
                admission: metadata,
            },
        };
        assert_eq!(
            inspect_durable(&event, &limits()),
            Err(ObservationRetentionExceeded)
        );
    }

    #[test]
    fn ordinary_admission_and_usage_are_supported_under_structural_policy() {
        let event = SessionEvent {
            schema_version: crate::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 0,
            timestamp_ms: 0,
            session_id: crate::SessionId::new(),
            provenance: None,
            kind: SessionEventKind::UserMessage {
                client_id: crate::ClientId::new(),
                text: "hello".to_owned(),
                admission: crate::TurnAdmissionMetadata::default(),
            },
        };
        let usage = inspect_durable(&event, &limits()).unwrap();
        assert_eq!(usage.copies, 1);
        assert_eq!(usage.scalar_capacity_bytes, 5);
        let token_usage = crate::SessionTokenUsage {
            request_id: Some(String::with_capacity(2048)),
            ..crate::SessionTokenUsage::default()
        };
        assert_eq!(
            inspect_session_token_usage(&token_usage, &limits().retention_limits().unwrap()),
            Err(ObservationRetentionExceeded)
        );
    }

    #[test]
    fn submission_identity_capacity_is_not_lost_in_wrapper() {
        let event = SessionEvent {
            schema_version: crate::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 0,
            timestamp_ms: 0,
            session_id: crate::SessionId::new(),
            provenance: None,
            kind: SessionEventKind::SubmissionCancelled {
                submission_id: crate::SubmissionId(String::with_capacity(2048)),
            },
        };
        assert_eq!(
            inspect_durable(&event, &limits()),
            Err(ObservationRetentionExceeded)
        );
    }

    #[test]
    fn tool_presentation_capacity_and_nested_depth_are_admitted_before_copy() {
        let mut id = String::with_capacity(4096);
        id.push('x');
        let value = crate::ToolPresentationUpdate {
            invocation_id: id,
            producer_id: String::new(),
            generation: 0,
            revision: 1,
            identity: crate::ToolPresentationIdentity::Primary,
            retention: crate::ToolPresentationRetention::RetainLatest,
            schema: String::new(),
            schema_version: 1,
            artifact: None,
            payload: serde_json::Value::Null,
        };
        assert_eq!(
            inspect_tool_presentation(&value, &limits().retention_limits().unwrap()),
            Err(ObservationRetentionExceeded)
        );
        let event = SessionLiveEvent {
            session_id: crate::SessionId::new(),
            kind: SessionLiveEventKind::ToolPresentationUpdated { update: value },
        };
        assert_eq!(
            inspect_live(&event, &limits()),
            Err(ObservationRetentionExceeded)
        );
    }

    #[test]
    fn tool_exchange_payload_slots_preserve_spare_capacity() {
        let value = crate::ToolExchangeRequest {
            invocation_id: String::new(),
            exchange_id: String::new(),
            producer_id: String::new(),
            schema: String::new(),
            schema_version: 1,
            payload: serde_json::Value::Array(Vec::with_capacity(2048)),
            response_policy: crate::ToolExchangeResponsePolicy::Optional,
        };
        assert_eq!(
            inspect_tool_exchange_request(&value, &limits().retention_limits().unwrap()),
            Err(ObservationRetentionExceeded)
        );
    }

    #[test]
    fn unknown_durable_schema_is_not_interpreted_as_current() {
        let event = SessionEvent {
            schema_version: crate::CURRENT_SESSION_EVENT_SCHEMA_VERSION + 1,
            sequence: 0,
            timestamp_ms: 0,
            session_id: crate::SessionId::new(),
            provenance: None,
            kind: SessionEventKind::AssistantMessage {
                text: String::new(),
            },
        };
        assert_eq!(
            inspect_durable(&event, &limits()),
            Err(ObservationRetentionExceeded)
        );
    }

    #[test]
    fn standalone_text_stream_has_no_event_copy_and_counts_spare_capacity() {
        let mut text = String::with_capacity(128);
        text.push('x');
        let value = crate::TextStreamUpdate {
            generation: 0,
            first_revision: 1,
            revision: 1,
            operation: TextStreamOperation::Append {
                expected_offset: 0,
                text,
            },
        };
        let mut policy = limits().retention_limits().unwrap();
        policy.copies = 0;
        let usage = inspect_text_stream(&value, &policy).unwrap();
        assert_eq!(usage.copies, 0);
        assert_eq!(usage.scalar_capacity_bytes, 128);
        assert_eq!(usage.depth, 3);
        policy.scalar_capacity_bytes = 127;
        assert_eq!(
            inspect_text_stream(&value, &policy),
            Err(ObservationRetentionExceeded)
        );
    }

    #[test]
    fn borrowed_string_spare_capacity_is_rejected_before_clone() {
        let mut text = String::with_capacity(4096);
        text.push('x');
        let event = SessionLiveEvent {
            session_id: crate::SessionId::new(),
            kind: SessionLiveEventKind::AssistantReasoningDelta {
                turn_id: String::new(),
                text,
            },
        };
        assert_eq!(
            inspect_live(&event, &limits()),
            Err(ObservationRetentionExceeded)
        );
    }

    #[test]
    fn json_spare_collection_capacity_is_rejected() {
        let values = Vec::with_capacity(2048);
        let event = SessionEvent {
            schema_version: crate::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 0,
            timestamp_ms: 0,
            session_id: crate::SessionId::new(),
            provenance: None,
            kind: SessionEventKind::InertHistory {
                event_type: String::new(),
                payload: serde_json::Value::Array(values),
            },
        };
        assert_eq!(
            inspect_durable(&event, &limits()),
            Err(ObservationRetentionExceeded)
        );
    }

    #[test]
    fn nested_depth_and_event_copy_are_accounted() {
        let event = SessionLiveEvent {
            session_id: crate::SessionId::new(),
            kind: SessionLiveEventKind::AssistantReasoningDelta {
                turn_id: "turn".to_owned(),
                text: "text".to_owned(),
            },
        };
        let usage = inspect_live(&event, &limits()).unwrap();
        assert_eq!(usage.copies, 1);
        assert_eq!(usage.scalar_capacity_bytes, 8);
        let mut tiny = limits();
        tiny.max_depth = 2;
        assert_eq!(
            inspect_live(&event, &tiny),
            Err(ObservationRetentionExceeded)
        );
    }
}
