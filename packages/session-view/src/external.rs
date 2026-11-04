//! Bounded managed-external presentation. No native process or plugin is needed to read history.

use std::fmt::Write as _;

use bcode_harness_models::{
    HarnessActivity, HarnessAttention, HarnessCapabilities, HarnessCapability, HarnessCompleteness,
    HarnessConnectivity, HarnessContinuation, HarnessContractError, HarnessExecutionStatus,
    HarnessInstance, HarnessInteraction, HarnessInteractionKind, HarnessObservation,
    HarnessObservationKind, HarnessSessionBinding, HarnessSupportLevel,
};
use bcode_session_view_models::external::{
    ExternalActionKind, ExternalActionView, ExternalActivityView, ExternalChoiceView,
    ExternalInteractionView, ExternalProvenanceView, ExternalSessionMetadataView,
    ExternalSessionView, ExternalTranscriptCategory, ExternalTranscriptItemView,
};

/// Absolute page ceiling; lifetime history remains incrementally accessible.
pub const MAX_EXTERNAL_VIEW_ITEMS: usize = 128;
/// Maximum retained UTF-8 content per portable page.
pub const MAX_EXTERNAL_VIEW_BYTES: usize = 256 * 1024;

/// Presentation freshness supplied by the application, not inferred from parentage or idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalInteractionAvailability {
    /// Exact current host-owned request, admitted for this responder.
    Admitted,
    /// Historical observation, not a current request.
    Historical,
    /// Plugin disabled or otherwise unavailable.
    Unavailable,
    /// Ownership generation no longer matches.
    Stale,
    /// A matching generation does not independently grant response authority.
    NotAuthorized,
}

/// Validate an already-projected page before renderer delivery, without full history replay.
///
/// # Errors
/// Rejects excessive entry counts or total retained UTF-8 bytes; never silently drops history.
pub fn validate_external_view(view: &ExternalSessionView) -> Result<(), HarnessContractError> {
    if view
        .transcript
        .len()
        .saturating_add(view.interactions.len())
        > MAX_EXTERNAL_VIEW_ITEMS
        || view.actions.len() > 6
        || view.interactions.iter().any(|item| item.choices.len() > 32)
    {
        return Err(HarnessContractError::BoundsExceeded);
    }
    let mut bytes = provenance_bytes(&view.provenance)
        .saturating_add(view.metadata.executable.len())
        .saturating_add(view.metadata.native_version.len())
        .saturating_add(view.metadata.config_namespace.len());
    for value in [
        &view.activity.connectivity,
        &view.activity.execution,
        &view.activity.attention,
        &view.activity.continuation,
        &view.activity.completeness,
    ] {
        bytes = bytes.saturating_add(value.len());
    }
    for item in &view.transcript {
        bytes = bytes
            .saturating_add(item.text.len())
            .saturating_add(item.observation_id.len())
            .saturating_add(provenance_bytes(&item.provenance));
    }
    for item in &view.interactions {
        bytes = bytes
            .saturating_add(item.prompt.len())
            .saturating_add(item.request_id.len())
            .saturating_add(item.scope.len())
            .saturating_add(item.kind.len())
            .saturating_add(item.response.reason.len())
            .saturating_add(provenance_bytes(&item.provenance));
        for choice in &item.choices {
            bytes = bytes
                .saturating_add(choice.id.len())
                .saturating_add(choice.label.len());
        }
    }
    for action in &view.actions {
        bytes = bytes.saturating_add(action.reason.len());
    }
    if bytes > MAX_EXTERNAL_VIEW_BYTES {
        return Err(HarnessContractError::BoundsExceeded);
    }
    Ok(())
}

const fn provenance_bytes(provenance: &ExternalProvenanceView) -> usize {
    provenance
        .session_id
        .len()
        .saturating_add(provenance.instance_id.len())
        .saturating_add(provenance.plugin_id.len())
        .saturating_add(provenance.native_session_id.len())
}

/// Derive non-secret retained launch metadata. Does not inspect native state or credentials.
/// # Errors
/// Rejects invalid contracts or mismatched instance identity.
pub fn external_metadata(
    instance: &HarnessInstance,
    binding: &HarnessSessionBinding,
) -> Result<(ExternalProvenanceView, ExternalSessionMetadataView), HarnessContractError> {
    instance.validate()?;
    binding.validate()?;
    if instance.instance_id != binding.instance_id {
        return Err(HarnessContractError::WrongOwner);
    }
    Ok((
        ExternalProvenanceView {
            session_id: binding.session_id.clone(),
            instance_id: binding.instance_id.clone(),
            plugin_id: instance.plugin_id.clone(),
            native_session_id: binding.native_session_id.clone(),
            generation: binding.generation,
        },
        ExternalSessionMetadataView {
            executable: instance.executable.clone(),
            native_version: binding.native_version.clone(),
            config_namespace: instance.config_namespace.clone(),
        },
    ))
}

/// Orthogonal status labels. Result/idle/disconnect do not independently mean completion.
#[must_use]
pub fn external_activity(status: &HarnessExecutionStatus) -> ExternalActivityView {
    ExternalActivityView {
        connectivity: match status.connectivity {
            HarnessConnectivity::Connected => "connected",
            HarnessConnectivity::Disconnected => "disconnected",
            HarnessConnectivity::Unknown => "connectivity unknown",
        }
        .into(),
        execution: match status.activity {
            HarnessActivity::Idle => "idle (not assignment completion)",
            HarnessActivity::Running => "running",
            HarnessActivity::Interrupted => "interrupted (effects may remain)",
            HarnessActivity::Settled => "native settled (not assignment acceptance)",
            HarnessActivity::Unknown => "execution unknown",
        }
        .into(),
        attention: match status.attention {
            HarnessAttention::None => "no observed attention",
            HarnessAttention::Approval => "native approval needed",
            HarnessAttention::Question => "question needs answer",
            HarnessAttention::Assistance => "assistance needed",
            HarnessAttention::Authentication => "authentication required (use dedicated auth flow)",
            HarnessAttention::Unknown => "attention unknown",
        }
        .into(),
        continuation: match status.continuation {
            HarnessContinuation::Available => "native continuation available",
            HarnessContinuation::Unavailable => {
                "native continuation unavailable; history remains readable"
            }
            HarnessContinuation::Unknown => "native continuation unknown; history remains readable",
        }
        .into(),
        completeness: match status.completeness {
            HarnessCompleteness::Complete => "observations complete",
            HarnessCompleteness::Partial => "observations partial",
            HarnessCompleteness::Gap => "observation gap",
            HarnessCompleteness::Unknown => "observation coverage unknown",
        }
        .into(),
    }
}

/// Capability-aware control. Constrained capabilities still require explicit application admission.
#[must_use]
pub fn external_action(
    action: ExternalActionKind,
    capability: &HarnessCapability,
    availability: ExternalInteractionAvailability,
) -> ExternalActionView {
    let unavailable = match availability {
        ExternalInteractionAvailability::Admitted => None,
        ExternalInteractionAvailability::Historical => {
            Some("historical observation; not a current request")
        }
        ExternalInteractionAvailability::Unavailable => {
            Some("harness plugin unavailable; history remains readable")
        }
        ExternalInteractionAvailability::Stale => {
            Some("stale ownership generation; refresh required")
        }
        ExternalInteractionAvailability::NotAuthorized => Some("response authority not admitted"),
    };
    let unsupported = match capability.support {
        HarnessSupportLevel::Unsupported => Some("native capability unsupported"),
        HarnessSupportLevel::Unknown => Some("native capability unknown"),
        HarnessSupportLevel::Constrained if capability.constraints.is_empty() => {
            Some("invalid capability constraints")
        }
        HarnessSupportLevel::Supported | HarnessSupportLevel::Constrained => None,
    };
    ExternalActionView {
        action,
        enabled: unavailable.is_none() && unsupported.is_none(),
        reason: unavailable
            .or(unsupported)
            .map_or_else(|| capability.constraints.join("; "), str::to_owned),
    }
}

/// Project a normalized interaction with provenance and explicit host admission evidence.
/// # Errors
/// Rejects invalid interaction/capability contracts or mismatched source provenance.
pub fn external_interaction(
    interaction: &HarnessInteraction,
    provenance: &ExternalProvenanceView,
    capabilities: &HarnessCapabilities,
    availability: ExternalInteractionAvailability,
) -> Result<ExternalInteractionView, HarnessContractError> {
    interaction.validate()?;
    capabilities.validate()?;
    if interaction.session_id != provenance.session_id
        || interaction.generation != provenance.generation
    {
        return Err(HarnessContractError::WrongOwner);
    }
    let (kind, action, capability) = match interaction.kind {
        HarnessInteractionKind::Approval => (
            "Native approval",
            ExternalActionKind::Approve,
            &capabilities.approval,
        ),
        HarnessInteractionKind::Question => (
            "Question",
            ExternalActionKind::Respond,
            &capabilities.questions,
        ),
        HarnessInteractionKind::Assistance => (
            "Assistance",
            ExternalActionKind::Respond,
            &capabilities.assistance,
        ),
    };
    Ok(ExternalInteractionView {
        request_id: interaction.request_id.clone(),
        provenance: provenance.clone(),
        kind: kind.into(),
        prompt: interaction.prompt.clone(),
        scope: interaction.scope.clone(),
        choices: interaction
            .choices
            .iter()
            .map(|choice| ExternalChoiceView {
                id: choice.choice_id.clone(),
                label: choice.label.clone(),
            })
            .collect(),
        response: external_action(action, capability, availability),
    })
}

/// Project one canonical normalized observation. Sequence is supplied by canonical persistence,
/// never synthesized from native timestamps. Vendor payloads are intentionally not exposed.
/// # Errors
/// Rejects invalid observations or mismatched canonical source provenance.
pub fn external_transcript_item(
    observation: &HarnessObservation,
    sequence: u64,
    provenance: &ExternalProvenanceView,
) -> Result<ExternalTranscriptItemView, HarnessContractError> {
    observation.validate()?;
    if observation.session_id != provenance.session_id
        || observation.generation != provenance.generation
    {
        return Err(HarnessContractError::WrongOwner);
    }
    let (category, text) = external_observation_content(&observation.kind);
    Ok(ExternalTranscriptItemView {
        observation_id: observation.observation_id.clone(),
        sequence,
        provenance: provenance.clone(),
        category,
        text,
        incomplete: observation.completeness != HarnessCompleteness::Complete,
    })
}

fn external_observation_content(
    kind: &HarnessObservationKind,
) -> (ExternalTranscriptCategory, String) {
    match kind {
        HarnessObservationKind::SubmissionAccepted {
            submission_id,
            attempt_id,
            native_turn_id,
        } => (
            ExternalTranscriptCategory::Status,
            format!(
                "Native submission accepted: {submission_id} · attempt {attempt_id} · native turn {native_turn_id} (not settlement, assignment completion, or replay authority)"
            ),
        ),
        HarnessObservationKind::NativeTurnTerminal {
            native_turn_id,
            disposition,
        } => (
            ExternalTranscriptCategory::Status,
            format!(
                "Native turn {native_turn_id} ended: {disposition:?} (not background settlement)"
            ),
        ),
        HarnessObservationKind::SubmissionConsumed {
            submission_id,
            attempt_id,
            native_frame_id,
        } => (
            ExternalTranscriptCategory::Status,
            format!(
                "Native reply consumed submission {submission_id} · attempt {attempt_id} · frame {native_frame_id} (not turn identity or settlement)"
            ),
        ),
        HarnessObservationKind::SubmissionResultObserved {
            submission_id,
            attempt_id,
            native_frame_id,
            is_error,
            queued_user_sends,
        } => (
            ExternalTranscriptCategory::Status,
            format!(
                "Native result attributed to submission {submission_id} · attempt {attempt_id} · frame {native_frame_id} · error {is_error} · queued user sends {queued_user_sends:?} (not settlement)"
            ),
        ),
        HarnessObservationKind::BindingConfirmed { open_id, binding } => (
            ExternalTranscriptCategory::Status,
            format!(
                "Native identity confirmed: {} · open {} · instance {} · generation {} · version {} (not settlement or assignment completion)",
                binding.native_session_id,
                open_id,
                binding.instance_id,
                binding.generation,
                binding.native_version
            ),
        ),
        HarnessObservationKind::Text { text } => {
            (ExternalTranscriptCategory::Assistant, text.clone())
        }
        HarnessObservationKind::Result { text } => (
            ExternalTranscriptCategory::Status,
            format!("Native result (not assignment completion): {text}"),
        ),
        HarnessObservationKind::Tool {
            native_tool_id,
            name,
            status,
        } => (
            ExternalTranscriptCategory::NativeTool,
            format!("{name} · native tool {native_tool_id} · {status}"),
        ),
        HarnessObservationKind::Subagent {
            native_agent_id,
            status,
        } => (
            ExternalTranscriptCategory::NativeSubagent,
            format!("native agent {native_agent_id} · {status}"),
        ),
        HarnessObservationKind::Interaction { interaction } => (
            ExternalTranscriptCategory::Status,
            format!(
                "Native interaction {} · {} · requested scope: {}",
                interaction.request_id, interaction.prompt, interaction.scope
            ),
        ),
        HarnessObservationKind::Status { status } => external_status_content(status),
        HarnessObservationKind::Diagnostic { code, message } => (
            ExternalTranscriptCategory::Status,
            format!("{code}: {message}"),
        ),
        HarnessObservationKind::Settlement { native_turn_id } => (
            ExternalTranscriptCategory::Status,
            format!("Native turn {native_turn_id} settled (not assignment acceptance)"),
        ),
        HarnessObservationKind::Vendor { namespace, .. } => (
            ExternalTranscriptCategory::Status,
            format!("Native extension observation: {namespace} (detail omitted)"),
        ),
    }
}

fn external_status_content(
    status: &HarnessExecutionStatus,
) -> (ExternalTranscriptCategory, String) {
    let view = external_activity(status);
    (
        ExternalTranscriptCategory::Status,
        format!(
            "{} · {} · {} · {} · {}",
            view.connectivity, view.execution, view.attention, view.continuation, view.completeness
        ),
    )
}

fn history_bindings(
    instance: &HarnessInstance,
    binding: &HarnessSessionBinding,
    prior_bindings: &[(HarnessInstance, HarnessSessionBinding)],
    session_id: &str,
) -> Result<std::collections::BTreeMap<u64, ExternalProvenanceView>, String> {
    let mut bindings = std::collections::BTreeMap::new();
    let mut definitions = std::collections::BTreeMap::new();
    let current = (instance.clone(), binding.clone());
    for (instance, binding) in prior_bindings.iter().chain(std::iter::once(&current)) {
        let (source, _) = external_metadata(instance, binding).map_err(|e| e.to_string())?;
        if source.session_id != session_id {
            return Err("mixed external session binding".into());
        }
        if let Some(previous) = definitions.insert(binding.generation, (instance, binding))
            && previous != (instance, binding)
        {
            return Err("conflicting external launch provenance".into());
        }
        if let Some(previous) = bindings.insert(source.generation, source.clone())
            && previous != source
        {
            return Err("conflicting external binding generation".into());
        }
    }
    Ok(bindings)
}

fn external_history_view(
    provenance: ExternalProvenanceView,
    metadata: ExternalSessionMetadataView,
    next_after_sequence: Option<u64>,
    has_more: bool,
) -> ExternalSessionView {
    ExternalSessionView {
        provenance,
        metadata,
        activity: external_activity(&HarnessExecutionStatus {
            connectivity: HarnessConnectivity::Unknown,
            activity: HarnessActivity::Unknown,
            attention: HarnessAttention::Unknown,
            continuation: HarnessContinuation::Unknown,
            completeness: HarnessCompleteness::Unknown,
        }),
        transcript: Vec::new(),
        interactions: Vec::new(),
        actions: Vec::new(),
        next_after_sequence,
        has_more,
        truncated: false,
    }
}

/// Project a bounded canonical history page using exact retained bindings supplied by indexed
/// lookup.
///
/// No plugin inspection, native resume, or full-log discovery is performed.
/// Interactions from history are always read-only until separately hydrated by the application.
/// # Errors
/// Rejects malformed, out-of-order, mixed-session, unbound, or over-budget history.
pub fn external_history_page(
    instance: &HarnessInstance,
    binding: &HarnessSessionBinding,
    prior_bindings: &[(HarnessInstance, HarnessSessionBinding)],
    page: &bcode_session_models::external_backend::ExternalBackendPage,
) -> Result<ExternalSessionView, String> {
    use bcode_session_models::external_backend::{ExternalBackendEnvelope, ExternalBackendRecord};
    let (provenance, metadata) = external_metadata(instance, binding).map_err(|e| e.to_string())?;
    if page.events.len() > MAX_EXTERNAL_VIEW_ITEMS || prior_bindings.len() > MAX_EXTERNAL_VIEW_ITEMS
    {
        return Err("external history page exceeds presentation bounds".into());
    }
    let bindings = history_bindings(instance, binding, prior_bindings, &provenance.session_id)?;
    let mut view = external_history_view(
        provenance,
        metadata,
        page.next_after_sequence,
        page.has_more,
    );
    let mut previous = None;
    for event in &page.events {
        if event.session_id.to_string() != binding.session_id
            || previous.is_some_and(|sequence| sequence >= event.sequence)
        {
            return Err("mixed or unordered external history".into());
        }
        previous = Some(event.sequence);
        if event.schema_version != bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION {
            return Err("unsupported external history event schema".into());
        }
        let envelope = ExternalBackendEnvelope::from_event(event)?
            .ok_or_else(|| "non-external event in external history page".to_owned())?;
        if envelope.record.session_id() != binding.session_id {
            return Err("external record owner mismatch".into());
        }
        let source = bindings
            .get(&envelope.record.generation())
            .ok_or_else(|| "retained external binding lookup required".to_owned())?;
        let item = match &envelope.record {
            ExternalBackendRecord::Observation { observation } => {
                if observation.generation == binding.generation
                    && let HarnessObservationKind::Status { status } = &observation.kind
                {
                    view.activity = external_activity(status);
                }
                external_transcript_item(observation, event.sequence, source)
                    .map_err(|e| e.to_string())?
            }
            ExternalBackendRecord::Binding { instance, binding } => {
                let (retained, _) =
                    external_metadata(instance, binding).map_err(|e| e.to_string())?;
                if &retained != source {
                    return Err("external binding evidence mismatch".into());
                }
                ExternalTranscriptItemView {
                    observation_id: envelope.record.identity_key(),
                    sequence: event.sequence,
                    provenance: source.clone(),
                    category: ExternalTranscriptCategory::Status,
                    text: "Managed external session bound".into(),
                    incomplete: false,
                }
            }
            ExternalBackendRecord::Submission { submission } => ExternalTranscriptItemView {
                observation_id: envelope.record.identity_key(),
                sequence: event.sequence,
                provenance: source.clone(),
                category: ExternalTranscriptCategory::User,
                text: submission.text.clone(),
                incomplete: false,
            },
            ExternalBackendRecord::Receipt { receipt } => ExternalTranscriptItemView {
                observation_id: envelope.record.identity_key(),
                sequence: event.sequence,
                provenance: source.clone(),
                category: ExternalTranscriptCategory::Status,
                text: format!(
                    "Submission {} · delivery {:?} (not assignment completion; uncertain delivery must not replay)",
                    receipt.submission_id, receipt.state
                ),
                incomplete: false,
            },
            ExternalBackendRecord::Response { interaction, .. } => ExternalTranscriptItemView {
                observation_id: envelope.record.identity_key(),
                sequence: event.sequence,
                provenance: source.clone(),
                category: ExternalTranscriptCategory::Status,
                text: format!(
                    "Response recorded for native request {}",
                    interaction.request_id
                ),
                incomplete: false,
            },
        };
        view.transcript.push(item);
    }
    validate_external_view(&view).map_err(|e| e.to_string())?;
    Ok(view)
}

/// Whether a durable event contributes managed external presentation.
#[must_use]
pub const fn is_external_event(kind: &bcode_session_models::SessionEventKind) -> bool {
    use bcode_session_models::SessionEventKind as E;
    matches!(
        kind,
        E::ExternalHarnessOperation { .. }
            | E::ExternalBackendBound { .. }
            | E::ExternalSubmissionAccepted { .. }
            | E::ExternalDeliveryObserved { .. }
            | E::ExternalObservationRecorded { .. }
            | E::ExternalInteractionResponded { .. }
    )
}

/// Describe preparation without presenting requested selection as native confirmation.
/// # Errors
/// Rejects invalid prepared-session contracts.
pub fn prepared_session_text(
    prepared: &bcode_harness_models::HarnessPreparedSession,
) -> Result<String, String> {
    prepared.validate().map_err(|e| e.to_string())?;
    Ok(format!(
        "Native transport prepared · Bcode {} · instance {} · generation {} · open {} · version {}\nRequested native selection: {} (unconfirmed)\nNo native conversation identity confirmed. Use /native status for the exact offline target, then /native send <generation> <submission-id> <attempt-id> <text> for first input. Prepared is not resumed, created, settled, or assignment completion. Existing live transport only; process loss requires reconciliation, never automatic recreation/replay.",
        prepared.session_id,
        prepared.instance_id,
        prepared.generation,
        prepared.open_id,
        prepared.native_version,
        prepared
            .requested_native_session_id
            .as_deref()
            .unwrap_or("new conversation requested")
    ))
}

fn canonical_operation_text(
    operation: &bcode_session_models::external_harness::ExternalHarnessOperation,
    sequence: u64,
) -> Result<String, String> {
    use bcode_session_models::external_harness::{
        ExternalHarnessOperationRecord as R, ExternalHarnessResponseKind as S,
    };
    operation.validate()?;
    let text = match &operation.record {
        R::Configured { instance, .. } => format!("Managed external configured · plugin {} · instance {}\nNo native conversation confirmed by configuration.", instance.plugin_id, instance.instance_id),
        R::DeliveryClaimed { .. } => "Native delivery claimed; outcome pending or uncertain. No replay or settlement implied.".into(),
        R::DeliveryCompleted { response, .. } => match &response.kind {
            S::Opened(binding) => format!("Native identity confirmed: {} · generation {} (not settled or assignment completion)", binding.native_session_id, binding.generation),
            S::Prepared(prepared) | S::Reconciled(bcode_harness_models::HarnessReconciliation::KnownPrepared { prepared, .. }) => prepared_session_text(prepared)?,
            S::Receipt(receipt) => format!("Native delivery {:?} · submission {} · attempt {} (not assignment completion)", receipt.state, receipt.submission_id, receipt.attempt_id),
            S::Acknowledged { mutation_id } => format!("Native acknowledgment {mutation_id} (not settlement or proof of no effects)"),
            S::Failure(failure) => format!("Native failure {failure:?}; no automatic replay"),
            S::Reconciled(_) => "Native reconciliation recorded; not assignment acceptance or automatic replay authority".into(),
        },
        R::ObservationPage { instance, page, .. } => {
            let mut text = format!("Native observation page · plugin {} · instance {} · coverage {:?}", instance.plugin_id, instance.instance_id, page.completeness);
            for observation in &page.observations {
                let source = ExternalProvenanceView { session_id: observation.session_id.clone(), instance_id: instance.instance_id.clone(),
                    plugin_id: instance.plugin_id.clone(), native_session_id: "not included in observation".into(), generation: observation.generation };
                let _ = write!(text, "\n{}", transcript_fallback(&external_transcript_item(observation, sequence, &source).map_err(|e| e.to_string())?));
            }
            text
        }
    };
    let mut text = format!(
        "Managed external · Bcode {} · generation {}\n{text}",
        operation.owner.session_id, operation.owner.generation
    );
    text.retain(|ch| !ch.is_control() || ch == '\n' || ch == '\t');
    if text.len() > MAX_EXTERNAL_VIEW_BYTES {
        return Err("external presentation exceeds bound".into());
    }
    Ok(text)
}

/// Normalize one canonical external event into literal transcript content.
///
/// Used by the ordinary `SessionView` reducer, so all existing renderers and offline history see the same
/// semantics even when no plugin or native process can be loaded.
/// # Errors
/// Rejects malformed reserved envelopes without exposing raw protocol content.
pub fn canonical_external_text(
    event: &bcode_session_models::SessionEvent,
) -> Result<Option<String>, String> {
    use bcode_session_models::external_backend::{ExternalBackendEnvelope, ExternalBackendRecord};
    if let bcode_session_models::SessionEventKind::ExternalHarnessOperation { operation } =
        &event.kind
    {
        return canonical_operation_text(operation, event.sequence).map(Some);
    }
    let Some(envelope) = ExternalBackendEnvelope::from_event(event)? else {
        return Ok(None);
    };
    let record = &envelope.record;
    let prefix = format!(
        "Managed external · Bcode {} · generation {}",
        record.session_id(),
        record.generation()
    );
    let text = match record {
        ExternalBackendRecord::Binding { instance, binding } => {
            let (source, metadata) =
                external_metadata(instance, binding).map_err(|e| e.to_string())?;
            format!(
                "Native identity confirmed · {}\nExecutable: {} · native version: {} · config namespace: {}\nHistory remains readable when plugin is disabled; native resume is independent.\nUse /native history [cursor] for bounded offline observations.\nSend: /native send <generation> <submission-id> <attempt-id> <text>\nSteer: /native steer <generation> <submission-id> <attempt-id> <expected-native-turn> <text>",
                provenance_label(&source),
                metadata.executable,
                metadata.native_version,
                metadata.config_namespace
            )
        }
        ExternalBackendRecord::Observation { observation } => {
            // A selected window may omit the binding event. Do not invent plugin/native metadata.
            let source = ExternalProvenanceView {
                session_id: observation.session_id.clone(),
                instance_id: "binding outside selected window".into(),
                plugin_id: "external harness".into(),
                native_session_id: "not included in this observation".into(),
                generation: observation.generation,
            };
            let item = external_transcript_item(observation, event.sequence, &source)
                .map_err(|e| e.to_string())?;
            let mut text = format!(
                "{}{}\n{}",
                transcript_category_label(item.category),
                if item.incomplete {
                    " · partial observation"
                } else {
                    ""
                },
                item.text
            );
            if let HarnessObservationKind::Interaction { interaction } = &observation.kind {
                let _ = writeln!(
                    text,
                    "\nNative {:?} request: {} · generation {}",
                    interaction.kind, interaction.request_id, interaction.generation
                );
                for choice in &interaction.choices {
                    let _ = writeln!(text, "{}: {}", choice.choice_id, choice.label);
                }
                text.push_str("Use /native respond <generation> <request-id> <response-id> <approve|deny|answer|choose|assist> [text or choice-id]. Current capability and ownership are rechecked; historical requests may be unavailable.");
            }
            text
        }
        ExternalBackendRecord::Submission { submission } => format!(
            "Submitted native input {} · placement {:?}\n{}",
            submission.submission_id, submission.placement, submission.text
        ),
        ExternalBackendRecord::Receipt { receipt } => format!(
            "Native delivery {:?} · submission {} · attempt {}\nAcknowledgment/result/idle is not assignment completion; uncertain delivery must not replay.",
            receipt.state, receipt.submission_id, receipt.attempt_id
        ),
        ExternalBackendRecord::Response { interaction, .. } => format!(
            "Native response recorded · request {} (not execution settlement)",
            interaction.request_id
        ),
    };
    let mut text = format!("{prefix}\n{text}");
    text.retain(|ch| !ch.is_control() || ch == '\n' || ch == '\t');
    if text.len() > MAX_EXTERNAL_VIEW_BYTES {
        return Err("external presentation exceeds bound".into());
    }
    Ok(Some(text))
}

/// Explicit source label, retained even with a disabled plugin.
#[must_use]
pub fn provenance_label(provenance: &ExternalProvenanceView) -> String {
    format!(
        "{} / {} · native {} · generation {} · Bcode {}",
        provenance.plugin_id,
        provenance.instance_id,
        provenance.native_session_id,
        provenance.generation,
        provenance.session_id
    )
}

/// Shared fallback titles explicitly distinguish observed native tools and subagents.
#[must_use]
pub const fn transcript_category_label(category: ExternalTranscriptCategory) -> &'static str {
    match category {
        ExternalTranscriptCategory::User => "User",
        ExternalTranscriptCategory::Assistant => "Assistant",
        ExternalTranscriptCategory::Reasoning => "Reasoning",
        ExternalTranscriptCategory::NativeTool => "Observed native tool (not executed by Bcode)",
        ExternalTranscriptCategory::NativeSubagent => {
            "Observed native subagent (not a Bcode child)"
        }
        ExternalTranscriptCategory::Status => "External status",
    }
}

/// Literal text fallback, never executable markup.
#[must_use]
pub fn transcript_fallback(item: &ExternalTranscriptItemView) -> String {
    format!(
        "{} · {}{}\n{}",
        transcript_category_label(item.category),
        provenance_label(&item.provenance),
        if item.incomplete {
            " · partial observation"
        } else {
            ""
        },
        item.text
    )
}

/// Include capability constraints even when an action is admitted.
#[must_use]
pub fn action_label(action: &ExternalActionView) -> String {
    let label = match action.action {
        ExternalActionKind::Submit => "Send",
        ExternalActionKind::Steer => "Steer expected turn",
        ExternalActionKind::Respond => "Respond",
        ExternalActionKind::Approve => "Decide native approval",
        ExternalActionKind::Interrupt => "Request interrupt (not settlement)",
        ExternalActionKind::Reconcile => "Inspect uncertain delivery (no replay)",
    };
    if action.reason.is_empty() && action.enabled {
        return label.to_owned();
    }
    format!(
        "{label}{}: {}",
        if action.enabled { "" } else { " unavailable" },
        action.reason
    )
}

/// One generic read-only content section shared by terminal and browser adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalFallbackBlock {
    /// Stable canonical identity for viewport anchoring and keyed DOM updates.
    pub id: String,
    /// Literal text, never executable markup or terminal escape sequences.
    pub text: String,
}

/// Build a bounded generic fallback with metadata, independent activity/attention, interactions,
/// constraints, and source attribution. Renderers must escape markup and use native text layout.
/// # Errors
/// Rejects pages exceeding portable presentation budgets before creating additional copies.
pub fn external_fallback_blocks(
    view: &ExternalSessionView,
) -> Result<Vec<ExternalFallbackBlock>, HarnessContractError> {
    validate_external_view(view)?;
    let mut blocks = vec![ExternalFallbackBlock {
        id: "external:metadata".into(),
        text: format!(
            "Managed external session · {}\nExecutable: {} · native version: {} · config namespace: {}\nConnectivity: {}\nActivity: {}\nAttention: {}\nContinuation: {}\nCoverage: {}{}{}",
            provenance_label(&view.provenance),
            view.metadata.executable,
            view.metadata.native_version,
            view.metadata.config_namespace,
            view.activity.connectivity,
            view.activity.execution,
            view.activity.attention,
            view.activity.continuation,
            view.activity.completeness,
            if view.has_more {
                " · more canonical history available"
            } else {
                ""
            },
            if view.truncated {
                " · presentation truncated"
            } else {
                ""
            },
        ),
    }];
    for item in &view.transcript {
        blocks.push(ExternalFallbackBlock {
            id: format!(
                "external:observation:{}:{}:{}",
                item.provenance.generation,
                item.observation_id.len(),
                item.observation_id
            ),
            text: transcript_fallback(item),
        });
    }
    for item in &view.interactions {
        let mut text = format!(
            "{} · {}\n{}\nRequested native scope: {}\n{}",
            item.kind,
            provenance_label(&item.provenance),
            item.prompt,
            item.scope,
            action_label(&item.response)
        );
        for choice in &item.choices {
            let _ = write!(text, "\n{}: {}", choice.id, choice.label);
        }
        blocks.push(ExternalFallbackBlock {
            id: format!(
                "external:interaction:{}:{}:{}",
                item.provenance.generation,
                item.request_id.len(),
                item.request_id
            ),
            text,
        });
    }
    for action in &view.actions {
        blocks.push(ExternalFallbackBlock {
            id: format!("external:action:{:?}", action.action),
            text: action_label(action),
        });
    }
    // Controls are not meaningful semantic content and must never become terminal escapes.
    for block in &mut blocks {
        block
            .text
            .retain(|ch| !ch.is_control() || ch == '\n' || ch == '\t');
    }
    Ok(blocks)
}

#[cfg(test)]
#[path = "external_tests.rs"]
mod tests;
