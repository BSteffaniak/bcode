//! Session-owned structural accounting used by semantic observation views.

pub use bcode_session_models::retention::{
    ObservationRetentionExceeded, ObservationRetentionLimits, ObservationRetentionUsage,
    SessionObservationSourceLimits,
};

use crate::TranscriptViewItemKind as Kind;

struct Walker {
    usage: ObservationRetentionUsage,
    limits: ObservationRetentionLimits,
}

impl Walker {
    fn new(
        limits: ObservationRetentionLimits,
        nodes: usize,
    ) -> Result<Self, ObservationRetentionExceeded> {
        let usage = ObservationRetentionUsage {
            value_nodes: nodes,
            depth: 1,
            ..ObservationRetentionUsage::default()
        };
        usage.check(limits)?;
        Ok(Self { usage, limits })
    }

    fn text(&mut self, text: &String, depth: usize) -> Result<(), ObservationRetentionExceeded> {
        self.usage
            .try_add(ObservationRetentionUsage::string(text, depth), self.limits)
    }

    fn optional_text(
        &mut self,
        text: Option<&String>,
        depth: usize,
    ) -> Result<(), ObservationRetentionExceeded> {
        self.usage.try_add(
            ObservationRetentionUsage {
                value_nodes: 1,
                depth,
                ..ObservationRetentionUsage::default()
            },
            self.limits,
        )?;
        if let Some(text) = text {
            self.text(
                text,
                depth.checked_add(1).ok_or(ObservationRetentionExceeded)?,
            )?;
        }
        Ok(())
    }

    fn nested(
        &mut self,
        mut usage: ObservationRetentionUsage,
        containing_depth: usize,
    ) -> Result<(), ObservationRetentionExceeded> {
        usage.depth = usage
            .depth
            .checked_add(containing_depth)
            .ok_or(ObservationRetentionExceeded)?;
        self.usage.try_add(usage, self.limits)
    }
}

/// Inspect every owned composer field using capacity, not serialized length.
///
/// # Errors
/// Returns an error on any exceeded structural dimension or counter overflow.
pub fn inspect_composer(
    value: &crate::ComposerViewState,
    limits: ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let crate::ComposerViewState {
        draft,
        can_submit: _,
        disabled_reason,
    } = value;
    let mut walker = Walker::new(limits, 2)?;
    walker.text(draft, 2)?;
    walker.optional_text(disabled_reason.as_ref(), 2)?;
    Ok(walker.usage)
}

/// Inspect every owned interaction field, including JSON and terminal resolution.
///
/// # Errors
/// Returns an error on an owning nested inspection failure or structural limit.
pub fn inspect_interaction(
    value: &crate::InteractionViewSummary,
    limits: ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let crate::InteractionViewSummary {
        interaction_id,
        producer_id,
        exchange_schema,
        exchange_schema_version: _,
        kind,
        tool_call_id,
        title,
        required: _,
        snapshot,
        state: _,
        status_detail,
        resolved: _,
        resolution,
    } = value;
    let mut walker = Walker::new(limits, 6)?;
    walker.text(interaction_id, 2)?;
    walker.text(kind, 2)?;
    for text in [
        producer_id.as_ref(),
        exchange_schema.as_ref(),
        tool_call_id.as_ref(),
        title.as_ref(),
        status_detail.as_ref(),
    ] {
        walker.optional_text(text, 2)?;
    }
    walker.usage.try_add(
        ObservationRetentionUsage {
            value_nodes: 2,
            depth: 2,
            ..ObservationRetentionUsage::default()
        },
        limits,
    )?;
    if let Some(snapshot) = snapshot {
        walker.nested(ObservationRetentionUsage::json(snapshot, limits)?, 2)?;
    }
    if let Some(resolution) = resolution {
        walker.nested(
            bcode_session_models::retention_events::inspect_tool_exchange_resolution(
                resolution, &limits,
            )?,
            2,
        )?;
    }
    Ok(walker.usage)
}

/// Inspect all owned runtime selections, usage, occupancy and provider progress.
///
/// # Errors
/// Returns an error on an owning nested inspection failure or structural limit.
pub fn inspect_runtime(
    value: &crate::SessionRuntimeViewState,
    limits: ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let crate::SessionRuntimeViewState {
        provider_plugin_id,
        requested_model_id,
        display_name,
        effective_model_id,
        agent_id,
        reasoning_effort,
        reasoning_summary,
        context_occupancy,
        cumulative_metered_tokens: _,
        cost,
        latest_usage,
        active_turn_id,
        cancelling: _,
        last_turn_outcome: _,
        last_turn_message,
        provider_progress,
        submission_queue,
        submission_queue_unavailable: _,
    } = value;
    let mut walker = Walker::new(limits, 4)?;
    walker.nested(inspect_cost(cost, limits)?, 1)?;
    for text in [
        provider_plugin_id.as_ref(),
        requested_model_id.as_ref(),
        display_name.as_ref(),
        effective_model_id.as_ref(),
        agent_id.as_ref(),
        reasoning_effort.as_ref(),
        reasoning_summary.as_ref(),
        active_turn_id.as_ref(),
        last_turn_message.as_ref(),
    ] {
        walker.optional_text(text, 2)?;
    }
    walker.usage.try_add(
        ObservationRetentionUsage {
            value_nodes: 3,
            depth: 2,
            ..ObservationRetentionUsage::default()
        },
        limits,
    )?;
    if let Some(occupancy) = context_occupancy {
        walker.nested(
            bcode_session_models::retention_events::inspect_context_occupancy(occupancy, &limits)?,
            2,
        )?;
    }
    if let Some(usage) = latest_usage {
        walker.nested(
            bcode_session_models::retention_events::inspect_session_token_usage(usage, &limits)?,
            2,
        )?;
    }
    if let Some(queue) = submission_queue {
        inspect_submission_queue(&mut walker, queue)?;
    }
    if let Some(progress) = provider_progress {
        let crate::ProviderProgressView {
            turn_id,
            detail,
            retry_at_unix: _,
        } = progress;
        walker.usage.try_add(
            ObservationRetentionUsage {
                value_nodes: 2,
                depth: 3,
                ..ObservationRetentionUsage::default()
            },
            limits,
        )?;
        walker.text(turn_id, 4)?;
        walker.text(detail, 4)?;
    }
    Ok(walker.usage)
}

fn inspect_submission_queue(
    walker: &mut Walker,
    value: &bcode_session_models::SubmissionQueueStatus,
) -> Result<(), ObservationRetentionExceeded> {
    let bcode_session_models::SubmissionQueueStatus { oldest, blocker: _ } = value;
    walker.nested(
        ObservationRetentionUsage {
            value_nodes: 3,
            depth: 2,
            ..Default::default()
        },
        1,
    )?;
    if let Some(record) = oldest {
        let bcode_session_models::SubmissionRecord {
            receipt,
            submission,
            status,
            context_observation: _,
            recovery_authorization,
        } = record;
        walker.text(&receipt.submission_id.0, 5)?;
        walker.nested(
            bcode_session_models::retention_events::inspect_prompt_submission(
                submission,
                &walker.limits,
            )?,
            4,
        )?;
        if let Some(recipe) = submission
            .execution_descriptor
            .as_ref()
            .and_then(|d| d.recovery_recipe.as_deref())
        {
            inspect_recovery_recipe(walker, recipe, 8)?;
        }
        if let bcode_session_models::SubmissionStatus::Activated { turn_id, .. } = status {
            walker.text(&turn_id.0, 6)?;
        }
        if let Some(authorization) = recovery_authorization {
            let bcode_session_models::SubmissionExecutionDescriptor {
                version: _,
                provider,
                model,
                reasoning,
                model_profile,
                auth_profile,
                config_identity,
                recovery_recipe,
            } = &authorization.descriptor;
            for text in [provider, model, config_identity] {
                walker.text(text, 7)?;
            }
            for text in [model_profile.as_ref(), auth_profile.as_ref()] {
                walker.optional_text(text, 7)?;
            }
            if let Some(reasoning) = reasoning {
                walker.optional_text(reasoning.effort.as_ref(), 8)?;
                walker.optional_text(reasoning.summary.as_ref(), 8)?;
            }
            if let Some(recipe) = recovery_recipe {
                inspect_recovery_recipe(walker, recipe, 8)?;
            }
        }
    }
    Ok(())
}

fn inspect_recovery_recipe(
    walker: &mut Walker,
    recipe: &bcode_session_models::SubmissionRecoveryRecipe,
    depth: usize,
) -> Result<(), ObservationRetentionExceeded> {
    let bcode_session_models::SubmissionRecoveryRecipe {
        version: _,
        config_source,
        model_profile,
        auth_profile,
        auth_pool,
    } = recipe;
    for text in [
        model_profile.as_ref(),
        auth_profile.as_ref(),
        auth_pool.as_ref(),
    ] {
        walker.optional_text(text, depth + 1)?;
    }
    let bcode_session_models::SubmissionConfigSource {
        schema_version: _,
        sources,
        execution_digest,
        source_presence,
    } = config_source;
    walker.text(execution_digest, depth + 2)?;
    let bcode_session_models::SubmissionConfigSources {
        cwd,
        discovered_paths,
        exclusive_config_path,
        base_config_path,
        env_config_path,
        cli_config_path,
        inline_overrides: _,
        active_context,
        model_profile,
    } = sources;
    walker.usage.try_add(
        ObservationRetentionUsage {
            collection_entries: source_presence
                .capacity()
                .checked_add(discovered_paths.capacity())
                .ok_or(ObservationRetentionExceeded)?,
            value_nodes: source_presence
                .len()
                .checked_add(16)
                .ok_or(ObservationRetentionExceeded)?,
            depth: depth + 4,
            ..Default::default()
        },
        walker.limits,
    )?;
    // Collection capacity is checked before traversing its potentially large contents.
    for path in std::iter::once(cwd)
        .chain(discovered_paths.iter())
        .chain(exclusive_config_path.iter())
        .chain(base_config_path.iter())
        .chain(env_config_path.iter())
        .chain(cli_config_path.iter())
    {
        walker.usage.try_add(
            ObservationRetentionUsage {
                scalar_capacity_bytes: path.capacity(),
                value_nodes: 1,
                depth: depth + 4,
                ..Default::default()
            },
            walker.limits,
        )?;
    }
    walker.optional_text(active_context.as_ref(), depth + 3)?;
    walker.optional_text(model_profile.as_ref(), depth + 3)?;
    Ok(())
}

/// Inspect currency-keyed cost retention without assigning B-tree byte sizes.
///
/// # Errors
/// Returns an error on a structural limit or arithmetic overflow.
pub fn inspect_cost(
    value: &crate::SessionCostSummary,
    limits: ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let crate::SessionCostSummary {
        original_usage_incomplete: _,
        revision: _,
        totals_micros,
        estimated_usage_count: _,
        unavailable_usage_count: _,
        observed_usage_count: _,
    } = value;
    let mut walker = Walker::new(limits, 6)?;
    walker.usage.try_add(
        ObservationRetentionUsage::collection(totals_micros.len(), 2),
        limits,
    )?;
    for currency in totals_micros.keys() {
        walker.text(currency, 3)?;
        walker.usage.try_add(
            ObservationRetentionUsage {
                value_nodes: 1,
                depth: 3,
                ..ObservationRetentionUsage::default()
            },
            limits,
        )?;
    }
    Ok(walker.usage)
}

/// Inspect every retained request-draft scalar and its optional output location.
///
/// # Errors
/// Returns an error on a structural limit or overflow.
pub fn inspect_tool_draft(
    value: &crate::ToolRequestDraftView,
    limits: ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let crate::ToolRequestDraftView {
        output_location,
        turn_id,
        tool_call_id,
        tool_name,
        producer_plugin_id,
        schema,
        schema_version: _,
        placement: _,
        generation: _,
        revision: _,
        argument_bytes: _,
        preview_start_offset: _,
        preview,
        truncated: _,
    } = value;
    let mut walker = Walker::new(limits, 8)?;
    for text in [turn_id, tool_call_id, tool_name, schema, preview] {
        walker.text(text, 2)?;
    }
    walker.optional_text(producer_plugin_id.as_ref(), 2)?;
    walker.usage.try_add(
        ObservationRetentionUsage {
            value_nodes: 1,
            depth: 2,
            ..ObservationRetentionUsage::default()
        },
        limits,
    )?;
    if let Some(location) = output_location {
        let crate::TurnOutputLocation {
            turn_id,
            position: _,
        } = location;
        walker.usage.try_add(
            ObservationRetentionUsage {
                value_nodes: 2,
                depth: 3,
                ..ObservationRetentionUsage::default()
            },
            limits,
        )?;
        walker.text(turn_id, 4)?;
    }
    Ok(walker.usage)
}

/// Inspect authorization presentation strings and batch correlation.
///
/// # Errors
/// Returns an error on a structural limit or overflow.
pub fn inspect_permission(
    value: &crate::PermissionView,
    limits: ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let crate::PermissionView {
        permission_id,
        session_id: _,
        tool_call_id,
        tool_name,
        arguments_json,
        batch,
        agent_id,
        title,
        policy_source,
        detail,
        resolved: _,
        approved: _,
        can_remember: _,
    } = value;
    let mut walker = Walker::new(limits, 5)?;
    for text in [
        permission_id,
        tool_call_id,
        tool_name,
        arguments_json,
        agent_id,
    ] {
        walker.text(text, 2)?;
    }
    for text in [title.as_ref(), policy_source.as_ref(), detail.as_ref()] {
        walker.optional_text(text, 2)?;
    }
    walker.usage.try_add(
        ObservationRetentionUsage {
            value_nodes: 1,
            depth: 2,
            ..ObservationRetentionUsage::default()
        },
        limits,
    )?;
    if let Some(batch) = batch {
        let crate::PermissionBatchView {
            batch_id,
            call_index: _,
            call_count: _,
        } = batch;
        walker.usage.try_add(
            ObservationRetentionUsage {
                value_nodes: 2,
                depth: 3,
                ..ObservationRetentionUsage::default()
            },
            limits,
        )?;
        walker.text(batch_id, 4)?;
    }
    Ok(walker.usage)
}

/// Inspect the current reasoning text without imposing a text-lifetime ceiling.
///
/// # Errors
/// Returns an error on a structural limit or overflow.
pub fn inspect_thinking(
    value: &crate::ThinkingViewState,
    limits: ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let crate::ThinkingViewState {
        visible: _,
        mode: _,
        active_text,
        streaming: _,
    } = value;
    let mut walker = Walker::new(limits, 3)?;
    walker.optional_text(active_text.as_ref(), 2)?;
    Ok(walker.usage)
}

/// Inspect runtime-work identity, presentation label, and optional progress text.
///
/// # Errors
/// Returns an error on a structural limit or overflow.
pub fn inspect_runtime_work(
    value: &crate::RuntimeWorkView,
    limits: ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let crate::RuntimeWorkView {
        work_id,
        kind: _,
        label,
        status: _,
        cancellable: _,
        message,
        completed_units: _,
        total_units: _,
        updated_at_ms: _,
    } = value;
    let mut walker = Walker::new(limits, 7)?;
    walker.text(&work_id.0, 2)?;
    walker.text(label, 2)?;
    walker.optional_text(message.as_ref(), 2)?;
    Ok(walker.usage)
}

/// Inspect selected reasoning activity text and all retained part identities.
///
/// # Errors
/// Returns an error on a structural limit or overflow.
pub fn inspect_reasoning(
    value: &crate::ReasoningActivityView,
    limits: ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let crate::ReasoningActivityView {
        turn_id,
        activity_id,
        order: _,
        status: _,
        parts,
        opaque: _,
        readable_parts_filtered: _,
        finished: _,
    } = value;
    let mut walker = Walker::new(limits, 5)?;
    walker.text(turn_id, 2)?;
    walker.text(activity_id, 2)?;
    walker.usage.try_add(
        ObservationRetentionUsage::collection(parts.len(), 2),
        limits,
    )?;
    for part in parts {
        let bcode_session_models::ReasoningPart {
            part_id,
            kind: _,
            role: _,
            order: _,
            text,
        } = part;
        walker.usage.try_add(
            ObservationRetentionUsage {
                value_nodes: 3,
                depth: 3,
                ..ObservationRetentionUsage::default()
            },
            limits,
        )?;
        walker.text(part_id, 3)?;
        walker.text(text, 3)?;
    }
    Ok(walker.usage)
}

/// Inspect an invocation's raw result and generic artifact copy independently.
///
/// # Errors
/// Returns an error on an owning inspection failure, structural limit, or overflow.
pub fn inspect_tool(
    value: &crate::ToolInvocationView,
    limits: ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let crate::ToolInvocationView {
        tool_call_id,
        producer_plugin_id,
        tool_name,
        arguments_json,
        working_directory,
        request_draft,
        status: _,
        result_text,
        is_error: _,
        result,
        presentation,
        timing,
    } = value;
    let mut walker = Walker::new(limits, 7)?;
    walker.text(tool_call_id, 2)?;
    for text in [
        producer_plugin_id.as_ref(),
        tool_name.as_ref(),
        arguments_json.as_ref(),
        result_text.as_ref(),
    ] {
        walker.optional_text(text, 2)?;
    }
    walker.usage.try_add(
        ObservationRetentionUsage {
            value_nodes: 4,
            depth: 2,
            ..ObservationRetentionUsage::default()
        },
        limits,
    )?;
    if let Some(path) = working_directory {
        walker.usage.try_add(
            ObservationRetentionUsage {
                scalar_capacity_bytes: path.capacity(),
                value_nodes: 1,
                depth: 3,
                ..ObservationRetentionUsage::default()
            },
            limits,
        )?;
    }
    if let Some(draft) = request_draft {
        walker.nested(inspect_tool_draft(draft, limits)?, 2)?;
    }
    let crate::ToolTimingView {
        started_at_ms: _,
        finished_at_ms: _,
        timeout_ms: _,
        timed_out: _,
        duration_ms: _,
    } = timing;
    walker.usage.try_add(
        ObservationRetentionUsage {
            value_nodes: 5,
            depth: 3,
            ..ObservationRetentionUsage::default()
        },
        limits,
    )?;
    if let Some(result) = result {
        match result {
            crate::ToolResultView::Text { text } => walker.text(text, 4)?,
            crate::ToolResultView::Json { value } => walker.text(value, 4)?,
            crate::ToolResultView::Artifact { artifact } => {
                let crate::ToolArtifactView {
                    artifact,
                    generic_payload,
                } = artifact;
                walker.nested(
                    bcode_session_models::retention_events::inspect_tool_artifact(
                        artifact, &limits,
                    )?,
                    3,
                )?;
                walker.nested(ObservationRetentionUsage::json(generic_payload, limits)?, 3)?;
            }
        }
    }
    if let Some(presentation) = presentation {
        inspect_tool_presentation(presentation, &mut walker)?;
    }
    Ok(walker.usage)
}

fn inspect_tool_presentation(
    presentation: &crate::ToolPresentationView,
    walker: &mut Walker,
) -> Result<(), ObservationRetentionExceeded> {
    let limits = walker.limits;
    let crate::ToolPresentationView {
        producer_id,
        generation: _,
        revision: _,
        retention: _,
        schema,
        schema_version: _,
        artifact,
        payload,
    } = presentation;
    walker.usage.try_add(
        ObservationRetentionUsage {
            value_nodes: 5,
            depth: 3,
            ..ObservationRetentionUsage::default()
        },
        limits,
    )?;
    walker.text(producer_id, 4)?;
    walker.text(schema, 4)?;
    if let Some(artifact) = artifact {
        let bcode_session_models::ToolContributionArtifact {
            artifact_id,
            reference_key,
            content_type,
            storage_uri,
            committed_bytes: _,
            revision: _,
            finalized: _,
            availability,
        } = artifact;
        for text in [artifact_id, reference_key, storage_uri] {
            walker.text(text, 5)?;
        }
        walker.optional_text(content_type.as_ref(), 5)?;
        walker.optional_text(availability.as_ref(), 5)?;
        walker.usage.try_add(
            ObservationRetentionUsage {
                value_nodes: 3,
                depth: 5,
                ..ObservationRetentionUsage::default()
            },
            limits,
        )?;
    }
    walker.nested(ObservationRetentionUsage::json(payload, limits)?, 3)?;
    Ok(())
}

/// Inspect chat text, submission annotations, and the independently retained activity envelope.
///
/// # Errors
/// Returns an error on an owning inspection failure, structural limit, or overflow.
pub fn inspect_chat(
    value: &crate::ChatMessageView,
    limits: ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let crate::ChatMessageView {
        submission_context,
        submission_status: _,
        activity,
        text,
        display_label,
        format: _,
    } = value;
    let mut walker = Walker::new(limits, 4)?;
    walker.text(text, 2)?;
    walker.optional_text(display_label.as_ref(), 2)?;
    if let Some(context) = submission_context {
        let bcode_session_models::SubmissionContextObservation {
            issue: _,
            event_sequence: _,
        } = context;
        walker.usage.try_add(
            ObservationRetentionUsage {
                value_nodes: 2,
                depth: 3,
                ..ObservationRetentionUsage::default()
            },
            limits,
        )?;
    }
    if let Some(activity) = activity {
        let crate::ActivityMessageView {
            source_sequence: _,
            exact_instructions,
            instruction_bytes: _,
            execution,
            presentation,
        } = activity.as_ref();
        walker.usage.try_add(
            ObservationRetentionUsage {
                value_nodes: 3,
                depth: 3,
                ..ObservationRetentionUsage::default()
            },
            limits,
        )?;
        walker.optional_text(exact_instructions.as_ref(), 4)?;
        let bcode_session_models::TurnExecutionCorrelation {
            execution_id,
            unit_id,
            attempt: _,
        } = execution;
        walker.text(execution_id, 5)?;
        walker.text(unit_id, 5)?;
        walker.usage.try_add(
            ObservationRetentionUsage {
                value_nodes: 1,
                depth: 5,
                ..ObservationRetentionUsage::default()
            },
            limits,
        )?;
        walker.nested(
            bcode_session_models::retention_events::inspect_activity_presentation(
                presentation,
                &limits,
            )?,
            3,
        )?;
    }
    Ok(walker.usage)
}

/// Inspect a transcript item using exhaustive semantic variants and borrowed nested inspectors.
///
/// # Errors
/// Returns an error on a structural limit, nested rejection, or overflow.
pub fn inspect_transcript_item(
    value: &crate::TranscriptViewItem,
    limits: ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let crate::TranscriptViewItem {
        id,
        revision: _,
        sequence: _,
        timestamp_ms: _,
        output_location,
        streaming: _,
        kind,
    } = value;
    let mut walker = Walker::new(limits, 6)?;
    walker.usage.try_add(id.retention_usage(2), limits)?;
    if let Some(location) = output_location {
        let crate::TurnOutputLocation {
            turn_id,
            position: _,
        } = location;
        walker.text(turn_id, 4)?;
        walker.usage.try_add(
            ObservationRetentionUsage {
                value_nodes: 1,
                depth: 4,
                ..ObservationRetentionUsage::default()
            },
            limits,
        )?;
    }
    inspect_transcript_kind(kind, &mut walker)?;
    Ok(walker.usage)
}

fn inspect_transcript_kind(
    kind: &Kind,
    walker: &mut Walker,
) -> Result<(), ObservationRetentionExceeded> {
    let limits = walker.limits;
    match kind {
        Kind::UserMessage { message }
        | Kind::AssistantMessage { message }
        | Kind::ReasoningMessage { message }
        | Kind::SystemMessage { message } => walker.nested(inspect_chat(message, limits)?, 2)?,
        Kind::ReasoningActivity { activity } => {
            walker.nested(inspect_reasoning(activity, limits)?, 2)?;
        }
        Kind::ToolInvocation { tool } | Kind::ToolRequest { tool } => {
            walker.nested(inspect_tool(tool, limits)?, 2)?;
        }
        Kind::ToolRequestDraft { draft } => walker.nested(inspect_tool_draft(draft, limits)?, 2)?,
        Kind::Permission { permission } => {
            walker.nested(inspect_permission(permission, limits)?, 2)?;
        }
        Kind::Interaction { interaction } => {
            walker.nested(inspect_interaction(interaction, limits)?, 2)?;
        }
        Kind::Compaction { compaction } => {
            let crate::CompactionView {
                status: _,
                text,
                provider_plugin_id,
                model_id,
            } = compaction;
            walker.text(text, 3)?;
            walker.optional_text(provider_plugin_id.as_ref(), 3)?;
            walker.optional_text(model_id.as_ref(), 3)?;
            walker.usage.try_add(
                ObservationRetentionUsage {
                    value_nodes: 1,
                    depth: 3,
                    ..ObservationRetentionUsage::default()
                },
                limits,
            )?;
        }
        Kind::Skill { skill } => {
            let crate::SkillView {
                skill_id,
                status: _,
                text,
            } = skill;
            walker.text(skill_id, 3)?;
            walker.text(text, 3)?;
            walker.usage.try_add(
                ObservationRetentionUsage {
                    value_nodes: 1,
                    depth: 3,
                    ..ObservationRetentionUsage::default()
                },
                limits,
            )?;
        }
        Kind::ToolContribution {
            contribution,
            placement: _,
            invocation,
        } => {
            walker.nested(
                bcode_session_models::retention_events::inspect_tool_contribution(
                    contribution,
                    &limits,
                )?,
                2,
            )?;
            walker.usage.try_add(
                ObservationRetentionUsage {
                    value_nodes: 2,
                    depth: 3,
                    ..ObservationRetentionUsage::default()
                },
                limits,
            )?;
            if let Some(invocation) = invocation {
                walker.nested(inspect_tool(invocation, limits)?, 3)?;
            }
        }
    }
    Ok(())
}

/// Clone a standalone semantic snapshot under aggregate pre-allocation admission.
///
/// The ledger includes the source and all enclosing owners. The returned snapshot
/// adds its inspected usage plus one copy reservation, released only after drop.
///
/// # Errors
/// Rejects exhaustion, overflow, inconsistent accounting, or clone capacity beyond
/// the pre-admitted source capacities. Rejection leaves the caller ledger unchanged.
pub fn clone_snapshot_bounded(
    value: &crate::SessionViewSnapshot,
    retained: &mut ObservationRetentionUsage,
    limits: ObservationRetentionLimits,
) -> Result<crate::SessionViewSnapshot, ObservationRetentionExceeded> {
    let mut charge = inspect_snapshot(value, limits)?;
    let mut next = *retained;
    let mut source_check = next;
    source_check.try_release(charge)?;
    charge.copies = 1;
    next.try_add(charge, limits)?;
    let cloned = value.clone();
    let mut actual = inspect_snapshot(&cloned, limits)?;
    actual.copies = 1;
    if actual.scalar_capacity_bytes > charge.scalar_capacity_bytes
        || actual.collection_entries > charge.collection_entries
        || actual.value_nodes > charge.value_nodes
        || actual.depth > charge.depth
    {
        return Err(ObservationRetentionExceeded);
    }
    next.try_release(charge)?;
    next.try_add(actual, limits)?;
    *retained = next;
    Ok(cloned)
}

/// Inspect the complete retained semantic snapshot without serializing or cloning it.
///
/// # Errors
/// Returns an error on an owning nested rejection, structural limit, or overflow.
pub fn inspect_snapshot(
    value: &crate::SessionViewSnapshot,
    limits: ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let crate::SessionViewSnapshot {
        schema_version: _,
        revision: _,
        session_id: _,
        title,
        working_directory,
        latest_sequence: _,
        connection_status,
        catalog_status,
        notice,
        transcript: _,
        text_streams: _,
        contributions: _,
        active_exchanges: _,
        active_invocations: _,
        tools: _,
        permissions: _,
        runtime_work: _,
        active_skills: _,
        plugin_status: _,
        composer: _,
        thinking: _,
        runtime: _,
        interactions: _,
        session_summary: _,
    } = value;
    let mut walker = Walker::new(limits, 8)?;
    walker.optional_text(title.as_ref(), 2)?;
    if let Some(path) = working_directory {
        walker.usage.try_add(
            ObservationRetentionUsage {
                scalar_capacity_bytes: path.capacity(),
                value_nodes: 1,
                depth: 3,
                ..ObservationRetentionUsage::default()
            },
            limits,
        )?;
    }
    if let crate::SessionConnectionViewStatus::Error(text) = connection_status {
        walker.text(text, 3)?;
    }
    if let crate::SessionCatalogViewStatus::Degraded(text)
    | crate::SessionCatalogViewStatus::Failed(text) = catalog_status
    {
        walker.text(text, 3)?;
    }
    if let Some(notice) = notice {
        let crate::SessionViewNotice { level: _, message } = notice;
        walker.text(message, 4)?;
        walker.usage.try_add(
            ObservationRetentionUsage {
                value_nodes: 1,
                depth: 4,
                ..ObservationRetentionUsage::default()
            },
            limits,
        )?;
    }
    inspect_snapshot_transcript(value, &mut walker)?;
    inspect_snapshot_tools(value, &mut walker)?;
    inspect_snapshot_application(value, &mut walker)?;
    Ok(walker.usage)
}

fn inspect_snapshot_transcript(
    value: &crate::SessionViewSnapshot,
    walker: &mut Walker,
) -> Result<(), ObservationRetentionExceeded> {
    let limits = walker.limits;
    let transcript = &value.transcript;
    let text_streams = &value.text_streams;
    let crate::TranscriptViewDocument {
        revision: _,
        items,
        source_start_sequence: _,
        source_end_sequence: _,
        has_older_history: _,
        has_newer_history: _,
    } = transcript;
    walker.usage.try_add(
        ObservationRetentionUsage::collection(items.capacity(), 3),
        limits,
    )?;
    for item in items {
        walker.nested(inspect_transcript_item(item, limits)?, 3)?;
    }
    walker.usage.try_add(
        ObservationRetentionUsage::collection(text_streams.len(), 2),
        limits,
    )?;
    for (id, stream) in text_streams {
        walker.usage.try_add(id.retention_usage(3), limits)?;
        let crate::TextStreamViewState {
            generation: _,
            revision: _,
            accepted_bytes: _,
            truncated: _,
            status: _,
        } = stream;
        walker.usage.try_add(
            ObservationRetentionUsage {
                value_nodes: 5,
                depth: 4,
                ..ObservationRetentionUsage::default()
            },
            limits,
        )?;
    }
    Ok(())
}

fn inspect_snapshot_tools(
    value: &crate::SessionViewSnapshot,
    walker: &mut Walker,
) -> Result<(), ObservationRetentionExceeded> {
    let limits = walker.limits;
    let contributions = &value.contributions;
    let active_exchanges = &value.active_exchanges;
    let active_invocations = &value.active_invocations;
    let tools = &value.tools;
    walker.usage.try_add(
        ObservationRetentionUsage::collection(contributions.len(), 2),
        limits,
    )?;
    for (key, value) in contributions {
        walker.text(key, 3)?;
        walker.nested(
            bcode_session_models::retention_events::inspect_tool_contribution(value, &limits)?,
            2,
        )?;
    }
    walker.usage.try_add(
        ObservationRetentionUsage::collection(active_exchanges.len(), 2),
        limits,
    )?;
    for (key, value) in active_exchanges {
        walker.text(key, 3)?;
        walker.nested(
            bcode_session_models::retention_events::inspect_tool_exchange_request(value, &limits)?,
            2,
        )?;
    }
    walker.usage.try_add(
        ObservationRetentionUsage::collection(active_invocations.len(), 2),
        limits,
    )?;
    for (key, value) in active_invocations {
        walker.text(key, 3)?;
        walker.nested(
            bcode_session_models::retention_events::inspect_tool_lifecycle(value, &limits)?,
            2,
        )?;
    }
    walker.usage.try_add(
        ObservationRetentionUsage::collection(tools.len(), 2),
        limits,
    )?;
    for (key, value) in tools {
        walker.text(key, 3)?;
        walker.nested(inspect_tool(value, limits)?, 2)?;
    }
    Ok(())
}

fn inspect_snapshot_application(
    value: &crate::SessionViewSnapshot,
    walker: &mut Walker,
) -> Result<(), ObservationRetentionExceeded> {
    let limits = walker.limits;
    let permissions = &value.permissions;
    let runtime_work = &value.runtime_work;
    let active_skills = &value.active_skills;
    let plugin_status = &value.plugin_status;
    let composer = &value.composer;
    let thinking = &value.thinking;
    let runtime = &value.runtime;
    let interactions = &value.interactions;
    let session_summary = &value.session_summary;
    walker.usage.try_add(
        ObservationRetentionUsage::collection(permissions.capacity(), 2),
        limits,
    )?;
    for value in permissions {
        walker.nested(inspect_permission(value, limits)?, 2)?;
    }
    walker.usage.try_add(
        ObservationRetentionUsage::collection(runtime_work.capacity(), 2),
        limits,
    )?;
    for value in runtime_work {
        walker.nested(inspect_runtime_work(value, limits)?, 2)?;
    }
    walker.usage.try_add(
        ObservationRetentionUsage::collection(active_skills.len(), 2),
        limits,
    )?;
    for value in active_skills {
        walker.text(value, 3)?;
    }
    walker.usage.try_add(
        ObservationRetentionUsage::collection(plugin_status.len(), 2),
        limits,
    )?;
    for (key, value) in plugin_status {
        walker.text(key, 3)?;
        walker.nested(value.retention_usage(limits)?, 2)?;
    }
    walker.nested(inspect_composer(composer, limits)?, 1)?;
    walker.nested(inspect_thinking(thinking, limits)?, 1)?;
    walker.nested(inspect_runtime(runtime, limits)?, 1)?;
    walker.usage.try_add(
        ObservationRetentionUsage::collection(interactions.capacity(), 2),
        limits,
    )?;
    for value in interactions {
        walker.nested(inspect_interaction(value, limits)?, 2)?;
    }
    if let Some(summary) = session_summary {
        walker.nested(
            bcode_session_models::retention_events::inspect_session_summary(summary, &limits)?,
            2,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> ObservationRetentionLimits {
        ObservationRetentionLimits {
            scalar_capacity_bytes: 4096,
            collection_entries: 128,
            value_nodes: 1024,
            depth: 32,
            copies: 4,
        }
    }

    #[test]
    fn runtime_queue_retention_rejects_hidden_submission_capacity() {
        let mut value = crate::SessionRuntimeViewState::default();
        let submission = bcode_session_models::PromptSubmission {
            id: bcode_session_models::SubmissionId("queued".into()),
            client_id: bcode_session_models::ClientId::new(),
            text: String::with_capacity(8192),
            admission: bcode_session_models::TurnAdmissionMetadata::default(),
            requested_placement: bcode_session_models::SubmissionPlacement::FollowUp,
            target: bcode_session_models::SubmissionTarget::OwnTurn {
                turn_id: bcode_session_models::TurnId("turn".into()),
            },
            execution_descriptor: None,
        };
        value.submission_queue = Some(bcode_session_models::SubmissionQueueStatus {
            oldest: Some(bcode_session_models::SubmissionRecord {
                receipt: bcode_session_models::SubmissionReceipt {
                    submission_id: submission.id.clone(),
                    accepted_event_sequence: 1,
                },
                submission,
                status: bcode_session_models::SubmissionStatus::Pending,
                context_observation: None,
                recovery_authorization: None,
            }),
            blocker: None,
        });
        assert_eq!(
            inspect_runtime(&value, limits()),
            Err(ObservationRetentionExceeded)
        );
        value
            .submission_queue
            .as_mut()
            .unwrap()
            .oldest
            .as_mut()
            .unwrap()
            .submission
            .text = "small".into();
        assert!(inspect_runtime(&value, limits()).is_ok());
    }

    #[test]
    fn cost_rebuild_peak_denial_restores_staging_ledger() {
        let tokens = bcode_session_models::SessionTokenUsage {
            cost: Some(bcode_session_models::SessionCostEstimate::Estimated {
                currency: "USD".into(),
                total_micros: 7,
                components: Vec::new(),
                source: String::new(),
                revision: None,
            }),
            ..bcode_session_models::SessionTokenUsage::default()
        };
        let mut retained = ObservationRetentionUsage::default();
        let values = [&tokens, &tokens];
        let rebuilt = crate::SessionCostSummary::rebuild_admitted(
            values.into_iter(),
            &mut retained,
            limits(),
        )
        .unwrap();
        assert_eq!(
            rebuilt,
            crate::SessionCostSummary::rebuild(values.into_iter())
        );
        let before = retained;
        let denied = ObservationRetentionLimits {
            scalar_capacity_bytes: retained.scalar_capacity_bytes,
            ..limits()
        };
        assert!(
            crate::SessionCostSummary::rebuild_admitted(values.into_iter(), &mut retained, denied)
                .is_err()
        );
        assert_eq!(retained, before);
    }

    #[test]
    fn currency_entry_admission_matches_ordinary_and_rejects_before_mutation() {
        let tokens = bcode_session_models::SessionTokenUsage {
            cost: Some(bcode_session_models::SessionCostEstimate::Estimated {
                currency: "USD".into(),
                total_micros: 7,
                components: Vec::new(),
                source: String::new(),
                revision: None,
            }),
            ..bcode_session_models::SessionTokenUsage::default()
        };
        let mut cost = crate::SessionCostSummary::default();
        let mut retained = inspect_cost(&cost, limits()).unwrap();
        let before = retained;
        assert!(
            cost.observe_admitted(
                &tokens,
                &mut retained,
                ObservationRetentionLimits {
                    collection_entries: 0,
                    ..limits()
                }
            )
            .is_err()
        );
        assert_eq!(cost, crate::SessionCostSummary::default());
        assert_eq!(retained, before);
        assert!(
            cost.observe_admitted(
                &tokens,
                &mut retained,
                ObservationRetentionLimits {
                    scalar_capacity_bytes: 2,
                    ..limits()
                }
            )
            .is_err()
        );
        assert_eq!(retained, before);
        let mut ordinary = cost.clone();
        ordinary.observe(&tokens);
        cost.observe_admitted(&tokens, &mut retained, limits())
            .unwrap();
        assert_eq!(cost, ordinary);
        let after_first = retained;
        ordinary.observe(&tokens);
        cost.observe_admitted(&tokens, &mut retained, limits())
            .unwrap();
        assert_eq!(cost, ordinary);
        assert_eq!(retained, after_first);
    }

    #[test]
    fn snapshot_checks_hidden_capacity_and_nested_transcript_text() {
        let mut snapshot = crate::SessionViewSnapshot::empty();
        let mut text = String::with_capacity(512);
        text.push('x');
        let capacity = text.capacity();
        snapshot.transcript.items.push(crate::TranscriptViewItem {
            id: crate::TranscriptViewItemId::new(String::new()),
            revision: 0,
            sequence: None,
            timestamp_ms: None,
            output_location: None,
            streaming: false,
            kind: crate::TranscriptViewItemKind::UserMessage {
                message: crate::ChatMessageView::plain(text),
            },
        });
        assert_eq!(
            inspect_snapshot(&snapshot, limits())
                .unwrap()
                .scalar_capacity_bytes,
            capacity
        );
        assert!(
            inspect_snapshot(
                &snapshot,
                ObservationRetentionLimits {
                    scalar_capacity_bytes: capacity - 1,
                    ..limits()
                }
            )
            .is_err()
        );
        assert!(
            inspect_snapshot(
                &snapshot,
                ObservationRetentionLimits {
                    depth: 3,
                    ..limits()
                }
            )
            .is_err()
        );
        snapshot.transcript.items.clear();
        assert_eq!(
            inspect_snapshot(&snapshot, limits())
                .unwrap()
                .scalar_capacity_bytes,
            0
        );
        assert!(
            inspect_snapshot(
                &snapshot,
                ObservationRetentionLimits {
                    collection_entries: 0,
                    ..limits()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn draft_preview_capacity_and_optional_location_depth_are_charged() {
        let mut preview = String::with_capacity(512);
        preview.push('x');
        let capacity = preview.capacity();
        let draft = crate::ToolRequestDraftView {
            output_location: None,
            turn_id: String::new(),
            tool_call_id: String::new(),
            tool_name: String::new(),
            producer_plugin_id: None,
            schema: String::new(),
            schema_version: 1,
            placement: bcode_session_models::ToolContributionPlacement::Request,
            generation: 1,
            revision: 1,
            argument_bytes: 1,
            preview_start_offset: 0,
            preview,
            truncated: false,
        };
        assert_eq!(
            inspect_tool_draft(&draft, limits())
                .unwrap()
                .scalar_capacity_bytes,
            capacity
        );
        assert!(
            inspect_tool_draft(
                &draft,
                ObservationRetentionLimits {
                    scalar_capacity_bytes: capacity - 1,
                    ..limits()
                }
            )
            .is_err()
        );
        assert!(
            inspect_tool_draft(
                &draft,
                ObservationRetentionLimits {
                    depth: 1,
                    ..limits()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn permission_batch_and_optional_detail_capacity_are_retained() {
        let mut batch_id = String::with_capacity(256);
        batch_id.push('b');
        let mut detail = String::with_capacity(512);
        detail.push('d');
        let capacity = batch_id.capacity() + detail.capacity();
        let permission = crate::PermissionView {
            permission_id: String::new(),
            session_id: None,
            tool_call_id: String::new(),
            tool_name: String::new(),
            arguments_json: String::new(),
            agent_id: String::new(),
            batch: Some(crate::PermissionBatchView {
                batch_id,
                call_index: 0,
                call_count: 1,
            }),
            title: None,
            policy_source: None,
            detail: Some(detail),
            resolved: false,
            approved: None,
            can_remember: false,
        };
        assert_eq!(
            inspect_permission(&permission, limits())
                .unwrap()
                .scalar_capacity_bytes,
            capacity
        );
        assert!(
            inspect_permission(
                &permission,
                ObservationRetentionLimits {
                    depth: 3,
                    ..limits()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn composer_spare_capacity_is_charged() {
        let mut draft = String::with_capacity(512);
        draft.push('x');
        let capacity = draft.capacity();
        let composer = crate::ComposerViewState {
            draft,
            can_submit: true,
            disabled_reason: None,
        };
        assert_eq!(
            inspect_composer(&composer, limits())
                .unwrap()
                .scalar_capacity_bytes,
            capacity
        );
        assert!(
            inspect_composer(
                &composer,
                ObservationRetentionLimits {
                    scalar_capacity_bytes: 128,
                    ..limits()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn runtime_selection_capacity_and_containment_are_counted() {
        let mut id = String::with_capacity(256);
        id.push('a');
        let capacity = id.capacity();
        let runtime = crate::SessionRuntimeViewState {
            active_turn_id: Some(id),
            ..crate::SessionRuntimeViewState::default()
        };
        let usage = inspect_runtime(&runtime, limits()).unwrap();
        assert_eq!(usage.scalar_capacity_bytes, capacity);
        assert!(
            inspect_runtime(
                &runtime,
                ObservationRetentionLimits {
                    depth: 2,
                    ..limits()
                }
            )
            .is_err()
        );
    }
}
