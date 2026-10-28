//! Local synthesis checkpoints: bounded operational evidence, never model instructions.

use super::*;
use sha2::Sha256;

#[path = "context_compaction_recovery_read.rs"]
mod recovery_read;
pub(super) use recovery_read::{RECOVERY_MAX_EVENTS, recovery_events};

const CHECKPOINT_FORMAT: &str = "bcode.local-compaction.v1";
const MAX_PAYLOAD: usize = bcode_session_models::MAX_COMPACTION_CHECKPOINT_PAYLOAD_BYTES;
const CAPACITY_POLICY_FORMAT: &str = "bcode.compaction.capacity-policy.v1";

/// Exact validation bounds, not a replacement for semantic request estimation.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointPolicy {
    format: String,
    max_carry_chars: usize,
    max_wire_bytes: usize,
    max_section_chars: usize,
}

impl CheckpointPolicy {
    fn from_policy(policy: synthesis::SynthesisPolicy) -> Result<Self, CompactionError> {
        let value = Self {
            format: CAPACITY_POLICY_FORMAT.into(),
            max_carry_chars: policy.carry_chars,
            max_wire_bytes: policy.wire_bytes,
            max_section_chars: policy.section_chars,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), CompactionError> {
        if self.format != CAPACITY_POLICY_FORMAT
            || self.max_carry_chars == 0
            || self.max_wire_bytes == 0
            || self.max_section_chars == 0
            || self.max_section_chars > self.max_carry_chars
        {
            return Err(invalid_checkpoint());
        }
        Ok(())
    }
}

#[derive(Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
enum SynthesisPhase {
    #[default]
    Accumulating,
    ConsolidatingV1,
}

fn legacy_packing_policy() -> String {
    "legacy-chars-v1".into()
}

const fn invalid_checkpoint() -> CompactionError {
    CompactionError::RecoveryRequired(CompactionRecoveryReason::InvalidCheckpoint)
}

fn digest<T: serde::Serialize + ?Sized>(value: &T) -> Result<String, CompactionError> {
    let bytes = serde_json::to_vec(value).map_err(|_| invalid_checkpoint())?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn source_digest(events: &[bcode_session_models::SessionEvent]) -> Result<String, CompactionError> {
    let sources = events
        .iter()
        .map(bcode_session::compaction_event_identity)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid_checkpoint())?;
    if sources.is_empty() || sources.len() > 4096 {
        return Err(invalid_checkpoint());
    }
    digest(
        &sources
            .iter()
            .map(|source| (source.sequence, source.sha256.as_str()))
            .collect::<Vec<_>>(),
    )
}

/// The namespace intentionally excludes the currently selected prefix and active model turn.
/// Appending a follow-up must not hide the operation or reset its consumed allowances.
pub(super) fn operation_key(
    history: &[bcode_session_models::SessionEvent],
    selection: &SessionModelSelection,
    tool_output_chars: usize,
    policy: CompactionPlanningPolicy,
) -> Result<String, CompactionError> {
    let marker = history.iter().rev().find(|event| {
        matches!(
            event.kind,
            SessionEventKind::ContextCompacted { .. }
                | SessionEventKind::ProviderContextCompacted { .. }
        )
    });
    Ok(format!(
        "local-compaction-v1:{}",
        digest(&(
            marker,
            &selection.provider_plugin_id,
            &selection.model_id,
            &selection.provider_context.auth_profile,
            tool_output_chars,
            match policy {
                CompactionPlanningPolicy::Proactive { keep_recent_tokens } =>
                    format!("proactive:{keep_recent_tokens}"),
                CompactionPlanningPolicy::OverflowRecovery => "overflow".into(),
            },
            COMPACTION_SYSTEM_PROMPT,
            COMPACTION_SUMMARY_FORMAT,
            COMPACTION_MAX_SUMMARY_INPUT_CHARS,
            COMPACTION_FRAGMENT_BYTES,
        ))?
    ))
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RestartAllowance {
    max_requests: u32,
    deadline_ms: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ContractContinuation {
    version: u32,
    contract: String,
    chunk: usize,
    requests: u8,
    baseline_requests: u32,
    reserved: u32,
    pending: bool,
    deadline_ms: u64,
    terminal_sequence: u64,
    terminal_sha256: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UpgradeSeed {
    evidence: CompactionEvidence,
    users: BTreeSet<u64>,
    unfinished: BTreeSet<u64>,
    source_start: usize,
    source_complete: bool,
    max_requests: u32,
    #[serde(default)]
    pending: bool,
    spent_requests: u32,
    source_checkpoint: u64,
    source_sha256: String,
}

impl UpgradeSeed {
    pub(super) const fn source_start(&self) -> usize {
        self.source_start
    }
    pub(super) const fn complete(&self) -> bool {
        self.source_complete
    }
    pub(super) const fn evidence(&self) -> &CompactionEvidence {
        &self.evidence
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LocalCheckpoint {
    format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    upgrade: Option<UpgradeSeed>,
    #[serde(default = "legacy_packing_policy")]
    pub(super) packing_policy: String,
    #[serde(default)]
    synthesis_policy: Option<CheckpointPolicy>,
    #[serde(default)]
    resolved_policy: Option<serde_json::Value>,
    #[serde(default)]
    restart_allowance: Option<RestartAllowance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    contract_continuation: Option<ContractContinuation>,
    #[serde(default)]
    synthesis_phase: SynthesisPhase,
    #[serde(default)]
    consolidation_requests: Vec<u8>,
    pub(super) boundary: u64,
    source_digest: String,
    manifest: Vec<String>,
    pub(super) next_chunk: usize,
    requests: Vec<u8>,
    evidence: CompactionEvidence,
    unfinished: BTreeSet<u64>,
    user_sequences: BTreeSet<u64>,
    #[serde(skip)]
    pub(super) checkpoint_sequence: u64,
}

impl LocalCheckpoint {
    #[cfg(test)]
    pub(super) fn create(
        plan: &CompactionPlan,
        transcript: &CompactionTranscript,
        chunks: &[String],
    ) -> Result<Self, CompactionError> {
        let evidence = initial_evidence(transcript);
        let checkpoint = Self {
            checkpoint_sequence: 0,
            format: CHECKPOINT_FORMAT.into(),
            packing_policy: legacy_packing_policy(),
            synthesis_policy: None,
            upgrade: None,
            resolved_policy: None,
            restart_allowance: None,
            contract_continuation: None,
            synthesis_phase: SynthesisPhase::Accumulating,
            consolidation_requests: Vec::new(),
            boundary: plan.compacted_through_sequence,
            source_digest: source_digest(&plan.compactable_prefix)?,
            manifest: chunks.iter().map(digest).collect::<Result<_, _>>()?,
            next_chunk: 0,
            requests: vec![0; chunks.len()],
            evidence,
            unfinished: BTreeSet::new(),
            user_sequences: prior_scope_sequences(transcript.previous_summary.as_deref()),
        };
        checkpoint.encode()?;
        Ok(checkpoint)
    }

    /// Start current capacity-aware work. Existing payloads are never upgraded in place.
    pub(super) fn create_with_policy(
        plan: &CompactionPlan,
        transcript: &CompactionTranscript,
        chunks: &[String],
        policy: synthesis::SynthesisPolicy,
    ) -> Result<Self, CompactionError> {
        let checkpoint = Self {
            checkpoint_sequence: 0,
            format: CHECKPOINT_FORMAT.into(),
            packing_policy: legacy_packing_policy(),
            synthesis_policy: Some(CheckpointPolicy::from_policy(policy)?),
            upgrade: None,
            resolved_policy: None,
            restart_allowance: None,
            contract_continuation: None,
            synthesis_phase: SynthesisPhase::Accumulating,
            consolidation_requests: vec![0; chunks.len()],
            boundary: plan.compacted_through_sequence,
            source_digest: source_digest(&plan.compactable_prefix)?,
            manifest: chunks.iter().map(digest).collect::<Result<_, _>>()?,
            next_chunk: 0,
            requests: vec![0; chunks.len()],
            evidence: initial_evidence_with_policy(transcript, policy.carry_chars)?,
            unfinished: BTreeSet::new(),
            user_sequences: prior_scope_sequences(transcript.previous_summary.as_deref()),
        };
        Self::decode(&checkpoint.encode()?)?;
        Ok(checkpoint)
    }

    /// Call before restoration or dispatch; absence is explicit incompatibility, not a reset.
    pub(super) fn verify_policy(
        &self,
        policy: synthesis::SynthesisPolicy,
    ) -> Result<(), CompactionError> {
        if self.synthesis_policy.as_ref() != Some(&CheckpointPolicy::from_policy(policy)?) {
            return Err(CompactionError::RecoveryRequired(
                CompactionRecoveryReason::PolicyChanged,
            ));
        }
        if self.synthesis_phase != SynthesisPhase::Accumulating {
            return Err(CompactionError::RecoveryRequired(
                CompactionRecoveryReason::ConsolidationUnresolved,
            ));
        }
        Ok(())
    }

    /// Freeze the complete version-1 resolved budget on fresh, undispatched work only.
    /// Generic serialization avoids coupling checkpoint storage to the planner module's location.
    pub(super) fn bind_resolved_policy<T: serde::Serialize>(
        &mut self,
        policy: &T,
    ) -> Result<(), CompactionError> {
        let value = serde_json::to_value(policy).map_err(|_| invalid_checkpoint())?;
        if value.get("version").and_then(serde_json::Value::as_u64) != Some(1)
            || self.synthesis_policy.is_none()
            || self.resolved_policy.is_some()
            || self.next_chunk != 0
            || self.requests.iter().any(|count| *count != 0)
        {
            return Err(invalid_checkpoint());
        }
        self.resolved_policy = Some(value);
        self.encode()?;
        Ok(())
    }

    /// Bind the complete serialized token-capacity contract, not only synthesis byte guards.
    pub(super) fn bind_capacity_policy<T: serde::Serialize>(
        &mut self,
        policy: &T,
    ) -> Result<(), CompactionError> {
        self.bind_resolved_policy(policy)
    }

    /// Verify the complete frozen contract without changing consumed allowances or source pins.
    pub(super) fn verify_capacity_policy<T: serde::Serialize>(
        &self,
        policy: &T,
    ) -> Result<(), CompactionError> {
        self.verify_resolved_policy(policy)
    }

    /// Stable digest of the complete frozen serialized capacity contract.
    #[cfg(test)]
    pub(super) fn capacity_policy_digest(&self) -> Result<String, CompactionError> {
        digest(
            self.resolved_policy
                .as_ref()
                .ok_or_else(invalid_checkpoint)?,
        )
    }

    /// Automatic replacement may happen once for an original operation, never replenish
    /// a replacement operation's durable allowance on another configuration change.
    pub(super) const fn is_original_operation(&self) -> bool {
        self.upgrade.is_none()
            && self.restart_allowance.is_none()
            && self.contract_continuation.is_none()
    }

    /// Decode the frozen budget into the planner's current typed contract.
    /// Absence is incompatible state, never permission to resolve and replace a spent budget.
    pub(super) fn resolved_policy<T: serde::de::DeserializeOwned>(
        &self,
    ) -> Result<T, CompactionError> {
        let value = self
            .resolved_policy
            .as_ref()
            .ok_or(CompactionError::RecoveryRequired(
                CompactionRecoveryReason::PolicyChanged,
            ))?;
        if value.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
            return Err(invalid_checkpoint());
        }
        serde_json::from_value(value.clone()).map_err(|_| invalid_checkpoint())
    }

    /// Compare the entire freshly resolved typed budget before any restoration or reservation.
    pub(super) fn verify_resolved_policy<T: serde::Serialize>(
        &self,
        policy: &T,
    ) -> Result<(), CompactionError> {
        let value = serde_json::to_value(policy).map_err(|_| invalid_checkpoint())?;
        if self.resolved_policy.as_ref() != Some(&value) {
            return Err(CompactionError::RecoveryRequired(
                CompactionRecoveryReason::PolicyChanged,
            ));
        }
        Ok(())
    }

    /// Total reserved requests, including corrective work; no old counter is ever reset.
    fn consumed_requests(&self) -> u32 {
        self.upgrade.as_ref().map_or(0, |seed| seed.spent_requests)
            + self
                .requests
                .iter()
                .chain(&self.consolidation_requests)
                .map(|count| u32::from(*count))
                .sum::<u32>()
            + self
                .contract_continuation
                .as_ref()
                .map_or(0, |grant| u32::from(grant.requests))
    }

    fn verify_contract_continuation(&self) -> Result<(), CompactionError> {
        if self.contract_continuation.as_ref().is_some_and(|grant| {
            grant.version != 1
                || grant.contract != "section-schema-v1"
                || grant.chunk >= self.manifest.len()
                || grant.chunk > self.next_chunk
                || self.requests.get(grant.chunk) != Some(&2)
                || grant.requests > 2
                || grant.deadline_ms == 0
                || grant.terminal_sequence == 0
                || grant.reserved > 64
                || grant.baseline_requests.checked_add(grant.reserved)
                    != Some(self.consumed_requests())
                || grant.terminal_sha256.len() != 64
                || !grant
                    .terminal_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                || (grant.chunk < self.next_chunk && grant.requests == 0)
        }) {
            return Err(invalid_checkpoint());
        }
        Ok(())
    }

    fn verify_upgrade(&self) -> Result<(), CompactionError> {
        if self.upgrade.as_ref().is_some_and(|seed| {
            seed.source_checkpoint == 0
                || seed.source_sha256.len() != 64
                || seed.spent_requests > 2048
                || seed.max_requests != seed.spent_requests.saturating_add(64)
                || !self.packing_policy.starts_with("capacity-summary-v2")
                || self.restart_allowance.is_some()
                || self.contract_continuation.is_some()
                || self.consumed_requests() > seed.max_requests
        }) {
            return Err(invalid_checkpoint());
        }
        Ok(())
    }

    pub(super) fn decode(payload: &str) -> Result<Self, CompactionError> {
        if payload.len() > MAX_PAYLOAD {
            return Err(invalid_checkpoint());
        }
        let checkpoint: Self = serde_json::from_str(payload).map_err(|_| invalid_checkpoint())?;
        checkpoint.verify_upgrade()?;
        checkpoint.verify_contract_continuation()?;
        if checkpoint
            .restart_allowance
            .as_ref()
            .is_some_and(|allowance| {
                allowance.max_requests == 0
                    || allowance.max_requests > 4096
                    || allowance.deadline_ms == 0
                    || checkpoint.resolved_policy.is_none()
                    || checkpoint.contract_continuation.as_ref().map_or_else(
                        || checkpoint.consumed_requests(),
                        |grant| grant.baseline_requests,
                    ) > allowance.max_requests
            })
        {
            return Err(invalid_checkpoint());
        }
        if checkpoint.resolved_policy.as_ref().is_some_and(|value| {
            checkpoint.synthesis_policy.is_none()
                || value.get("version").and_then(serde_json::Value::as_u64) != Some(1)
        }) {
            return Err(invalid_checkpoint());
        }
        let max_carry_chars = if let Some(policy) = &checkpoint.synthesis_policy {
            policy.validate()?;
            if checkpoint.consolidation_requests.len() != checkpoint.manifest.len()
                || checkpoint
                    .consolidation_requests
                    .iter()
                    .any(|count| *count > 2)
                || checkpoint
                    .consolidation_requests
                    .iter()
                    .skip(checkpoint.next_chunk)
                    .any(|count| *count != 0)
                || (checkpoint.synthesis_phase == SynthesisPhase::ConsolidatingV1
                    && (checkpoint.next_chunk == 0
                        || checkpoint
                            .consolidation_requests
                            .get(checkpoint.next_chunk - 1)
                            .is_none_or(|count| *count == 0)))
            {
                return Err(invalid_checkpoint());
            }
            policy.max_carry_chars
        } else {
            if checkpoint.synthesis_phase != SynthesisPhase::Accumulating
                || !checkpoint.consolidation_requests.is_empty()
            {
                return Err(invalid_checkpoint());
            }
            COMPACTION_MAX_CARRIED_SUMMARY_CHARS
        };
        if checkpoint.format != CHECKPOINT_FORMAT
            || checkpoint.manifest.is_empty()
            || checkpoint.manifest.len() > if checkpoint.packing_policy.starts_with("capacity-summary-v2") { 4096 } else { COMPACTION_MAX_CHUNKS }
            || checkpoint.requests.len() != checkpoint.manifest.len()
            || checkpoint.next_chunk > checkpoint.manifest.len()
            || checkpoint.requests.iter().any(|count| *count > 2)
            || (checkpoint.requests[..checkpoint.next_chunk].contains(&0) && !checkpoint.upgrade.as_ref().is_some_and(|seed| seed.source_complete && checkpoint.next_chunk == checkpoint.manifest.len()))
            || checkpoint
                .requests
                .iter()
                .skip(checkpoint.next_chunk.saturating_add(1))
                .any(|count| *count != 0)
            // Automatic recovery consults decoded carry before full source verification.
            // A cursor alone is not acceptance evidence when its retained state disagrees.
            || (checkpoint.next_chunk > 0
                && (checkpoint.evidence.continuation.trim().is_empty()
                    || checkpoint.user_sequences
                        != checkpoint
                            .evidence
                            .current_scope
                            .iter()
                            .map(|scope| scope.source_sequence)
                            .chain(checkpoint.unfinished.iter().copied())
                            .collect()))
            || checkpoint.evidence.current_scope.iter().any(|scope| {
                scope.statement.trim().is_empty()
                    || !checkpoint.user_sequences.contains(&scope.source_sequence)
            })
            || checkpoint.evidence.format != COMPACTION_SUMMARY_FORMAT
            || encode_compaction_evidence(&checkpoint.evidence)?
                .chars()
                .count()
                > max_carry_chars
        {
            return Err(invalid_checkpoint());
        }
        Ok(checkpoint)
    }

    /// A cursor alone cannot establish acceptance while a second synthesis call is unresolved.
    pub(super) fn accepted_chunk(&self, chunk: usize) -> bool {
        self.synthesis_phase == SynthesisPhase::Accumulating && self.next_chunk > chunk
    }

    pub(super) fn verify_chunks(&self, chunks: &[String]) -> Result<(), CompactionError> {
        if self.manifest != chunks.iter().map(digest).collect::<Result<Vec<_>, _>>()? {
            return Err(invalid_checkpoint());
        }
        Ok(())
    }

    pub(super) fn verify(
        &self,
        plan: &CompactionPlan,
        transcript: &CompactionTranscript,
        chunks: &[String],
    ) -> Result<(), CompactionError> {
        if self.boundary != plan.compacted_through_sequence
            || self.source_digest != source_digest(&plan.compactable_prefix)?
            || self.manifest != chunks.iter().map(digest).collect::<Result<Vec<_>, _>>()?
        {
            return Err(invalid_checkpoint());
        }
        let mut unfinished = self
            .upgrade
            .as_ref()
            .map_or_else(BTreeSet::new, |seed| seed.unfinished.clone());
        let mut known = self.upgrade.as_ref().map_or_else(
            || prior_scope_sequences(transcript.previous_summary.as_deref()),
            |seed| seed.users.clone(),
        );
        for chunk in chunks.iter().take(self.next_chunk) {
            known.extend(chunk_user_sequences(chunk));
            update_unfinished_sources(&mut unfinished, chunk);
        }
        let expected_users: BTreeSet<_> = self
            .evidence
            .current_scope
            .iter()
            .map(|scope| scope.source_sequence)
            .chain(unfinished.iter().copied())
            .collect();
        if self.unfinished != unfinished
            || self.evidence.current_scope.iter().any(|scope| {
                scope.statement.trim().is_empty() || !known.contains(&scope.source_sequence)
            })
            || (self.next_chunk > 0 && self.user_sequences != expected_users)
            || (self.next_chunk == 0
                && (self.user_sequences != known
                    || digest(&self.evidence)?
                        != digest(&match (&self.upgrade, &self.synthesis_policy) {
                            (Some(seed), _) => seed.evidence.clone(),
                            (None, Some(policy)) => {
                                initial_evidence_with_policy(transcript, policy.max_carry_chars)?
                            }
                            (None, None) => initial_evidence(transcript),
                        })?))
        {
            return Err(invalid_checkpoint());
        }
        Ok(())
    }

    fn encode(&self) -> Result<String, CompactionError> {
        let payload = serde_json::to_string(self).map_err(|_| invalid_checkpoint())?;
        if payload.len() > MAX_PAYLOAD {
            return Err(invalid_checkpoint());
        }
        Ok(payload)
    }

    /// Emit only after `verify` succeeds or validated acceptance has been durably saved.
    pub(super) async fn observe(
        &self,
        state: &ServerState,
        session_id: SessionId,
        resume: bcode_session_models::CompactionInspectionResume,
        phase: bcode_session_models::CompactionInspectionPhase,
    ) {
        use bcode_session_models::{
            CompactionInspectionBudgets, CompactionInspectionCount, CompactionProgressObservation,
        };
        let Ok(total) = u64::try_from(self.manifest.len()) else {
            return;
        };
        let observation = CompactionProgressObservation {
            version: if matches!(
                phase,
                bcode_session_models::CompactionInspectionPhase::Validating
                    | bcode_session_models::CompactionInspectionPhase::Publishing
            ) {
                2
            } else {
                1
            },
            phase,
            chunks: Some(CompactionInspectionCount {
                done: u64::try_from(self.next_chunk).unwrap_or(u64::MAX),
                total,
            }),
            resume,
            budgets: Some(CompactionInspectionBudgets {
                provider_requests: Some(CompactionInspectionCount {
                    done: u64::from(self.consumed_requests()),
                    total: self.upgrade.as_ref().map_or_else(
                        || {
                            self.contract_continuation.as_ref().map_or_else(
                                || {
                                    self.restart_allowance.as_ref().map_or_else(
                                        || {
                                            total.saturating_mul(
                                                if self.synthesis_policy.is_some() {
                                                    4
                                                } else {
                                                    2
                                                },
                                            )
                                        },
                                        |allowance| u64::from(allowance.max_requests),
                                    )
                                },
                                |grant| u64::from(grant.baseline_requests).saturating_add(64),
                            )
                        },
                        |seed| u64::from(seed.max_requests),
                    ),
                }),
                source_events: None,
                decoded_bytes: None,
            }),
            blocker: None,
        };
        let Ok(detail) = observation.encode() else {
            return;
        };
        let turn_id = compaction_trace_owner(state, session_id).await;
        match state
            .sessions
            .append_trace_event(
                session_id,
                SessionTraceEvent {
                    timestamp_ms: current_unix_millis(),
                    turn_id,
                    phase: SessionTracePhase::ModelProviderEvent,
                    payload: SessionTracePayload::ProviderEvent {
                        event_type: bcode_session_models::COMPACTION_PROGRESS_OBSERVATION_EVENT
                            .into(),
                        detail: Some(detail),
                    },
                },
            )
            .await
        {
            Ok(event) => publish_session_event(state, &event).await,
            Err(error) => {
                tracing::warn!(%session_id, %error, "compaction progress observation unavailable");
            }
        }
    }

    pub(super) async fn save(
        &mut self,
        state: &ServerState,
        session_id: SessionId,
        key: &str,
    ) -> Result<(), CompactionError> {
        self.checkpoint_sequence = state
            .sessions
            .append_compaction_checkpoint(session_id, key, Some(self.encode()?))
            .await?
            .sequence;
        Ok(())
    }

    /// Every dispatched attempt, including an ambiguous interrupted request, consumes a slot.
    /// The reservation is saved before provider dispatch; cancellation never refunds it.
    /// Admit a single contract-correction grant only from exact, settled canonical rejection.
    /// This does not supersede source state, reset counters, or replay an uncertain request.
    pub(super) async fn admit_contract_continuation(
        &mut self,
        state: &ServerState,
        session_id: SessionId,
        key: &str,
    ) -> Result<bool, CompactionError> {
        use bcode_session_models::{SessionHistoryDirection, SessionHistoryQuery};
        if self.contract_continuation.is_some()
            || self.requests.get(self.next_chunk) != Some(&2)
            || self.synthesis_phase != SynthesisPhase::Accumulating
        {
            return Ok(false);
        }
        let request_exhausted = self
            .restart_allowance
            .as_ref()
            .is_some_and(|allowance| self.consumed_requests() >= allowance.max_requests);
        if request_exhausted {
            return Ok(false);
        }
        let page = state
            .sessions
            .session_history_page(
                session_id,
                SessionHistoryQuery {
                    cursor: None,
                    limit: 512,
                    direction: SessionHistoryDirection::Backward,
                },
            )
            .await?;
        if !page.compatibility_issues.is_empty() {
            return Err(invalid_checkpoint());
        }
        let rejection = format!(
            "compaction synthesis rejected after bounded repair; chunk={}/{} reason=",
            self.next_chunk + 1,
            self.manifest.len()
        );
        let Some(terminal) = page.events.iter().rev().find(|event| {
            event.sequence > self.checkpoint_sequence
                && matches!(&event.kind, SessionEventKind::TraceEvent { trace }
                    if trace.phase == SessionTracePhase::ContextCompactionFinished
                    && matches!(&trace.payload, SessionTracePayload::ContextCompaction {
                        compacted: false, message: Some(message), ..
                    } if message.contains(&rejection) && message.ends_with("; boundary unchanged")
                        && ["reason=invalid_json", "reason=invalid_response_shape", "reason=incomplete_output", "reason=wire_budget"].iter().any(|reason| message.contains(reason))))
        }) else { return Ok(false); };
        let identity =
            bcode_session::compaction_event_identity(terminal).map_err(|_| invalid_checkpoint())?;
        let events = recovery_events(
            state,
            session_id,
            self.checkpoint_sequence.saturating_sub(511).max(1),
            terminal.sequence,
        )
        .await?;
        let request = bcode_session_models::SessionCompactionRestartRequest {
            version: 1,
            session_id,
            operation_id: uuid::Uuid::new_v4(),
            expected_operation_key: key.into(),
            expected_checkpoint_sequence: self.checkpoint_sequence,
            expected_terminal_sequence: terminal.sequence,
            expected_terminal_sha256: identity.sha256.clone(),
            expected_tail_marker: None,
            expected_prior_marker: None,
            budget: bcode_session_models::SessionCompactionRestartBudget {
                max_provider_requests: 2,
                timeout_ms: 300_000,
            },
        };
        // Select the owning failed attempt, not an unrelated later terminal note. This verifies
        // a unique terminal usage identity for every reservation and unchanged source marker.
        select_restart_failure_proof(&request, &events)?;
        let deadline = current_unix_millis().saturating_add(300_000);
        self.contract_continuation = Some(ContractContinuation {
            version: 1,
            contract: "section-schema-v1".into(),
            chunk: self.next_chunk,
            requests: 0,
            baseline_requests: self.consumed_requests(),
            reserved: 0,
            pending: false,
            deadline_ms: deadline,
            terminal_sequence: terminal.sequence,
            terminal_sha256: identity.sha256,
        });
        self.save(state, session_id, key).await?;
        Ok(true)
    }

    /// Clear only after a complete returned response, never a transport/cancellation outcome.
    pub(super) async fn settle_contract_response(
        &mut self,
        state: &ServerState,
        session_id: SessionId,
        key: &str,
    ) -> Result<(), CompactionError> {
        let changed = self.upgrade.is_some() || self.contract_continuation.is_some();
        if let Some(seed) = &mut self.upgrade {
            seed.pending = false;
        }
        if let Some(grant) = &mut self.contract_continuation {
            grant.pending = false;
        }
        if changed {
            self.save(state, session_id, key).await?;
        }
        Ok(())
    }

    pub(super) async fn reserve(
        &mut self,
        state: &ServerState,
        session_id: SessionId,
        key: &str,
        index: usize,
    ) -> Result<u8, CompactionError> {
        let attempt = self.reserve_slot(index)?;
        self.save(state, session_id, key).await?;
        self.observe(
            state,
            session_id,
            bcode_session_models::CompactionInspectionResume::NotReported,
            bcode_session_models::CompactionInspectionPhase::Synthesizing,
        )
        .await;
        Ok(attempt)
    }

    /// Recheck immediately before final publication; reaching the call cap after a completed
    /// response is allowed, but an elapsed explicit restart deadline is not.
    pub(super) fn verify_publication_deadline(&self) -> Result<(), CompactionError> {
        if let Some(seed) = &self.upgrade {
            if seed.pending
                || self.next_chunk != self.manifest.len()
                || self.synthesis_phase != SynthesisPhase::Accumulating
            {
                return Err(invalid_checkpoint());
            }
            return Ok(());
        }
        if let Some(grant) = &self.contract_continuation {
            // An automatic dispatch deadline limits additional provider spending, not the
            // validity of already accepted, settled evidence. Explicit restart contracts below
            // retain their original publication semantics.
            if grant.pending
                || self.next_chunk != self.manifest.len()
                || self.synthesis_phase != SynthesisPhase::Accumulating
            {
                return Err(invalid_checkpoint());
            }
            // Dispatch already selects the corrective grant instead of the superseded restart
            // allowance. Finalization must use the same authority: once settled and complete,
            // neither dispatch deadline invalidates accepted work. Explicit restart operations
            // without a corrective grant retain the publication deadline below.
            return Ok(());
        }
        if self
            .restart_allowance
            .as_ref()
            .is_some_and(|allowance| current_unix_millis() >= allowance.deadline_ms)
        {
            return Err(CompactionError::Provider(
                "explicit compaction restart deadline exhausted; boundary unchanged".into(),
            ));
        }
        Ok(())
    }

    fn verify_restart_allowance(&self) -> Result<(), CompactionError> {
        if let Some(seed) = &self.upgrade {
            if seed.pending {
                return Err(CompactionError::Provider(
                    "compaction request settlement is unresolved; boundary unchanged".into(),
                ));
            }
            if self.consumed_requests() >= seed.max_requests {
                return Err(CompactionError::Provider(
                    "compaction provider request allowance exhausted; boundary unchanged".into(),
                ));
            }
            return Ok(());
        }
        if let Some(grant) = &self.contract_continuation {
            return if grant.pending
                || grant.reserved >= 64
                || current_unix_millis() >= grant.deadline_ms
            {
                Err(CompactionError::Provider(
                    "compaction corrective allowance exhausted; boundary unchanged".into(),
                ))
            } else {
                Ok(())
            };
        }
        let consumed = self.consumed_requests();
        if self
            .restart_allowance
            .as_ref()
            .is_some_and(|allowance| consumed >= allowance.max_requests)
            || self.contract_continuation.as_ref().is_some_and(|grant| {
                grant.chunk == self.next_chunk && current_unix_millis() >= grant.deadline_ms
            })
        {
            return Err(CompactionError::Provider(
                "compaction corrective allowance exhausted; boundary unchanged".into(),
            ));
        }
        if let Some(allowance) = &self.restart_allowance {
            let consumed: u32 = self
                .requests
                .iter()
                .chain(&self.consolidation_requests)
                .map(|count| u32::from(*count))
                .sum();
            if consumed >= allowance.max_requests || current_unix_millis() >= allowance.deadline_ms
            {
                return Err(CompactionError::Provider(
                    "explicit compaction restart allowance exhausted; boundary unchanged".into(),
                ));
            }
        }
        Ok(())
    }

    fn reserve_slot(&mut self, index: usize) -> Result<u8, CompactionError> {
        self.verify_restart_allowance()?;
        if index != self.next_chunk || self.synthesis_phase != SynthesisPhase::Accumulating {
            return Err(invalid_checkpoint());
        }
        let count = self
            .requests
            .get_mut(index)
            .ok_or_else(invalid_checkpoint)?;
        if *count >= 2 {
            let grant = self
                .contract_continuation
                .as_mut()
                .filter(|grant| {
                    grant.chunk == index
                        && grant.requests < 2
                        && current_unix_millis() < grant.deadline_ms
                })
                .ok_or_else(|| {
                    CompactionError::Provider(
                        "compaction request/repair budget exhausted; boundary unchanged".into(),
                    )
                })?;
            grant.requests += 1;
            grant.reserved += 1;
            grant.pending = true;
            return Ok(grant.requests - 1);
        }
        let attempt = *count;
        if let Some(seed) = &mut self.upgrade {
            seed.pending = true;
        }
        *count += 1;
        if let Some(grant) = &mut self.contract_continuation {
            grant.reserved += 1;
            grant.pending = true;
        }
        Ok(attempt)
    }

    fn reserve_consolidation_slot(&mut self) -> Result<u8, CompactionError> {
        self.verify_restart_allowance()?;
        if self.synthesis_policy.is_none()
            || self.next_chunk == 0
            || self.synthesis_phase != SynthesisPhase::Accumulating
        {
            return Err(invalid_checkpoint());
        }
        let count = self
            .consolidation_requests
            .get_mut(self.next_chunk - 1)
            .ok_or_else(invalid_checkpoint)?;
        if *count >= 2 {
            return Err(CompactionError::Provider(
                "compaction consolidation request/repair budget exhausted; boundary unchanged"
                    .into(),
            ));
        }
        let attempt = *count;
        *count += 1;
        self.synthesis_phase = SynthesisPhase::ConsolidatingV1;
        if let Some(seed) = &mut self.upgrade {
            seed.pending = true;
        }
        if let Some(grant) = &mut self.contract_continuation {
            grant.reserved += 1;
            grant.pending = true;
        }
        Ok(attempt)
    }

    /// Reserve before dispatch. Only an explicitly rejected response permits in-process repair;
    /// restoration of an unresolved dispatch fails closed through `verify_policy`.
    pub(super) async fn reserve_consolidation(
        &mut self,
        state: &ServerState,
        session_id: SessionId,
        key: &str,
    ) -> Result<u8, CompactionError> {
        let attempt = self.reserve_consolidation_slot()?;
        self.save(state, session_id, key).await?;
        compaction_recovery::transition(
            state,
            session_id,
            compaction_recovery::Phase::ConsolidatingV1,
        )
        .await?;
        Ok(attempt)
    }

    /// Record a complete, explicitly rejected response in the current process before repair.
    /// Do not call for cancellation, interruption, transport failure or an unknown outcome.
    pub(super) fn reject_consolidation_response(&mut self) -> Result<(), CompactionError> {
        if self.synthesis_phase != SynthesisPhase::ConsolidatingV1 {
            return Err(invalid_checkpoint());
        }
        self.synthesis_phase = SynthesisPhase::Accumulating;
        Ok(())
    }

    /// Accept only a caller-validated consolidation. Independent scope and provenance cannot change.
    pub(super) fn accept_consolidation(
        &mut self,
        evidence: &CompactionEvidence,
    ) -> Result<(), CompactionError> {
        let Some(policy) = &self.synthesis_policy else {
            return Err(invalid_checkpoint());
        };
        if self.synthesis_phase != SynthesisPhase::ConsolidatingV1
            || evidence.format != self.evidence.format
            || digest(&evidence.current_scope)? != digest(&self.evidence.current_scope)?
            || evidence.continuation.trim().is_empty()
            || encode_compaction_evidence(evidence)?.chars().count() > policy.max_carry_chars
        {
            return Err(invalid_checkpoint());
        }
        self.evidence = evidence.clone();
        self.synthesis_phase = SynthesisPhase::Accumulating;
        Ok(())
    }

    pub(super) fn needs_upgrade(&self) -> bool {
        bcode_session_migration::is_character_compaction(&self.packing_policy)
    }

    pub(super) const fn upgrade_seed(&self) -> Option<&UpgradeSeed> {
        self.upgrade.as_ref()
    }

    /// Historical decoding is migration-owned. Caller must prove settlement and atomically
    /// supersede this exact checkpoint before admitting any request using the returned seed.
    pub(super) fn migration_seed(
        &self,
        transcript: &CompactionTranscript,
    ) -> Result<UpgradeSeed, CompactionError> {
        let converted = bcode_session_migration::convert_character_compaction(
            &self.encode()?,
            self.checkpoint_sequence,
            transcript.previous_summary.as_deref(),
            &transcript.summary_input,
        )
        .map_err(|_| invalid_checkpoint())?;
        if converted.reserved_requests != self.consumed_requests() {
            return Err(invalid_checkpoint());
        }
        Ok(UpgradeSeed {
            evidence: self.evidence.clone(),
            users: self.user_sequences.clone(),
            unfinished: self.unfinished.clone(),
            source_start: converted.accepted_bytes,
            source_complete: self.next_chunk == self.manifest.len(),
            max_requests: self
                .consumed_requests()
                .checked_add(64)
                .ok_or_else(invalid_checkpoint)?,
            pending: false,
            spent_requests: self.consumed_requests(),
            source_checkpoint: converted.checkpoint_sequence,
            source_sha256: converted.checkpoint_sha256,
        })
    }

    pub(super) fn apply_migration_seed(&mut self, seed: UpgradeSeed) {
        self.evidence = seed.evidence.clone();
        self.user_sequences.clone_from(&seed.users);
        self.unfinished.clone_from(&seed.unfinished);
        if seed.source_complete {
            self.next_chunk = self.manifest.len();
        }
        self.upgrade = Some(seed);
    }

    pub(super) fn restore(&self) -> (CompactionEvidence, BTreeSet<u64>, BTreeSet<u64>) {
        (
            self.evidence.clone(),
            self.user_sequences.clone(),
            self.unfinished.clone(),
        )
    }

    pub(super) fn accept(
        &mut self,
        evidence: &CompactionEvidence,
        users: &BTreeSet<u64>,
        unfinished: &BTreeSet<u64>,
    ) {
        self.evidence = evidence.clone();
        self.user_sequences.clone_from(users);
        self.unfinished.clone_from(unfinished);
        self.next_chunk += 1;
    }
}

pub(super) fn initial_evidence_with_policy(
    transcript: &CompactionTranscript,
    max_carry_chars: usize,
) -> Result<CompactionEvidence, CompactionError> {
    let Some(summary) = transcript.previous_summary.as_deref() else {
        return Ok(initial_evidence(transcript));
    };
    // Current-format evidence only. Historical conversion belongs to migration, not restoration.
    if summary.len() > MAX_PAYLOAD {
        return Err(invalid_checkpoint());
    }
    // An opaque portable recollection is source data, not a checkpoint representation.
    // It is consumed whole by compaction_evidence_input; it grants no structured scope.
    let Ok(evidence) = serde_json::from_str::<CompactionEvidence>(summary) else {
        return Ok(CompactionEvidence {
            format: COMPACTION_SUMMARY_FORMAT.into(),
            current_scope: Vec::new(),
            continuation: String::new(),
        });
    };
    if evidence.format != COMPACTION_SUMMARY_FORMAT
        || encode_compaction_evidence(&evidence)?.chars().count() > max_carry_chars
    {
        return Err(invalid_checkpoint());
    }
    Ok(evidence)
}

pub(super) fn initial_evidence(transcript: &CompactionTranscript) -> CompactionEvidence {
    transcript
        .previous_summary
        .as_deref()
        .filter(|summary| summary.len() <= COMPACTION_MAX_CARRIED_SUMMARY_CHARS * 4)
        .and_then(|summary| serde_json::from_str::<CompactionEvidence>(summary).ok())
        .filter(|evidence| evidence.format == COMPACTION_SUMMARY_FORMAT)
        .unwrap_or_else(|| CompactionEvidence {
            format: COMPACTION_SUMMARY_FORMAT.into(),
            current_scope: Vec::new(),
            continuation: String::new(),
        })
}

/// Read exact-key recovery evidence without provider calls, publication, or ownership changes.
/// The result is advisory: serialized admission and atomic supersession recheck authority/state.
#[allow(clippy::too_many_lines)] // Keep the bounded read and all early, payload-free blockers together.
pub(super) async fn prepare_recovery(
    state: &ServerState,
    request: &bcode_session_models::compaction_restart::SessionCompactionRecoveryPrepareRequest,
) -> Result<
    bcode_session_models::compaction_restart::SessionCompactionRecoveryPreparation,
    CompactionError,
> {
    use bcode_session_models::compaction_restart::{
        SessionCompactionRecoveryBlocker as Blocker, SessionCompactionRecoveryPreparation,
        SessionCompactionRestartMarker, SessionCompactionRestartRequest,
    };
    request.validate().map_err(|_| invalid_checkpoint())?;
    let mut result = SessionCompactionRecoveryPreparation {
        version: 1,
        session_id: request.session_id,
        checkpoint: None,
        terminal: None,
        blocker: Some(Blocker::EvidenceUnavailable),
        restart: None,
    };
    let Some(event) = state
        .sessions
        .session_compaction_checkpoint(request.session_id, &request.expected_operation_key)
        .await?
    else {
        return Ok(result);
    };
    let identity =
        bcode_session::compaction_event_identity(&event).map_err(|_| invalid_checkpoint())?;
    result.checkpoint = Some(SessionCompactionRestartMarker {
        sequence: identity.sequence,
        sha256: identity.sha256,
    });
    if event.sequence != request.expected_checkpoint_sequence {
        result.blocker = Some(Blocker::CheckpointChanged);
        return Ok(result);
    }
    let envelope = match bcode_session_models::CompactionSynthesisCheckpoint::from_event(&event) {
        Ok(Some(value)) if value.operation_key == request.expected_operation_key => value,
        _ => {
            result.blocker = Some(Blocker::UnsupportedCheckpoint);
            return Ok(result);
        }
    };
    let Some(payload) = envelope.host_payload.as_deref() else {
        result.blocker = Some(Blocker::UnsupportedCheckpoint);
        return Ok(result);
    };
    if LocalCheckpoint::decode(payload).is_err() {
        result.blocker = Some(Blocker::UnsupportedCheckpoint);
        return Ok(result);
    }
    if state
        .sessions
        .session_compaction_checkpoint(
            request.session_id,
            &restart_pointer_key(&request.expected_operation_key),
        )
        .await?
        .is_some()
    {
        result.blocker = Some(Blocker::AlreadySuperseded);
        return Ok(result);
    }
    let page = state
        .sessions
        .session_history_page(
            request.session_id,
            bcode_session_models::SessionHistoryQuery {
                cursor: None,
                limit: 1,
                direction: bcode_session_models::SessionHistoryDirection::Backward,
            },
        )
        .await?;
    if !page.compatibility_issues.is_empty() {
        return Ok(result);
    }
    let Some(tail_sequence) = page.events.last().map(|event| event.sequence) else {
        return Ok(result);
    };
    let events = recovery_events(
        state,
        request.session_id,
        request
            .expected_checkpoint_sequence
            .saturating_sub(511)
            .max(1),
        tail_sequence,
    )
    .await?;
    if events.last().map(|event| event.sequence) != Some(tail_sequence) {
        return Ok(result);
    }
    let Some(last) = events
        .iter()
        .find(|event| event.sequence == request.expected_terminal_sequence)
    else {
        return Ok(result);
    };
    let terminal =
        bcode_session::compaction_event_identity(last).map_err(|_| invalid_checkpoint())?;
    result.terminal = Some(SessionCompactionRestartMarker {
        sequence: terminal.sequence,
        sha256: terminal.sha256.clone(),
    });
    let Some(start) = events.iter().position(|event| {
        event.sequence < request.expected_checkpoint_sequence
            && matches!(&event.kind,
            SessionEventKind::TraceEvent { trace }
                if trace.phase == SessionTracePhase::ContextCompactionStarted)
    }) else {
        return Ok(result);
    };
    let history = state
        .sessions
        .activated_conversation_snapshot(request.session_id)
        .await?
        .events;
    let marker = history
        .iter()
        .rev()
        .find(|event| {
            matches!(
                event.kind,
                SessionEventKind::ContextCompacted { .. }
                    | SessionEventKind::ProviderContextCompacted { .. }
            )
        })
        .map(bcode_session::compaction_event_identity)
        .transpose()
        .map_err(|_| invalid_checkpoint())?;
    let candidate = SessionCompactionRestartRequest {
        version: request.version,
        session_id: request.session_id,
        operation_id: request.operation_id,
        expected_operation_key: request.expected_operation_key.clone(),
        expected_checkpoint_sequence: request.expected_checkpoint_sequence,
        expected_terminal_sequence: request.expected_terminal_sequence,
        expected_terminal_sha256: terminal.sha256,
        expected_tail_marker: events
            .last()
            .map(bcode_session::compaction_event_identity)
            .transpose()
            .map_err(|_| invalid_checkpoint())?
            .map(|identity| SessionCompactionRestartMarker {
                sequence: identity.sequence,
                sha256: identity.sha256,
            }),
        expected_prior_marker: marker.map(|identity| SessionCompactionRestartMarker {
            sequence: identity.sequence,
            sha256: identity.sha256,
        }),
        budget: request.budget,
    };
    if candidate.validate().is_err() {
        result.blocker = Some(Blocker::CheckpointChanged);
        return Ok(result);
    }
    let evidence = &events[start..];
    if select_restart_failure_proof(&candidate, evidence).is_err() {
        result.blocker = Some(restart_failure_blocker(&candidate, evidence));
        return Ok(result);
    }
    result.blocker = None;
    result.restart = Some(candidate);
    Ok(result)
}

/// Canonical identities to recheck in the same transaction that publishes supersession.
pub(super) struct RestartFailureProof {
    events: Vec<bcode_session::CompactionEventIdentity>,
    operation_key: String,
    abandoned_responses: Vec<String>,
}

/// Explicit maintenance evidence read, paginated with aggregate event/byte bounds.
/// `start_sequence` selects the operation's Started trace; `finished_sequence` selects its
/// complete rejection. This never upgrades or rewrites the retained checkpoint.
pub(super) async fn read_restart_failure_proof(
    state: &ServerState,
    request: &bcode_session_models::SessionCompactionRestartRequest,
    start_sequence: u64,
    finished_sequence: u64,
) -> Result<RestartFailureProof, CompactionError> {
    request.validate().map_err(|_| invalid_checkpoint())?;
    let length = finished_sequence
        .checked_sub(start_sequence)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(invalid_checkpoint)?;
    if length > RECOVERY_MAX_EVENTS as u64
        || start_sequence >= request.expected_checkpoint_sequence
        || finished_sequence != request.expected_terminal_sequence
        || finished_sequence <= request.expected_checkpoint_sequence
    {
        return Err(invalid_checkpoint());
    }
    let tail_sequence = request
        .expected_tail_marker
        .as_ref()
        .map_or(finished_sequence, |tail| tail.sequence);
    if tail_sequence.saturating_sub(start_sequence) >= RECOVERY_MAX_EVENTS as u64 {
        return Err(invalid_checkpoint());
    }
    let events = recovery_events(state, request.session_id, start_sequence, tail_sequence).await?;
    select_restart_failure_proof(request, &events)
}

/// Select a complete proof within an already bounded canonical window, never scan backwards
/// beyond it. Earlier unrelated attempts must not make an otherwise exact selection inaccessible.
fn validate_restart_tail(
    request: &bcode_session_models::SessionCompactionRestartRequest,
    events: &[bcode_session_models::SessionEvent],
) -> Result<(), CompactionError> {
    let rejection = events.first().ok_or_else(invalid_checkpoint)?;
    let SessionEventKind::TraceEvent { trace } = &rejection.kind else {
        return Err(invalid_checkpoint());
    };
    let owner = trace.turn_id.as_deref();
    let last = events.last().ok_or_else(invalid_checkpoint)?;
    let identity =
        bcode_session::compaction_event_identity(last).map_err(|_| invalid_checkpoint())?;
    let (sequence, sha256) = request.expected_tail_marker.as_ref().map_or(
        (
            request.expected_terminal_sequence,
            request.expected_terminal_sha256.as_str(),
        ),
        |tail| (tail.sequence, tail.sha256.as_str()),
    );
    if identity.sequence != sequence
        || identity.sha256 != sha256
        || events.len() > 512
        || events
            .iter()
            .any(|event| event.session_id != request.session_id)
        || events
            .windows(2)
            .any(|pair| pair[0].sequence.checked_add(1) != Some(pair[1].sequence))
    {
        return Err(invalid_checkpoint());
    }
    let mut terminal_seen = false;
    let mut runtime_terminal_seen = false;
    let SessionTracePayload::ContextCompaction {
        message: Some(message),
        ..
    } = &trace.payload
    else {
        return Err(invalid_checkpoint());
    };
    let detail = message
        .strip_prefix("context compaction failed:")
        .ok_or_else(invalid_checkpoint)?
        .trim();
    for event in events.iter().skip(1) {
        match &event.kind {
            SessionEventKind::ModelTurnFinished {
                turn_id, outcome, ..
            } if !terminal_seen
                && owner == Some(turn_id.as_str())
                && matches!(
                    outcome,
                    bcode_session_models::ModelTurnOutcome::Cancelled
                        | bcode_session_models::ModelTurnOutcome::Error
                        | bcode_session_models::ModelTurnOutcome::ProviderUnavailable
                        | bcode_session_models::ModelTurnOutcome::IdleTimeout
                ) =>
            {
                terminal_seen = true;
            }
            SessionEventKind::RuntimeWorkFinished {
                work_id, status, ..
            } if terminal_seen
                && !runtime_terminal_seen
                && owner.is_some_and(|owner| work_id.0 == format!("model_{owner}"))
                && matches!(
                    status,
                    bcode_session_models::RuntimeWorkStatus::Cancelled
                        | bcode_session_models::RuntimeWorkStatus::Failed
                        | bcode_session_models::RuntimeWorkStatus::TimedOut
                ) =>
            {
                runtime_terminal_seen = true;
            }
            SessionEventKind::SystemMessage { text }
                if !terminal_seen
                    && [
                        "auto compaction failed: ",
                        "output-budget compaction failed: ",
                        "context overflow compaction failed: ",
                    ]
                    .iter()
                    .any(|prefix| text.strip_prefix(prefix) == Some(detail)) => {}
            _ => return Err(invalid_checkpoint()),
        }
    }
    Ok(())
}

/// Diagnose only complete operation-local activation windows. Older operations sharing a
/// source boundary cannot supply settlement for the selected operation's reservations.
fn restart_failure_blocker(
    request: &bcode_session_models::SessionCompactionRestartRequest,
    events: &[bcode_session_models::SessionEvent],
) -> bcode_session_models::SessionCompactionRecoveryBlocker {
    use bcode_session_models::SessionCompactionRecoveryBlocker as Blocker;
    let Some(terminal) = events
        .iter()
        .position(|event| event.sequence == request.expected_terminal_sequence)
    else {
        return Blocker::EvidenceUnavailable;
    };
    for (index, event) in events[..terminal].iter().enumerate().rev() {
        if event.sequence >= request.expected_checkpoint_sequence
            || !matches!(&event.kind,
            SessionEventKind::TraceEvent { trace } if trace.phase == SessionTracePhase::ContextCompactionStarted)
        {
            continue;
        }
        let window = &events[index..=terminal];
        if validate_settled_attempts(window).is_err() {
            continue;
        }
        if let Ok((checkpoint, usages)) =
            restart_attempt_evidence(request, &window[1..window.len() - 1])
        {
            let reserved = checkpoint
                .requests
                .iter()
                .chain(&checkpoint.consolidation_requests)
                .map(|count| usize::from(*count))
                .sum::<usize>()
                + checkpoint
                    .contract_continuation
                    .as_ref()
                    .map_or(0, |grant| usize::from(grant.requests));
            if usages.len() < reserved {
                return Blocker::UnsettledRequests;
            }
        }
    }
    Blocker::UnsupportedFailure
}

fn select_restart_failure_proof(
    request: &bcode_session_models::SessionCompactionRestartRequest,
    events: &[bcode_session_models::SessionEvent],
) -> Result<RestartFailureProof, CompactionError> {
    let terminal_index = events
        .iter()
        .position(|event| event.sequence == request.expected_terminal_sequence)
        .ok_or_else(invalid_checkpoint)?;
    validate_restart_tail(request, &events[terminal_index..])?;
    let events = &events[..=terminal_index];
    events
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, event)| {
            event.sequence < request.expected_checkpoint_sequence
                && matches!(&event.kind,
            SessionEventKind::TraceEvent { trace }
                if trace.phase == SessionTracePhase::ContextCompactionStarted)
        })
        .find_map(|(index, _)| validate_restart_failure_proof(request, &events[index..]).ok())
        .ok_or_else(invalid_checkpoint)
}

fn validate_restart_failure_proof(
    request: &bcode_session_models::SessionCompactionRestartRequest,
    events: &[bcode_session_models::SessionEvent],
) -> Result<RestartFailureProof, CompactionError> {
    if events.len() < 3
        || events.len() > RECOVERY_MAX_EVENTS
        || events
            .iter()
            .any(|event| event.session_id != request.session_id)
        || events
            .windows(2)
            .any(|pair| pair[0].sequence.checked_add(1) != Some(pair[1].sequence))
    {
        return Err(invalid_checkpoint());
    }
    let first = events.first().ok_or_else(invalid_checkpoint)?;
    let last = events.last().ok_or_else(invalid_checkpoint)?;
    let terminal_identity =
        bcode_session::compaction_event_identity(last).map_err(|_| invalid_checkpoint())?;
    if terminal_identity.sequence != request.expected_terminal_sequence
        || terminal_identity.sha256 != request.expected_terminal_sha256
    {
        return Err(invalid_checkpoint());
    }
    let SessionEventKind::TraceEvent { trace: started } = &first.kind else {
        return Err(invalid_checkpoint());
    };
    let SessionEventKind::TraceEvent { trace: finished } = &last.kind else {
        return Err(invalid_checkpoint());
    };
    if started.phase != SessionTracePhase::ContextCompactionStarted
        || finished.phase != SessionTracePhase::ContextCompactionFinished
    {
        return Err(invalid_checkpoint());
    }
    let SessionTracePayload::ContextCompaction {
        reason,
        compacted: false,
        message: Some(message),
        ..
    } = &finished.payload
    else {
        return Err(invalid_checkpoint());
    };
    if reason != "compaction_finished" {
        return Err(invalid_checkpoint());
    }
    validate_settled_attempts(events)?;
    let (checkpoint, usages) = restart_attempt_evidence(request, &events[1..events.len() - 1])?;
    validate_restart_rejection(request, &checkpoint, &usages, message)?;
    let abandoned_responses = if request.version == 2 {
        events
            .iter()
            .filter_map(|event| match &event.kind {
                SessionEventKind::ModelUsage { turn_id, usage }
                    if !usage.terminal
                        && turn_id.starts_with(&format!(
                            "{}-compact-{}-",
                            request.session_id, checkpoint.boundary
                        )) =>
                {
                    let id = usage.request_id.as_ref()?;
                    (!usages.contains(&(turn_id.clone(), id.clone()))).then_some(id.clone())
                }
                _ => None,
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    } else {
        Vec::new()
    };
    Ok(RestartFailureProof {
        abandoned_responses,
        operation_key: request.expected_operation_key.clone(),
        events: events
            .iter()
            .map(bcode_session::compaction_event_identity)
            .collect::<Result<_, _>>()
            .map_err(|_| invalid_checkpoint())?,
    })
}

/// The bounded proof may span a failed original attempt and a later policy-incompatible retry.
/// Every intervening attempt must finish unpublished before the next one starts.
fn validate_settled_attempts(
    events: &[bcode_session_models::SessionEvent],
) -> Result<(), CompactionError> {
    let mut active = false;
    let mut owner = None;
    for event in events {
        let SessionEventKind::TraceEvent { trace } = &event.kind else {
            continue;
        };
        match trace.phase {
            SessionTracePhase::ContextCompactionFinished => {
                if !active || owner.as_ref().is_some_and(|value| value != &trace.turn_id) {
                    return Err(invalid_checkpoint());
                }
                if !matches!(&trace.payload, SessionTracePayload::ContextCompaction {
                    compacted: false, reason, ..
                } if reason == "compaction_finished")
                {
                    return Err(invalid_checkpoint());
                }
                active = false;
            }
            SessionTracePhase::ContextCompactionStarted => {
                if active {
                    return Err(invalid_checkpoint());
                }
                active = true;
                owner = Some(trace.turn_id.clone());
            }
            _ => {}
        }
    }
    if active {
        return Err(invalid_checkpoint());
    }
    Ok(())
}

type RestartRequestUsages = BTreeSet<(String, String)>;

fn restart_attempt_evidence(
    request: &bcode_session_models::SessionCompactionRestartRequest,
    events: &[bcode_session_models::SessionEvent],
) -> Result<(LocalCheckpoint, RestartRequestUsages), CompactionError> {
    let boundary = events
        .iter()
        .find_map(|event| {
            if event.sequence != request.expected_checkpoint_sequence {
                return None;
            }
            bcode_session_models::CompactionSynthesisCheckpoint::from_event(event)
                .ok()
                .flatten()
                .and_then(|envelope| envelope.host_payload)
                .and_then(|payload| LocalCheckpoint::decode(&payload).ok())
                .map(|checkpoint| checkpoint.boundary)
        })
        .ok_or_else(invalid_checkpoint)?;
    let prefix = format!("{}-compact-{boundary}-", request.session_id);
    let mut checkpoint = None;
    let mut observed = BTreeSet::new();
    let mut usages = BTreeSet::new();
    for event in events {
        if matches!(
            event.kind,
            SessionEventKind::ContextCompacted { .. }
                | SessionEventKind::ProviderContextCompacted { .. }
        ) {
            return Err(invalid_checkpoint());
        }
        if let Some(envelope) =
            bcode_session_models::CompactionSynthesisCheckpoint::from_event(event)
                .map_err(|_| invalid_checkpoint())?
        {
            if envelope.operation_key != request.expected_operation_key
                || event.sequence > request.expected_checkpoint_sequence
            {
                return Err(invalid_checkpoint());
            }
            if event.sequence == request.expected_checkpoint_sequence {
                checkpoint = Some(LocalCheckpoint::decode(
                    envelope
                        .host_payload
                        .as_deref()
                        .ok_or_else(invalid_checkpoint)?,
                )?);
            }
        }
        if let SessionEventKind::ModelUsage { turn_id, usage } = &event.kind
            && turn_id.starts_with(&prefix)
        {
            let request_id = usage.request_id.as_deref().ok_or_else(invalid_checkpoint)?;
            observed.insert((turn_id.clone(), request_id.to_owned()));
        }
        if let SessionEventKind::ModelUsage { turn_id, usage } = &event.kind
            && usage.terminal
            && turn_id.starts_with(&prefix)
        {
            let request_id = usage.request_id.as_deref().ok_or_else(invalid_checkpoint)?;
            if !usages.insert((turn_id.clone(), request_id.to_owned())) {
                return Err(invalid_checkpoint());
            }
        }
    }
    let checkpoint = checkpoint.ok_or_else(invalid_checkpoint)?;
    if request.version == 2 {
        let reserved = checkpoint
            .requests
            .iter()
            .chain(&checkpoint.consolidation_requests)
            .map(|count| usize::from(*count))
            .sum::<usize>()
            + checkpoint
                .contract_continuation
                .as_ref()
                .map_or(0, |grant| usize::from(grant.requests));
        if observed.len() != reserved
            || observed
                .iter()
                .map(|(_, id)| id)
                .collect::<BTreeSet<_>>()
                .len()
                != observed.len()
        {
            return Err(invalid_checkpoint());
        }
    }
    Ok((checkpoint, usages))
}

fn validate_restart_rejection(
    request: &bcode_session_models::SessionCompactionRestartRequest,
    checkpoint: &LocalCheckpoint,
    usages: &RestartRequestUsages,
    message: &str,
) -> Result<(), CompactionError> {
    let prefix = format!("{}-compact-{}-", request.session_id, checkpoint.boundary);
    let reserved = checkpoint
        .requests
        .iter()
        .chain(&checkpoint.consolidation_requests)
        .map(|count| usize::from(*count))
        .sum::<usize>()
        + checkpoint
            .contract_continuation
            .as_ref()
            .map_or(0, |grant| usize::from(grant.requests));
    if (if request.version == 2 {
        usages.len() > reserved
    } else {
        usages.len() != reserved
    }) || usages
        .iter()
        .map(|(_, request_id)| request_id)
        .collect::<BTreeSet<_>>()
        .len()
        != usages.len()
        || usages
            .iter()
            .any(|(turn_id, _)| !turn_id.starts_with(&prefix))
    {
        return Err(invalid_checkpoint());
    }
    let rejection = format!(
        "compaction synthesis rejected after bounded repair; chunk={}/{} reason=",
        checkpoint.next_chunk + 1,
        checkpoint.manifest.len(),
    );
    // Only current canonical diagnostics are recognized. An unrelated failure or a mere
    // exhausted reservation never establishes settlement or safe replay.
    let bounded_repair = checkpoint.synthesis_phase == SynthesisPhase::Accumulating
        && checkpoint.requests.get(checkpoint.next_chunk) == Some(&2)
        && message.contains(&rejection);
    let incompatible = message
        .contains("invalid or incompatible compaction checkpoint; boundary unchanged")
        || message.contains("compaction synthesis policy changed or is incompatible; explicit supersession required; boundary unchanged");
    let consolidation = message
        .contains("compaction consolidation outcome is unresolved; boundary unchanged")
        || message.contains("derived consolidation rejected:");
    let migration = checkpoint.needs_upgrade()
        && (message
            .contains("explicit compaction restart allowance exhausted; boundary unchanged")
            || message
                .contains("explicit compaction restart deadline exhausted; boundary unchanged")
            || message.contains("compaction corrective deadline exhausted; boundary unchanged"));
    let cancelled_consolidation = checkpoint.synthesis_phase == SynthesisPhase::ConsolidatingV1
        && message == "context compaction failed: compaction cancelled";
    if !cancelled_consolidation
        && (!message.starts_with("context compaction failed:")
            || !message.ends_with("; boundary unchanged")
            || !(bounded_repair || incompatible || consolidation || migration))
    {
        return Err(invalid_checkpoint());
    }
    Ok(())
}

const RESTART_POINTER_FORMAT: &str = "bcode.compaction.supersession.v1";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RestartPointer {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    abandoned_responses: Vec<String>,
    format: String,
    request: bcode_session_models::SessionCompactionRestartRequest,
    replacement_key: String,
    replacement_payload_digest: String,
    initial_replacement_payload: String,
}

/// Stable indexed discovery namespace, distinct from both operational namespaces.
pub(super) fn restart_pointer_key(operation_key: &str) -> String {
    format!(
        "local-compaction-discovery-v1:{}",
        hex::encode(Sha256::digest(operation_key.as_bytes()))
    )
}

/// Publish both payloads in one ownership-fenced session transaction. Never append separately.
/// The transaction must also establish authoritative terminal failure (not reservation exhaustion),
/// compare canonical marker/source identities, and deduplicate the exact request operation ID.
/// Old checkpoint events remain canonical; the pointer supersedes their lookup, not their bytes.
pub(super) struct PreparedRestart {
    pub(super) request: bcode_session_models::SessionCompactionRestartRequest,
    pub(super) source: Vec<bcode_session::CompactionEventIdentity>,
    pub(super) replacement_key: String,
    pub(super) replacement_payload: String,
    pub(super) pointer_payload: String,
    pub(super) failure_evidence: Vec<bcode_session::CompactionEventIdentity>,
}

impl PreparedRestart {
    /// Convert validated host preparation to the session domain's atomic compare-and-supersede.
    pub(super) fn into_domain(
        self,
    ) -> Result<bcode_session::CompactionCheckpointRestart, CompactionError> {
        let rejection = self
            .failure_evidence
            .last()
            .cloned()
            .ok_or_else(invalid_checkpoint)?;
        Ok(bcode_session::CompactionCheckpointRestart {
            pointer_key: restart_pointer_key(&self.request.expected_operation_key),
            operation_key: self.request.expected_operation_key,
            expected_checkpoint_sequence: self.request.expected_checkpoint_sequence,
            expected_prior_marker: self.request.expected_prior_marker.map(|marker| {
                bcode_session::CompactionEventIdentity {
                    sequence: marker.sequence,
                    sha256: marker.sha256,
                }
            }),
            source: self.source,
            rejection,
            expected_tail: self.request.expected_tail_marker.map(|tail| {
                bcode_session::CompactionEventIdentity {
                    sequence: tail.sequence,
                    sha256: tail.sha256,
                }
            }),
            replacement_key: self.replacement_key,
            replacement_payload: self.replacement_payload,
            pointer_payload: self.pointer_payload,
        })
    }
}

/// Prepare a stable atomic request. An existing exact discovery pointer supplies the original
/// initial checkpoint and deadline, never the subsequently advanced replacement checkpoint.
pub(super) async fn prepare_restart_domain(
    state: &ServerState,
    request: &bcode_session_models::SessionCompactionRestartRequest,
    old: &LocalCheckpoint,
    failure: &RestartFailureProof,
    replacement: &LocalCheckpoint,
    source: (&CompactionPlan, &CompactionTranscript, &[String]),
) -> Result<bcode_session::CompactionCheckpointRestart, CompactionError> {
    let (plan, transcript, chunks) = source;
    let mut prepared =
        prepare_restart(request, old, failure, replacement, plan, transcript, chunks)?;
    if let Some(event) = state
        .sessions
        .session_compaction_checkpoint(
            request.session_id,
            &restart_pointer_key(&request.expected_operation_key),
        )
        .await?
    {
        let envelope = bcode_session_models::CompactionSynthesisCheckpoint::from_event(&event)
            .map_err(|_| invalid_checkpoint())?
            .ok_or_else(invalid_checkpoint)?;
        if envelope.operation_key != restart_pointer_key(&request.expected_operation_key) {
            return Err(invalid_checkpoint());
        }
        let payload = envelope
            .host_payload
            .as_deref()
            .ok_or_else(invalid_checkpoint)?;
        let pointer: RestartPointer =
            serde_json::from_str(payload).map_err(|_| invalid_checkpoint())?;
        if pointer.format != RESTART_POINTER_FORMAT
            || pointer.request != *request
            || pointer.abandoned_responses != failure.abandoned_responses
            || pointer.replacement_key != prepared.replacement_key
            || pointer.replacement_payload_digest != digest(&pointer.initial_replacement_payload)?
        {
            return Err(invalid_checkpoint());
        }
        let initial = LocalCheckpoint::decode(&pointer.initial_replacement_payload)?;
        if initial.resolved_policy != replacement.resolved_policy
            || initial.synthesis_policy != replacement.synthesis_policy
            || initial.packing_policy != replacement.packing_policy
            || initial.boundary != replacement.boundary
            || initial.source_digest != replacement.source_digest
            || initial.manifest != replacement.manifest
        {
            return Err(invalid_checkpoint());
        }
        prepared.replacement_payload = pointer.initial_replacement_payload;
        payload.clone_into(&mut prepared.pointer_payload);
    }
    prepared.into_domain()
}

/// Prepare current-format work only. The caller holds serialized runtime ownership; publication
/// still requires the atomic domain comparison described by `PreparedRestart`.
// Expansion is fresh synthesis, never a claim that partial carry covers new source.
// Prove the old selection is an unchanged prefix before admitting the larger one.
fn validates_legacy_expansion(
    old: &LocalCheckpoint,
    replacement: &LocalCheckpoint,
    plan: &CompactionPlan,
) -> Result<bool, CompactionError> {
    Ok(old.needs_upgrade()
        && replacement.upgrade.is_none()
        && replacement.boundary > old.boundary
        && plan
            .compactable_prefix
            .iter()
            .any(|event| event.sequence == old.boundary)
        && source_digest(
            &plan
                .compactable_prefix
                .iter()
                .filter(|event| event.sequence <= old.boundary)
                .cloned()
                .collect::<Vec<_>>(),
        )? == old.source_digest)
}

pub(super) fn prepare_restart(
    request: &bcode_session_models::SessionCompactionRestartRequest,
    old: &LocalCheckpoint,
    failure: &RestartFailureProof,
    replacement: &LocalCheckpoint,
    plan: &CompactionPlan,
    transcript: &CompactionTranscript,
    chunks: &[String],
) -> Result<PreparedRestart, CompactionError> {
    request.validate().map_err(|_| invalid_checkpoint())?;
    let expanded_legacy_source = validates_legacy_expansion(old, replacement, plan)?;
    if failure.operation_key != request.expected_operation_key
        || failure
            .events
            .first()
            .is_none_or(|event| event.sequence >= request.expected_checkpoint_sequence)
        || failure
            .events
            .last()
            .is_none_or(|event| event.sequence <= request.expected_checkpoint_sequence)
        || !failure
            .events
            .iter()
            .any(|event| event.sequence == request.expected_checkpoint_sequence)
        || old.checkpoint_sequence != request.expected_checkpoint_sequence
        || replacement.synthesis_phase != SynthesisPhase::Accumulating
        || (!expanded_legacy_source
            && (old.boundary != replacement.boundary
                || old.source_digest != replacement.source_digest))
        || (replacement.next_chunk != 0
            && !replacement.upgrade.as_ref().is_some_and(|seed| {
                seed.source_complete && replacement.next_chunk == replacement.manifest.len()
            }))
        || (replacement.upgrade.is_some() && !old.needs_upgrade())
        || replacement.requests.iter().any(|count| *count != 0)
        || replacement
            .consolidation_requests
            .iter()
            .any(|count| *count != 0)
        || replacement.synthesis_policy.is_none()
        || replacement.resolved_policy.is_none()
        || replacement.upgrade.as_ref().is_some_and(|seed| {
            old.migration_seed(transcript)
                .and_then(|expected| digest(&expected))
                .ok()
                != digest(seed).ok()
        })
    {
        return Err(invalid_checkpoint());
    }
    // Rebuilding the entire frozen selection avoids treating partial accepted carry as complete
    // coverage. Old accepted carry and allowances remain in their original canonical events.
    replacement.verify(plan, transcript, chunks)?;
    let source = plan
        .compactable_prefix
        .iter()
        .map(bcode_session::compaction_event_identity)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid_checkpoint())?;
    let replacement_key = format!("local-compaction-restart-v1:{}", request.operation_id);
    let mut replacement = LocalCheckpoint::decode(&replacement.encode()?)?;
    if replacement.restart_allowance.is_some() {
        return Err(invalid_checkpoint());
    }
    replacement.restart_allowance = Some(RestartAllowance {
        max_requests: replacement
            .upgrade
            .as_ref()
            .map_or(0, |seed| seed.spent_requests)
            .checked_add(request.budget.max_provider_requests)
            .ok_or_else(invalid_checkpoint)?,
        deadline_ms: current_unix_millis()
            .checked_add(u64::from(request.budget.timeout_ms))
            .ok_or_else(invalid_checkpoint)?,
    });
    if replacement.upgrade.is_some() {
        replacement.restart_allowance = None;
    }
    let replacement_payload = replacement.encode()?;
    let pointer = RestartPointer {
        abandoned_responses: failure.abandoned_responses.clone(),
        format: RESTART_POINTER_FORMAT.into(),
        request: request.clone(),
        replacement_key: replacement_key.clone(),
        replacement_payload_digest: digest(&replacement_payload)?,
        initial_replacement_payload: replacement_payload.clone(),
    };
    let pointer_payload = serde_json::to_string(&pointer).map_err(|_| invalid_checkpoint())?;
    if pointer_payload.len() > MAX_PAYLOAD {
        return Err(invalid_checkpoint());
    }
    Ok(PreparedRestart {
        request: request.clone(),
        source,
        replacement_key,
        replacement_payload,
        pointer_payload,
        failure_evidence: failure.events.clone(),
    })
}

/// Resolve at most eight explicit supersessions through exact indexed canonical checkpoints.
/// Returned key must be used for every subsequent save/reservation/publication; never write a
/// replacement checkpoint over its discovery pointer. Ordinary reads never replay or repair.
pub(super) async fn load_resolved(
    state: &ServerState,
    session_id: SessionId,
    key: &str,
) -> Result<(String, Option<LocalCheckpoint>), CompactionError> {
    let mut key = key.to_owned();
    let mut visited = BTreeSet::new();
    for _ in 0..=8 {
        if !visited.insert(key.clone()) {
            return Err(invalid_checkpoint());
        }
        let pointer_key = restart_pointer_key(&key);
        let pointer_event = state
            .sessions
            .session_compaction_checkpoint(session_id, &pointer_key)
            .await?;
        let is_pointer = pointer_event.is_some();
        let event = match pointer_event {
            Some(event) => Some(event),
            None => {
                state
                    .sessions
                    .session_compaction_checkpoint(session_id, &key)
                    .await?
            }
        };
        let Some(event) = event else {
            if visited.len() == 1 {
                return Ok((key, None));
            }
            return Err(invalid_checkpoint());
        };
        let envelope = bcode_session_models::CompactionSynthesisCheckpoint::from_event(&event)
            .map_err(|_| invalid_checkpoint())?
            .ok_or_else(invalid_checkpoint)?;
        if envelope.operation_key
            != if is_pointer {
                pointer_key.as_str()
            } else {
                key.as_str()
            }
        {
            return Err(invalid_checkpoint());
        }
        let Some(payload) = envelope.host_payload.as_deref() else {
            // A verified supersession chain may end at an explicitly retired replacement.
            // Absence is not retirement; missing linked records still fail above.
            return Ok((key, None));
        };
        if payload.len() > MAX_PAYLOAD {
            return Err(invalid_checkpoint());
        }
        let value: serde_json::Value =
            serde_json::from_str(payload).map_err(|_| invalid_checkpoint())?;
        if is_pointer {
            if value.get("format").and_then(serde_json::Value::as_str)
                != Some(RESTART_POINTER_FORMAT)
            {
                return Err(invalid_checkpoint());
            }
            let pointer: RestartPointer =
                serde_json::from_value(value).map_err(|_| invalid_checkpoint())?;
            pointer
                .request
                .validate()
                .map_err(|_| invalid_checkpoint())?;
            if pointer.request.session_id != session_id
                || pointer.request.expected_operation_key != key
                || pointer.request.expected_checkpoint_sequence >= event.sequence
                || pointer.replacement_key
                    != format!(
                        "local-compaction-restart-v1:{}",
                        pointer.request.operation_id
                    )
                || pointer.replacement_payload_digest
                    != digest(&pointer.initial_replacement_payload)?
            {
                return Err(invalid_checkpoint());
            }
            key = pointer.replacement_key;
            continue;
        }
        let mut checkpoint = LocalCheckpoint::decode(payload)?;
        checkpoint.checkpoint_sequence = event.sequence;
        return Ok((key, Some(checkpoint)));
    }
    Err(invalid_checkpoint())
}

/// Read exactly the caller-selected canonical checkpoint, including after supersession.
/// This is explicit restart evidence, not active-operation discovery or permission to resume it.
pub(super) async fn load_archived_checkpoint(
    state: &ServerState,
    request: &bcode_session_models::SessionCompactionRestartRequest,
) -> Result<LocalCheckpoint, CompactionError> {
    request.validate().map_err(|_| invalid_checkpoint())?;
    let sequence = request.expected_checkpoint_sequence;
    let events = state
        .sessions
        .session_events_range(request.session_id, sequence, sequence, 1)
        .await?;
    let [event] = events.as_slice() else {
        return Err(invalid_checkpoint());
    };
    if event.sequence != sequence || event.session_id != request.session_id {
        return Err(invalid_checkpoint());
    }
    let envelope = bcode_session_models::CompactionSynthesisCheckpoint::from_event(event)
        .map_err(|_| invalid_checkpoint())?
        .ok_or_else(invalid_checkpoint)?;
    if envelope.operation_key != request.expected_operation_key {
        return Err(invalid_checkpoint());
    }
    let mut checkpoint = LocalCheckpoint::decode(
        envelope
            .host_payload
            .as_deref()
            .ok_or_else(invalid_checkpoint)?,
    )?;
    checkpoint.checkpoint_sequence = sequence;
    Ok(checkpoint)
}

pub(super) async fn load(
    state: &ServerState,
    session_id: SessionId,
    key: &str,
) -> Result<Option<LocalCheckpoint>, CompactionError> {
    let Some(event) = state
        .sessions
        .session_compaction_checkpoint(session_id, key)
        .await?
    else {
        return Ok(None);
    };
    let envelope = bcode_session_models::CompactionSynthesisCheckpoint::from_event(&event)
        .map_err(|_| invalid_checkpoint())?
        .ok_or_else(invalid_checkpoint)?;
    if envelope.operation_key != key {
        return Err(invalid_checkpoint());
    }
    let mut checkpoint = envelope
        .host_payload
        .as_deref()
        .map(LocalCheckpoint::decode)
        .transpose()?;
    if let Some(checkpoint) = &mut checkpoint {
        checkpoint.checkpoint_sequence = event.sequence;
    }
    Ok(checkpoint)
}

/// Rebuild only the frozen, complete prefix; appended history remains an untouched tail.
pub(super) fn frozen_plan(
    history: &[bcode_session_models::SessionEvent],
    tool_output_chars: usize,
    boundary: u64,
) -> Result<CompactionPlan, CompactionError> {
    let history = compact_attach_history(history.to_vec());
    let marker = history
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, event)| match &event.kind {
            SessionEventKind::ContextCompacted {
                summary,
                compacted_through_sequence,
            } => Some((index, *compacted_through_sequence, summary.clone())),
            SessionEventKind::ProviderContextCompacted {
                snapshot,
                compacted_through_sequence,
            } => Some((
                index,
                *compacted_through_sequence,
                snapshot.portable_summary.clone(),
            )),
            _ => None,
        });
    let start = marker.as_ref().map_or(0, |(index, _, _)| index + 1);
    let frozen: Vec<_> = history[start..]
        .iter()
        .filter(|event| event.sequence <= boundary)
        .cloned()
        .collect();
    let units = conversational_units(&frozen, tool_output_chars);
    let selected: Vec<_> = units
        .iter()
        .take_while(|unit| {
            unit.events
                .last()
                .is_some_and(|event| event.sequence <= boundary)
        })
        .collect();
    let prefix: Vec<_> = selected
        .iter()
        .flat_map(|unit| unit.events.iter().cloned())
        .collect();
    if prefix.last().is_none_or(|event| event.sequence != boundary) {
        return Err(invalid_checkpoint());
    }
    let tail: Vec<_> = history[start..]
        .iter()
        .filter(|event| event.sequence > boundary)
        .cloned()
        .collect();
    Ok(CompactionPlan {
        prior_boundary: marker.as_ref().map(|(_, boundary, _)| *boundary),
        summary_input: selected
            .iter()
            .flat_map(|unit| unit.summary_input.iter().cloned())
            .collect(),
        provider_native_messages: session_events_to_model_messages_with_limit(
            &prefix,
            tool_output_chars,
        ),
        portable_fallback: marker.map_or_else(String::new, |(_, _, summary)| summary),
        estimated_prefix_tokens: selected
            .iter()
            .fold(0_u64, |sum, unit| sum.saturating_add(unit.estimated_tokens)),
        estimated_tail_tokens: conversational_units(&tail, tool_output_chars)
            .iter()
            .fold(0_u64, |sum, unit| sum.saturating_add(unit.estimated_tokens)),
        compacted_through_sequence: boundary,
        compactable_prefix: prefix,
        retained_tail: tail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(
        session_id: SessionId,
        sequence: u64,
        text: &str,
    ) -> bcode_session_models::SessionEvent {
        bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            session_id,
            sequence,
            timestamp_ms: sequence,
            provenance: None,
            kind: SessionEventKind::UserMessage {
                text: text.into(),
                client_id: ClientId::new(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        }
    }

    fn fixture(
        text: &str,
    ) -> (
        Vec<bcode_session_models::SessionEvent>,
        CompactionPlan,
        CompactionTranscript,
        Vec<String>,
    ) {
        let session = SessionId::new();
        let history = vec![user(session, 1, text), user(session, 2, "follow-up")];
        let plan = frozen_plan(&history, 2000, 1).expect("frozen plan");
        let transcript = compaction_transcript_from_plan(&plan);
        let chunks = compaction_evidence_input(&transcript, None).expect("chunks");
        (history, plan, transcript, chunks)
    }

    #[test]
    fn legacy_expansion_requires_unchanged_source_and_rejects_modern_operations() {
        let (history, plan, transcript, chunks) = fixture("Preserve original instructions.");
        let mut old = LocalCheckpoint::create(&plan, &transcript, &chunks).unwrap();
        let expanded = frozen_plan(&history, 2000, 2).unwrap();
        let expanded_transcript = compaction_transcript_from_plan(&expanded);
        let expanded_chunks = compaction_evidence_input(&expanded_transcript, None).unwrap();
        let replacement = LocalCheckpoint::create_with_policy(
            &expanded,
            &expanded_transcript,
            &expanded_chunks,
            capacity_policy(),
        )
        .unwrap();
        assert!(validates_legacy_expansion(&old, &replacement, &expanded).unwrap());
        let altered = vec![
            user(history[0].session_id, 1, "Altered instructions"),
            history[1].clone(),
        ];
        let altered = frozen_plan(&altered, 2000, 2).unwrap();
        assert!(!validates_legacy_expansion(&old, &replacement, &altered).unwrap());
        old = LocalCheckpoint::create_with_policy(&plan, &transcript, &chunks, capacity_policy())
            .unwrap();
        old.packing_policy = "adaptive-complete-request-v1:policy-digest".into();
        assert!(!validates_legacy_expansion(&old, &replacement, &expanded).unwrap());
    }

    #[test]
    fn corrective_reservations_preserve_spent_work_and_survive_restart_without_renewal() {
        let (_, plan, transcript, chunks) = fixture("Preserve every accepted source.");
        let mut checkpoint =
            LocalCheckpoint::create_with_policy(&plan, &transcript, &chunks, capacity_policy())
                .unwrap();
        checkpoint.reserve_slot(0).unwrap();
        checkpoint.reserve_slot(0).unwrap();
        assert!(checkpoint.reserve_slot(0).is_err());
        let before = serde_json::to_value(&checkpoint.evidence).unwrap();
        checkpoint.contract_continuation = Some(ContractContinuation {
            version: 1,
            contract: "section-schema-v1".into(),
            chunk: 0,
            requests: 0,
            baseline_requests: 2,
            reserved: 0,
            pending: false,
            deadline_ms: current_unix_millis() + 300_000,
            terminal_sequence: 10,
            terminal_sha256: "ab".repeat(32),
        });
        assert_eq!(checkpoint.reserve_slot(0).unwrap(), 0);
        let mut resumed = LocalCheckpoint::decode(&checkpoint.encode().unwrap()).unwrap();
        assert_eq!(resumed.requests[0], 2);
        assert_eq!(resumed.consumed_requests(), 3);
        assert_eq!(serde_json::to_value(&resumed.evidence).unwrap(), before);
        assert!(resumed.reserve_slot(0).is_err()); // Interrupted dispatch is not replayable.
        resumed.contract_continuation.as_mut().unwrap().pending = false; // Complete response settled.
        assert_eq!(resumed.reserve_slot(0).unwrap(), 1);
        assert!(resumed.reserve_slot(0).is_err());
        let mut resumed = LocalCheckpoint::decode(&resumed.encode().unwrap()).unwrap();
        assert!(resumed.reserve_slot(0).is_err());
        resumed.contract_continuation.as_mut().unwrap().deadline_ms = 1;
        assert!(resumed.verify_restart_allowance().is_err());
        // Completed accepted evidence can finalize after dispatch expiry, but unresolved work cannot.
        assert!(resumed.verify_publication_deadline().is_err());
        resumed.contract_continuation.as_mut().unwrap().pending = false;
        resumed.next_chunk = resumed.manifest.len();
        resumed.restart_allowance = Some(RestartAllowance {
            max_requests: 64,
            deadline_ms: 1,
        });
        assert!(resumed.verify_publication_deadline().is_ok());
        resumed.contract_continuation.as_mut().unwrap().pending = true;
        assert!(resumed.verify_publication_deadline().is_err());
        resumed.contract_continuation = None;
        assert!(resumed.verify_publication_deadline().is_err()); // Explicit-only deadline is not waived.
    }

    #[tokio::test]
    async fn corrective_admission_requires_exact_settled_contract_rejection() {
        Box::pin(corrective_admission_case(true)).await;
        Box::pin(corrective_admission_case(false)).await;
    }

    async fn corrective_admission_case(settled: bool) {
        let root = tempfile::tempdir().unwrap();
        let sessions = bcode_session::SessionManager::default();
        let session = sessions
            .create_session(None, root.path().to_owned())
            .await
            .unwrap();
        let id = session.id;
        drop(session);
        for text in ["Keep source attribution", "continue"] {
            sessions
                .append_event(
                    id,
                    SessionEventKind::UserMessage {
                        client_id: ClientId::new(),
                        text: text.into(),
                        admission: bcode_session_models::TurnAdmissionMetadata::default(),
                    },
                )
                .await
                .unwrap();
        }
        let state = crate::tests::test_server_state_with_fake_provider(sessions);
        let history = state
            .sessions
            .activated_conversation_snapshot(id)
            .await
            .unwrap()
            .events;
        let plan = frozen_plan(&history, 2000, history[0].sequence).unwrap();
        let transcript = compaction_transcript_from_plan(&plan);
        let chunks = compaction_evidence_input(&transcript, None).unwrap();
        let mut checkpoint =
            LocalCheckpoint::create_with_policy(&plan, &transcript, &chunks, capacity_policy())
                .unwrap();
        append_context_compaction_trace(
            &state,
            id,
            CompactionTraceKind::Started,
            "compaction_started",
            0,
            false,
            None,
        )
        .await;
        for request in 0..2 {
            checkpoint.reserve_slot(0).unwrap();
            checkpoint
                .save(&state, id, "contract-failure")
                .await
                .unwrap();
            state
                .sessions
                .append_event(
                    id,
                    SessionEventKind::ModelUsage {
                        turn_id: format!("{id}-compact-{}-attempt-{request}", checkpoint.boundary),
                        usage: bcode_session_models::SessionTokenUsage {
                            request_id: Some(format!("attempt-{request}")),
                            terminal: settled,
                            ..Default::default()
                        },
                    },
                )
                .await
                .unwrap();
        }
        append_context_compaction_trace(&state, id, CompactionTraceKind::Finished, "compaction_finished", 0, false,
            Some(format!("context compaction failed: provider error: compaction synthesis rejected after bounded repair; chunk=1/{} reason=invalid_json; boundary unchanged", chunks.len()))).await;
        let result = checkpoint
            .admit_contract_continuation(&state, id, "contract-failure")
            .await;
        if settled {
            assert!(result.unwrap());
            assert_eq!(checkpoint.requests[0], 2);
            assert_eq!(checkpoint.next_chunk, 0);
            assert!(
                !checkpoint
                    .admit_contract_continuation(&state, id, "contract-failure")
                    .await
                    .unwrap()
            );
            let loaded = load(&state, id, "contract-failure").await.unwrap().unwrap();
            assert_eq!(loaded.contract_continuation.unwrap().reserved, 0);
        } else {
            assert!(result.is_err());
            assert!(checkpoint.contract_continuation.is_none());
        }
        drop(state);
    }

    #[test]
    fn appended_tail_does_not_hide_frozen_operation_or_change_manifest() {
        let (mut history, plan, transcript, chunks) = fixture("Preserve read-only scope.");
        let checkpoint = LocalCheckpoint::create(&plan, &transcript, &chunks).unwrap();
        let selection = SessionModelSelection::default();
        let key = operation_key(
            &history,
            &selection,
            2000,
            CompactionPlanningPolicy::OverflowRecovery,
        )
        .unwrap();
        history.push(user(
            history[0].session_id,
            3,
            "New follow-up must remain canonical.",
        ));
        assert_eq!(
            key,
            operation_key(
                &history,
                &selection,
                2000,
                CompactionPlanningPolicy::OverflowRecovery
            )
            .unwrap()
        );
        let resumed = frozen_plan(&history, 2000, checkpoint.boundary).unwrap();
        checkpoint
            .verify(
                &resumed,
                &compaction_transcript_from_plan(&resumed),
                &chunks,
            )
            .unwrap();
        assert_eq!(resumed.retained_tail.len(), 2);
        history[0].kind = SessionEventKind::UserMessage {
            text: "Changed source".into(),
            client_id: ClientId::new(),
            admission: bcode_session_models::TurnAdmissionMetadata::default(),
        };
        let changed = frozen_plan(&history, 2000, 1).unwrap();
        assert!(checkpoint.verify(&changed, &transcript, &chunks).is_err());
    }

    #[test]
    fn structural_rows_are_part_of_exact_frozen_source_identity() {
        let (mut history, _, _, _) = fixture("Read-only scope.");
        let mut structural = history[0].clone();
        structural.sequence = 2;
        structural.kind = SessionEventKind::ModelTurnStarted {
            turn_id: "turn-one".into(),
        };
        history[1].sequence = 3;
        history.insert(1, structural);
        let plan = frozen_plan(&history, 2000, 2).unwrap();
        let transcript = compaction_transcript_from_plan(&plan);
        let chunks = compaction_evidence_input(&transcript, None).unwrap();
        let checkpoint = LocalCheckpoint::create(&plan, &transcript, &chunks).unwrap();
        assert_eq!(plan.compactable_prefix.len(), 2);
        assert_eq!(plan.compacted_through_sequence, 2);
        history[1].timestamp_ms += 1;
        let changed = frozen_plan(&history, 2000, 2).unwrap();
        assert!(checkpoint.verify(&changed, &transcript, &chunks).is_err());
    }

    #[test]
    fn packing_policy_roundtrip_and_manifest_reject_repacking_accepted_work() {
        let (_, plan, transcript, chunks) = fixture(&"chronological evidence ".repeat(2000));
        assert!(chunks.len() > 1);
        let mut checkpoint = LocalCheckpoint::create(&plan, &transcript, &chunks).unwrap();
        checkpoint.packing_policy = "adaptive-complete-request-v1:policy-digest".into();
        checkpoint.reserve_slot(0).unwrap();
        let mut restored = LocalCheckpoint::decode(&checkpoint.encode().unwrap()).unwrap();
        assert_eq!(restored.packing_policy, checkpoint.packing_policy);
        restored.verify_chunks(&chunks).unwrap();
        assert!(restored.verify_chunks(&[chunks.concat()]).is_err());
        assert_eq!(restored.reserve_slot(0).unwrap(), 1);
        assert!(restored.reserve_slot(0).is_err());
        let mut legacy =
            serde_json::to_value(LocalCheckpoint::create(&plan, &transcript, &chunks).unwrap())
                .unwrap();
        legacy.as_object_mut().unwrap().remove("packing_policy");
        assert_eq!(
            LocalCheckpoint::decode(&legacy.to_string())
                .unwrap()
                .packing_policy,
            "legacy-chars-v1"
        );
    }

    fn capacity_policy() -> synthesis::SynthesisPolicy {
        synthesis::SynthesisPolicy {
            carry_chars: 24_000,
            wire_bytes: 96_000,
            section_chars: 12_000,
        }
    }

    #[test]
    fn capacity_policy_is_exact_and_old_exhausted_work_is_not_reset() {
        let (_, plan, transcript, chunks) = fixture("Do not modify files.");
        let mut legacy = LocalCheckpoint::create(&plan, &transcript, &chunks).unwrap();
        legacy.reserve_slot(0).unwrap();
        legacy.reserve_slot(0).unwrap();
        let payload = legacy.encode().unwrap();
        let mut restored = LocalCheckpoint::decode(&payload).unwrap();
        assert!(restored.verify_policy(capacity_policy()).is_err());
        assert!(restored.reserve_slot(0).is_err());
        assert_eq!(restored.encode().unwrap(), payload);

        let mut current =
            LocalCheckpoint::create_with_policy(&plan, &transcript, &chunks, capacity_policy())
                .unwrap();
        let resolved = serde_json::json!({"version": 1, "estimator_version": 3, "maximum_retained_tokens": 8000});
        assert!(current.verify_capacity_policy(&resolved).is_err());
        assert!(current.capacity_policy_digest().is_err());
        current.bind_capacity_policy(&resolved).unwrap();
        let frozen_digest = current.capacity_policy_digest().unwrap();
        assert!(current.bind_resolved_policy(&resolved).is_err());
        let restored = LocalCheckpoint::decode(&current.encode().unwrap()).unwrap();
        restored.verify_capacity_policy(&resolved).unwrap();
        assert_eq!(restored.capacity_policy_digest().unwrap(), frozen_digest);
        assert_eq!(
            restored.resolved_policy::<serde_json::Value>().unwrap(),
            resolved
        );
        let changed_budget = serde_json::json!({"version": 1, "estimator_version": 3, "maximum_retained_tokens": 8001});
        assert!(restored.verify_capacity_policy(&changed_budget).is_err());
        assert_ne!(frozen_digest, digest(&changed_budget).unwrap());
        current.verify_policy(capacity_policy()).unwrap();
        let mut changed = capacity_policy();
        changed.section_chars -= 1;
        assert!(current.verify_policy(changed).is_err());
        let mut payload = serde_json::to_value(current).unwrap();
        payload["synthesis_policy"]["format"] = serde_json::json!("future-policy");
        assert!(LocalCheckpoint::decode(&payload.to_string()).is_err());
    }

    #[test]
    fn consolidation_fences_ambiguous_calls_and_retains_finite_allowance() {
        let (_, plan, transcript, chunks) = fixture("Do not modify files.");
        let mut checkpoint =
            LocalCheckpoint::create_with_policy(&plan, &transcript, &chunks, capacity_policy())
                .unwrap();
        assert!(checkpoint.reserve_consolidation_slot().is_err());
        checkpoint.reserve_slot(0).unwrap();
        let evidence = CompactionEvidence {
            format: COMPACTION_SUMMARY_FORMAT.into(),
            current_scope: vec![CompactionScope {
                source_sequence: 1,
                statement: "Do not modify files.".into(),
                supersedes: Vec::new(),
            }],
            continuation: "Continue inspection.".into(),
        };
        checkpoint.accept(&evidence, &BTreeSet::from([1]), &BTreeSet::new());
        assert!(checkpoint.accepted_chunk(0));
        assert_eq!(checkpoint.reserve_consolidation_slot().unwrap(), 0);
        let payload = checkpoint.encode().unwrap();
        let restored = LocalCheckpoint::decode(&payload).unwrap();
        assert!(!restored.accepted_chunk(0));
        assert!(restored.verify_policy(capacity_policy()).is_err());
        assert_eq!(restored.requests, checkpoint.requests);
        assert_eq!(restored.consolidation_requests, vec![1; chunks.len()]);
        let mut invalid = evidence.clone();
        invalid.current_scope.clear();
        assert!(checkpoint.accept_consolidation(&invalid).is_err());
        assert_eq!(checkpoint.encode().unwrap(), payload);
        assert!(checkpoint.reserve_consolidation_slot().is_err());
        checkpoint.reject_consolidation_response().unwrap();
        assert_eq!(checkpoint.reserve_consolidation_slot().unwrap(), 1);
        assert!(checkpoint.reserve_consolidation_slot().is_err());
        checkpoint.accept_consolidation(&evidence).unwrap();
        let mut restored = LocalCheckpoint::decode(&checkpoint.encode().unwrap()).unwrap();
        restored.verify_policy(capacity_policy()).unwrap();
        assert!(restored.accepted_chunk(0));
        assert!(restored.reserve_consolidation_slot().is_err());
        assert_eq!(restored.next_chunk, 1);
        assert_eq!(restored.user_sequences, BTreeSet::from([1]));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One canonical lifecycle covers original rejection and settled retry.
    fn canonical_completed_rejection_proves_restart_but_reservations_alone_do_not() {
        let (history, plan, transcript, chunks) = fixture("Do not modify files.");
        let mut checkpoint = LocalCheckpoint::create(&plan, &transcript, &chunks).unwrap();
        checkpoint.reserve_slot(0).unwrap();
        checkpoint.reserve_slot(0).unwrap();
        let session_id = history[0].session_id;
        let mut request = bcode_session_models::SessionCompactionRestartRequest {
            version: 1,
            session_id,
            operation_id: uuid::Uuid::new_v4(),
            expected_operation_key: "failed-operation".into(),
            expected_checkpoint_sequence: 4,
            expected_terminal_sequence: 6,
            expected_terminal_sha256: "ab".repeat(32),
            expected_tail_marker: None,
            expected_prior_marker: None,
            budget: bcode_session_models::SessionCompactionRestartBudget {
                max_provider_requests: 4,
                timeout_ms: 60_000,
            },
        };
        let mut events: Vec<_> = (1..=6)
            .map(|sequence| user(session_id, sequence, "unused"))
            .collect();
        let trace = |phase, compacted, message| SessionEventKind::TraceEvent {
            trace: Box::new(SessionTraceEvent {
                timestamp_ms: 1,
                turn_id: Some("owning-turn".into()),
                phase,
                payload: SessionTracePayload::ContextCompaction {
                    reason: "compaction_finished".into(),
                    projected_context_chars: 0,
                    compacted,
                    message,
                },
            }),
        };
        events[0].kind = trace(SessionTracePhase::ContextCompactionStarted, false, None);
        events[3].kind = bcode_session_models::CompactionSynthesisCheckpoint {
            version: 1,
            operation_key: request.expected_operation_key.clone(),
            host_payload: Some(checkpoint.encode().unwrap()),
        }
        .into_event_kind(1)
        .unwrap();
        for (index, request_number) in [(2, 1), (4, 2)] {
            events[index].kind = SessionEventKind::ModelUsage {
                turn_id: format!(
                    "{session_id}-compact-{}-attempt-{request_number}",
                    checkpoint.boundary
                ),
                usage: bcode_session_models::SessionTokenUsage {
                    request_id: Some(format!("request-{request_number}")),
                    terminal: true,
                    ..Default::default()
                },
            };
        }
        events[5].kind = trace(
            SessionTracePhase::ContextCompactionFinished,
            false,
            Some(format!(
                "context compaction failed: model provider error: compaction synthesis rejected after bounded repair; chunk=1/{} reason=carry_budget; boundary unchanged",
                chunks.len(),
            )),
        );
        request.expected_terminal_sha256 =
            bcode_session::compaction_event_identity(events.last().unwrap())
                .unwrap()
                .sha256;
        assert!(validate_restart_failure_proof(&request, &events).is_ok());

        // A policy-incompatible retry can fail before making a NEW request. Its checkpoint
        // predates that retry, so the bounded proof must retain earlier terminal usages.
        let mut retried = events.clone();
        retried.push(user(session_id, 7, "unused"));
        retried[6].kind = trace(SessionTracePhase::ContextCompactionStarted, false, None);
        retried.push(user(session_id, 8, "unused"));
        retried[7].kind = trace(SessionTracePhase::ContextCompactionFinished, false,
            Some("context compaction failed: provider error: invalid or incompatible compaction checkpoint; boundary unchanged".into()));
        let mut retry_request = request.clone();
        retry_request.expected_terminal_sequence = 8;
        retry_request.expected_terminal_sha256 =
            bcode_session::compaction_event_identity(&retried[7])
                .unwrap()
                .sha256;
        assert!(select_restart_failure_proof(&retry_request, &retried).is_ok());
        // A spent checkpoint remains recoverable after thousands of ordinary events.
        // Unrelated terminal model requests are not compaction attempt settlement.
        let mut aged = events.clone();
        aged.extend((7..=5_006).map(|sequence| user(session_id, sequence, "later history")));
        aged[100].kind = SessionEventKind::ModelUsage {
            turn_id: "ordinary-turn".into(),
            usage: bcode_session_models::SessionTokenUsage {
                request_id: Some("ordinary-provider-request".into()),
                terminal: true,
                ..bcode_session_models::SessionTokenUsage::default()
            },
        };
        let mut aged_start = events[0].clone();
        aged_start.sequence = 5_007;
        aged.push(aged_start);
        let mut aged_failure = events[5].clone();
        aged_failure.sequence = 5_008;
        aged_failure.kind = trace(SessionTracePhase::ContextCompactionFinished, false,
            Some("context compaction failed: invalid or incompatible compaction checkpoint; boundary unchanged".into()));
        aged.push(aged_failure);
        let mut aged_request = request.clone();
        aged_request.expected_terminal_sequence = 5_008;
        aged_request.expected_terminal_sha256 =
            bcode_session::compaction_event_identity(aged.last().unwrap())
                .unwrap()
                .sha256;
        assert!(select_restart_failure_proof(&aged_request, &aged).is_ok());
        let mut missing_aged_usage = aged.clone();
        if let SessionEventKind::ModelUsage { usage, .. } = &mut missing_aged_usage[4].kind {
            usage.terminal = false;
        }
        assert!(select_restart_failure_proof(&aged_request, &missing_aged_usage).is_err());
        let mut missing_usage = retried.clone();
        if let SessionEventKind::ModelUsage { usage, .. } = &mut missing_usage[4].kind {
            usage.terminal = false;
        }
        assert!(select_restart_failure_proof(&retry_request, &missing_usage).is_err());
        assert_eq!(
            restart_failure_blocker(&retry_request, &missing_usage),
            bcode_session_models::SessionCompactionRecoveryBlocker::UnsettledRequests
        );
        let mut mixed = missing_usage.clone();
        let mut unrelated = events[3].clone();
        unrelated.sequence = 0;
        unrelated.kind = bcode_session_models::CompactionSynthesisCheckpoint {
            version: 1,
            operation_key: "unrelated-predecessor".into(),
            host_payload: Some(checkpoint.encode().unwrap()),
        }
        .into_event_kind(0)
        .unwrap();
        mixed.insert(0, unrelated);
        assert_eq!(
            restart_failure_blocker(&retry_request, &mixed),
            bcode_session_models::SessionCompactionRecoveryBlocker::UnsettledRequests
        );
        assert!(select_restart_failure_proof(&retry_request, &mixed).is_err());
        let mut abandon_request = retry_request.clone();
        abandon_request.version = 2;
        assert!(select_restart_failure_proof(&abandon_request, &mixed).is_ok());
        // Abandonment does not weaken activation ownership or marker fencing.
        let mut wrong_owner = mixed.clone();
        if let SessionEventKind::TraceEvent { trace } = &mut wrong_owner.last_mut().unwrap().kind {
            trace.turn_id = Some("foreign".into());
        }
        abandon_request.expected_terminal_sha256 =
            bcode_session::compaction_event_identity(wrong_owner.last().unwrap())
                .unwrap()
                .sha256;
        assert!(select_restart_failure_proof(&abandon_request, &wrong_owner).is_err());
        let mut mismatched_owner = retried;
        if let SessionEventKind::TraceEvent { trace } = &mut mismatched_owner[7].kind {
            trace.turn_id = Some("foreign-turn".into());
        }
        retry_request.expected_terminal_sha256 =
            bcode_session::compaction_event_identity(&mismatched_owner[7])
                .unwrap()
                .sha256;
        assert!(select_restart_failure_proof(&retry_request, &mismatched_owner).is_err());

        let mut settled = events.clone();
        settled.push(user(session_id, 7, "unused"));
        settled[6].kind = SessionEventKind::ModelTurnFinished {
            turn_id: "owning-turn".into(),
            outcome: bcode_session_models::ModelTurnOutcome::Cancelled,
            message: None,
        };
        let mut tailed_request = request.clone();
        let tail = bcode_session::compaction_event_identity(&settled[6]).unwrap();
        tailed_request.expected_tail_marker =
            Some(bcode_session_models::SessionCompactionRestartMarker {
                sequence: tail.sequence,
                sha256: tail.sha256,
            });
        assert!(select_restart_failure_proof(&tailed_request, &settled).is_ok());
        assert!(select_restart_failure_proof(&request, &settled).is_err());
        settled.push(user(
            session_id,
            8,
            "new work must not be silently accepted",
        ));
        let tail = bcode_session::compaction_event_identity(&settled[7]).unwrap();
        tailed_request.expected_tail_marker =
            Some(bcode_session_models::SessionCompactionRestartMarker {
                sequence: tail.sequence,
                sha256: tail.sha256,
            });
        assert!(select_restart_failure_proof(&tailed_request, &settled).is_err());
        let mut ambiguous = events.clone();
        if let SessionEventKind::ModelUsage { usage, .. } = &mut ambiguous[4].kind {
            usage.terminal = false;
        }
        assert!(validate_restart_failure_proof(&request, &ambiguous).is_err());
        let mut unknown_failure = events.clone();
        unknown_failure[5].kind = trace(
            SessionTracePhase::ContextCompactionFinished,
            false,
            Some("context compaction failed: request interrupted".into()),
        );
        assert!(validate_restart_failure_proof(&request, &unknown_failure).is_err());
        let mut gap = events;
        gap.remove(1);
        assert!(validate_restart_failure_proof(&request, &gap).is_err());
    }

    #[test]
    fn prepared_restart_preserves_old_bytes_and_binds_distinct_identity_and_allowance() {
        let (history, plan, transcript, chunks) = fixture("Do not modify files.");
        let mut old = LocalCheckpoint::create(&plan, &transcript, &chunks).unwrap();
        old.checkpoint_sequence = 10;
        old.reserve_slot(0).unwrap();
        old.reserve_slot(0).unwrap();
        let old_payload = old.encode().unwrap();
        let mut replacement =
            LocalCheckpoint::create_with_policy(&plan, &transcript, &chunks, capacity_policy())
                .unwrap();
        replacement
            .bind_capacity_policy(&serde_json::json!({"version": 1}))
            .unwrap();
        let request = bcode_session_models::SessionCompactionRestartRequest {
            version: 1,
            session_id: history[0].session_id,
            operation_id: uuid::Uuid::new_v4(),
            expected_operation_key: "old-operation".into(),
            expected_checkpoint_sequence: 10,
            expected_terminal_sequence: 11,
            expected_terminal_sha256: "ab".repeat(32),
            expected_tail_marker: None,
            expected_prior_marker: None,
            budget: bcode_session_models::SessionCompactionRestartBudget {
                max_provider_requests: 3,
                timeout_ms: 60_000,
            },
        };
        let failure = RestartFailureProof {
            abandoned_responses: Vec::new(),
            operation_key: request.expected_operation_key.clone(),
            events: [9, 10, 11]
                .into_iter()
                .map(|sequence| bcode_session::CompactionEventIdentity {
                    sequence,
                    sha256: "0".repeat(64),
                })
                .collect(),
        };
        let prepared = prepare_restart(
            &request,
            &old,
            &failure,
            &replacement,
            &plan,
            &transcript,
            &chunks,
        )
        .unwrap();
        assert_eq!(old.encode().unwrap(), old_payload);
        assert_ne!(prepared.replacement_key, request.expected_operation_key);
        let restored = LocalCheckpoint::decode(&prepared.replacement_payload).unwrap();
        assert_eq!(restored.restart_allowance.unwrap().max_requests, 3);
        let pointer: RestartPointer = serde_json::from_str(&prepared.pointer_payload).unwrap();
        assert_eq!(pointer.request, request);
        assert_eq!(
            pointer.replacement_payload_digest,
            digest(&prepared.replacement_payload).unwrap()
        );
        let domain = prepared.into_domain().unwrap();
        assert_eq!(domain.operation_key, request.expected_operation_key);
        assert_eq!(
            domain.expected_checkpoint_sequence,
            request.expected_checkpoint_sequence
        );
        assert_eq!(domain.rejection.sequence, 11);
        assert_eq!(
            domain.pointer_key,
            restart_pointer_key(&request.expected_operation_key)
        );
        assert_ne!(domain.pointer_key, domain.operation_key);
        assert_ne!(domain.pointer_key, domain.replacement_key);
        let mut stale = request;
        stale.expected_checkpoint_sequence += 1;
        assert!(
            prepare_restart(
                &stale,
                &old,
                &failure,
                &replacement,
                &plan,
                &transcript,
                &chunks
            )
            .is_err()
        );
    }

    fn consolidation_restart_request(
        session_id: SessionId,
    ) -> bcode_session_models::SessionCompactionRestartRequest {
        bcode_session_models::SessionCompactionRestartRequest {
            version: 1,
            session_id,
            operation_id: uuid::Uuid::new_v4(),
            expected_operation_key: "old-operation".into(),
            expected_checkpoint_sequence: 10,
            expected_terminal_sequence: 11,
            expected_terminal_sha256: "ab".repeat(32),
            expected_tail_marker: None,
            expected_prior_marker: None,
            budget: bcode_session_models::SessionCompactionRestartBudget {
                max_provider_requests: 3,
                timeout_ms: 60_000,
            },
        }
    }

    #[test]
    fn interrupted_consolidation_restart_requires_all_phase_reservations_settled() {
        let (history, plan, transcript, chunks) = fixture("Preserve user authority.");
        let mut old =
            LocalCheckpoint::create_with_policy(&plan, &transcript, &chunks, capacity_policy())
                .unwrap();
        old.bind_capacity_policy(&serde_json::json!({"version": 1}))
            .unwrap();
        old.requests[0] = 1;
        old.next_chunk = 1;
        old.consolidation_requests[0] = 1;
        old.synthesis_phase = SynthesisPhase::ConsolidatingV1;
        old.evidence.continuation = "Accepted facts remain unchanged.".into();
        old.checkpoint_sequence = 10;
        let old_payload = old.encode().unwrap();
        LocalCheckpoint::decode(&old_payload).unwrap();
        let request = consolidation_restart_request(history[0].session_id);
        let usage = |number| {
            (
                format!("{}-compact-{}-{number}", request.session_id, old.boundary),
                format!("request-{number}"),
            )
        };
        let mut usages = BTreeSet::from([usage(1)]);
        let message = "context compaction failed: compaction consolidation outcome is unresolved; boundary unchanged";
        assert!(validate_restart_rejection(&request, &old, &usages, message).is_err());
        usages.insert(usage(2));
        validate_restart_rejection(&request, &old, &usages, message).unwrap();
        validate_restart_rejection(
            &request,
            &old,
            &usages,
            "context compaction failed: compaction cancelled",
        )
        .unwrap();
        assert!(
            validate_restart_rejection(
                &request,
                &old,
                &BTreeSet::from([usage(1)]),
                "context compaction failed: compaction cancelled"
            )
            .is_err()
        );
        let mut accumulating = LocalCheckpoint::decode(&old_payload).unwrap();
        accumulating.synthesis_phase = SynthesisPhase::Accumulating;
        assert!(
            validate_restart_rejection(
                &request,
                &accumulating,
                &usages,
                "context compaction failed: compaction cancelled"
            )
            .is_err()
        );
        let mut replacement =
            LocalCheckpoint::create_with_policy(&plan, &transcript, &chunks, capacity_policy())
                .unwrap();
        replacement
            .bind_capacity_policy(&serde_json::json!({"version": 1}))
            .unwrap();
        let proof = RestartFailureProof {
            abandoned_responses: Vec::new(),
            operation_key: request.expected_operation_key.clone(),
            events: [9, 10, 11]
                .into_iter()
                .map(|sequence| bcode_session::CompactionEventIdentity {
                    sequence,
                    sha256: "0".repeat(64),
                })
                .collect(),
        };
        let prepared = prepare_restart(
            &request,
            &old,
            &proof,
            &replacement,
            &plan,
            &transcript,
            &chunks,
        )
        .unwrap();
        let restarted = LocalCheckpoint::decode(&prepared.replacement_payload).unwrap();
        assert!(restarted.requests.iter().all(|count| *count == 0));
        assert!(
            restarted
                .consolidation_requests
                .iter()
                .all(|count| *count == 0)
        );
        assert_eq!(old.encode().unwrap(), old_payload);
    }

    #[test]
    fn explicit_restart_allowance_is_shared_by_all_request_phases_and_not_refunded() {
        let (_, plan, transcript, chunks) = fixture("Do not modify files.");
        let mut checkpoint =
            LocalCheckpoint::create_with_policy(&plan, &transcript, &chunks, capacity_policy())
                .unwrap();
        checkpoint
            .bind_capacity_policy(&serde_json::json!({"version": 1}))
            .unwrap();
        checkpoint.restart_allowance = Some(RestartAllowance {
            max_requests: 1,
            deadline_ms: current_unix_millis() + 60_000,
        });
        checkpoint.reserve_slot(0).unwrap();
        let mut restored = LocalCheckpoint::decode(&checkpoint.encode().unwrap()).unwrap();
        assert!(restored.reserve_slot(0).is_err());
        assert!(restored.reserve_consolidation_slot().is_err());
        assert_eq!(restored.requests[0], 1);
        restored.restart_allowance.as_mut().unwrap().deadline_ms = 1;
        assert!(restored.verify_restart_allowance().is_err());
    }

    async fn append_recovery_history_gap(state: &ServerState, session_id: SessionId) {
        for _ in 0..5_100 {
            state
                .sessions
                .append_event(
                    session_id,
                    SessionEventKind::SystemMessage {
                        text: "later ordinary conversation".into(),
                    },
                )
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn read_only_preparation_returns_exact_identity_without_changing_checkpoint() {
        use bcode_session_models::compaction_restart::{
            SessionCompactionRecoveryBlocker, SessionCompactionRecoveryPrepareRequest,
        };
        let (_, plan, transcript, chunks) = fixture("Preserve restrictions.");
        let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        let session = state
            .sessions
            .create_session(None, std::env::temp_dir())
            .await
            .unwrap();
        let session_id = session.id;
        drop(session);
        let trace = |phase, message| SessionTraceEvent {
            timestamp_ms: 1,
            turn_id: Some("owner".into()),
            phase,
            payload: SessionTracePayload::ContextCompaction {
                reason: "compaction_finished".into(),
                compacted: false,
                projected_context_chars: 0,
                message,
            },
        };
        state
            .sessions
            .append_trace_event(
                session_id,
                trace(SessionTracePhase::ContextCompactionStarted, None),
            )
            .await
            .unwrap();
        // Missing typed resolved policy is explicit incompatibility, not corruption or a
        // reason to make recovery inaccessible. No provider reservation has been made.
        let mut checkpoint = LocalCheckpoint::create(&plan, &transcript, &chunks).unwrap();
        checkpoint
            .save(&state, session_id, "incompatible-operation")
            .await
            .unwrap();
        let before = checkpoint.encode().unwrap();
        // The accepted checkpoint can predate thousands of later conversation events.
        // Recovery preparation must page from the checkpoint, not only the latest failure.
        state.sessions.append_trace_event(session_id, trace(
            SessionTracePhase::ContextCompactionFinished,
            Some("context compaction failed: invalid or incompatible compaction checkpoint; boundary unchanged".into()),
        )).await.unwrap();
        append_recovery_history_gap(&state, session_id).await;
        state
            .sessions
            .append_trace_event(
                session_id,
                trace(SessionTracePhase::ContextCompactionStarted, None),
            )
            .await
            .unwrap();
        let terminal = state.sessions.append_trace_event(session_id, trace(
            SessionTracePhase::ContextCompactionFinished,
            Some("context compaction failed: invalid or incompatible compaction checkpoint; boundary unchanged".into()),
        )).await.unwrap();
        let mut request = SessionCompactionRecoveryPrepareRequest {
            version: 1,
            session_id,
            operation_id: uuid::Uuid::new_v4(),
            expected_operation_key: "incompatible-operation".into(),
            expected_checkpoint_sequence: checkpoint.checkpoint_sequence,
            expected_terminal_sequence: terminal.sequence,
            budget: bcode_session_models::SessionCompactionRestartBudget {
                max_provider_requests: 8,
                timeout_ms: 60_000,
            },
        };
        let prepared = prepare_recovery(&state, &request).await.unwrap();
        assert_eq!(prepared.blocker, None);
        let candidate = prepared.restart.unwrap();
        assert_eq!(
            candidate.expected_terminal_sha256,
            bcode_session::compaction_event_identity(&terminal)
                .unwrap()
                .sha256
        );
        assert_eq!(candidate.operation_id, request.operation_id);
        let after = load(&state, session_id, "incompatible-operation")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.encode().unwrap(), before);
        assert_eq!(
            after.checkpoint_sequence,
            request.expected_checkpoint_sequence
        );
        request.expected_checkpoint_sequence -= 1;
        assert_eq!(
            prepare_recovery(&state, &request).await.unwrap().blocker,
            Some(SessionCompactionRecoveryBlocker::CheckpointChanged)
        );
        drop(state);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // One end-to-end retained-checkpoint lifecycle.
    async fn ordinary_compaction_resumes_settled_contract_failure_preserving_accepted_chunks() {
        Box::pin(retained_contract_failure_case(false)).await;
    }

    #[tokio::test]
    async fn feasible_compaction_dispatches_once_and_publishes_source_backed_scope() {
        let _guard = bcode_fake_provider_plugin::fake_compaction_test_guard().await;
        let root = tempfile::tempdir().unwrap();
        let sessions = bcode_session::SessionManager::default();
        let session = sessions
            .create_session(None, root.path().to_owned())
            .await
            .unwrap();
        let id = session.id;
        drop(session);
        for text in [
            "BCODE_FIXTURE_SCOPE {\"key\":\"scope\",\"statement\":\"Review only; do not modify files\"}",
            "continue",
        ] {
            sessions
                .append_event(
                    id,
                    SessionEventKind::UserMessage {
                        client_id: ClientId::new(),
                        text: text.into(),
                        admission: bcode_session_models::TurnAdmissionMetadata::default(),
                    },
                )
                .await
                .unwrap();
            sessions
                .append_event(
                    id,
                    SessionEventKind::AssistantMessage {
                        text: "Historical investigation detail. ".repeat(24),
                    },
                )
                .await
                .unwrap();
        }
        let mut state = crate::tests::test_server_state_with_fake_provider(sessions);
        state.startup_config.skills.enabled = false;
        state.startup_config.invariants.enabled = false;
        state.startup_config.system_prompt.mode = bcode_config::SystemPromptMode::Replace;
        state.startup_config.system_prompt.text = Some("Test compaction".into());
        state.startup_config.model.compaction.keep_recent_tokens = 1;
        state.startup_config.model.compaction.backend = bcode_config::CompactionBackend::Local;
        let mut selection = SessionModelSelection {
            provider_plugin_id: Some("bcode.fake-provider".into()),
            model_id: Some("fake-echo".into()),
            ..SessionModelSelection::default()
        };
        selection
            .provider_context
            .settings
            .insert("fake_compaction_fixture".into(), "scope-v1".into());
        Box::pin(compact_session_context_with_limit(
            &state,
            id,
            &selection,
            None,
            None,
            &TurnCancelState::default(),
            None,
        ))
        .await
        .unwrap();
        let history = state.sessions.session_history(id).await.unwrap();
        let requests: BTreeSet<_> = history
            .iter()
            .filter_map(|event| match &event.kind {
                SessionEventKind::ModelUsage { usage, .. } if usage.terminal => {
                    usage.request_id.as_ref()
                }
                _ => None,
            })
            .collect();
        assert_eq!(requests.len(), 1);
        let summary = history
            .iter()
            .find_map(|event| match &event.kind {
                SessionEventKind::ContextCompacted { summary, .. } => Some(summary),
                _ => None,
            })
            .unwrap();
        let evidence: CompactionEvidence = serde_json::from_str(summary).unwrap();
        assert!(
            evidence
                .current_scope
                .iter()
                .any(|scope| scope.statement == "Review only; do not modify files")
        );
        drop(state);
    }

    #[tokio::test]
    async fn capacity_default_uses_one_batch_for_feasible_complete_history() {
        let root = tempfile::tempdir().unwrap();
        let sessions = bcode_session::SessionManager::default();
        let session = sessions
            .create_session(None, root.path().to_owned())
            .await
            .unwrap();
        let id = session.id;
        drop(session);
        let state = crate::tests::test_server_state_with_fake_provider(sessions);
        let selection = SessionModelSelection {
            provider_plugin_id: Some("bcode.fake-provider".into()),
            model_id: Some("fake-echo".into()),
            ..SessionModelSelection::default()
        };
        let (_, _, transcript, _) = fixture("Keep this instruction. ".repeat(760).as_str());
        assert!(compaction_evidence_input(&transcript, None).unwrap().len() > 1);
        let (batches, identity) = synthesis_chunks(
            &state,
            id,
            &selection,
            &transcript,
            &TurnCancelState::default(),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(identity.starts_with("capacity-summary-v2"));
        assert_eq!(batches.len(), 1);
        drop(state);
    }

    #[tokio::test]
    async fn completed_expired_automatic_carry_publishes_without_new_provider_requests() {
        Box::pin(retained_contract_failure_case(true)).await;
    }

    #[allow(clippy::too_many_lines)]
    async fn retained_contract_failure_case(completed: bool) {
        let _guard = bcode_fake_provider_plugin::fake_compaction_test_guard().await;
        let root = tempfile::tempdir().unwrap();
        let sessions = bcode_session::SessionManager::default();
        let session = sessions
            .create_session(None, root.path().to_owned())
            .await
            .unwrap();
        let id = session.id;
        drop(session);
        for text in ["Inspect only", "continue"] {
            sessions
                .append_event(
                    id,
                    SessionEventKind::UserMessage {
                        client_id: ClientId::new(),
                        text: text.into(),
                        admission: bcode_session_models::TurnAdmissionMetadata::default(),
                    },
                )
                .await
                .unwrap();
            sessions
                .append_event(
                    id,
                    SessionEventKind::AssistantMessage {
                        text: "resolved historical detail; ".repeat(if text == "Inspect only" {
                            800
                        } else {
                            4
                        }),
                    },
                )
                .await
                .unwrap();
        }
        let mut state = crate::tests::test_server_state_with_fake_provider(sessions);
        state.startup_config.skills.enabled = false;
        state.startup_config.invariants.enabled = false;
        state.startup_config.system_prompt.mode = bcode_config::SystemPromptMode::Replace;
        state.startup_config.system_prompt.text = Some("Test compaction.".into());
        state.startup_config.model.compaction.keep_recent_tokens = 1;
        state.startup_config.model.compaction.backend = bcode_config::CompactionBackend::Local;
        let selection = SessionModelSelection {
            provider_plugin_id: Some("bcode.fake-provider".into()),
            model_id: Some("fake-basic".into()),
            ..Default::default()
        };
        let history = state
            .sessions
            .activated_conversation_snapshot(id)
            .await
            .unwrap()
            .events;
        let planning = CompactionPlanningPolicy::Proactive {
            keep_recent_tokens: 1,
        };
        let plan = structural_compaction_plan_with_policy(&history, 2000, planning).unwrap();
        let transcript = compaction_transcript_from_plan(&plan);
        let cancel = TurnCancelState::default();
        let budget = resolve_summary_budget(
            &state,
            id,
            &selection,
            &history,
            plan.compacted_through_sequence,
            1,
            &cancel,
        )
        .await
        .unwrap();
        let chunks = compaction_evidence_input(&transcript, Some(&cancel)).unwrap();
        let packing = "legacy-chars-v1".to_owned();
        assert!(chunks.len() > 1);
        let mut old = LocalCheckpoint::create_with_policy(
            &plan,
            &transcript,
            &chunks,
            synthesis_limits(&budget),
        )
        .unwrap();
        old.bind_resolved_policy(&budget).unwrap();
        old.packing_policy = budget_packing_identity(&packing, &budget).unwrap();
        let config = state.session_config(id).await;
        let key = format!(
            "{}:{:?}",
            operation_key(
                &history,
                &selection,
                config.model.tool_output.context_chars,
                planning
            )
            .unwrap(),
            config.model.compaction.backend
        );
        append_context_compaction_trace(
            &state,
            id,
            CompactionTraceKind::Started,
            "compaction_started",
            0,
            false,
            None,
        )
        .await;
        old.reserve_slot(0).unwrap();
        let accepted = CompactionEvidence {
            format: COMPACTION_SUMMARY_FORMAT.into(),
            current_scope: vec![CompactionScope {
                source_sequence: history[0].sequence,
                statement: "Inspect only".into(),
                supersedes: vec![],
            }],
            continuation: "Accepted first chunk evidence".into(),
        };
        old.accept(
            &accepted,
            &BTreeSet::from([history[0].sequence]),
            &BTreeSet::new(),
        );
        old.reserve_slot(1).unwrap();
        old.reserve_slot(1).unwrap();
        if completed {
            // Regression: automatic correction can follow a restart whose older deadline expired.
            // Dispatch uses the corrective grant; publication must not revive the older veto.
            old.restart_allowance = Some(RestartAllowance {
                max_requests: 64,
                deadline_ms: 1,
            });
            old.contract_continuation = Some(ContractContinuation {
                version: 1,
                contract: "section-schema-v1".into(),
                chunk: 1,
                requests: 1,
                baseline_requests: old.consumed_requests(),
                reserved: 1,
                pending: false,
                deadline_ms: 1,
                terminal_sequence: 1,
                terminal_sha256: "a".repeat(64),
            });
            for _ in 1..chunks.len() {
                old.accept(
                    &accepted,
                    &BTreeSet::from([history[0].sequence]),
                    &BTreeSet::new(),
                );
            }
        }
        old.save(&state, id, &key).await.unwrap();
        let original_sequence = old.checkpoint_sequence;
        let original_payload = old.encode().unwrap();
        for request in 0..if completed { 4 } else { 3 } {
            state
                .sessions
                .append_event(
                    id,
                    SessionEventKind::ModelUsage {
                        turn_id: format!("{id}-compact-{}-request-{request}", old.boundary),
                        usage: bcode_session_models::SessionTokenUsage {
                            request_id: Some(format!("request-{request}")),
                            terminal: true,
                            ..Default::default()
                        },
                    },
                )
                .await
                .unwrap();
        }
        let failure_message = if completed {
            "context compaction failed: provider error: compaction corrective deadline exhausted; boundary unchanged".to_owned()
        } else {
            format!(
                "context compaction failed: provider error: compaction synthesis rejected after bounded repair; chunk=2/{} reason=invalid_json; boundary unchanged",
                chunks.len()
            )
        };
        append_context_compaction_trace(
            &state,
            id,
            CompactionTraceKind::Finished,
            "compaction_finished",
            0,
            false,
            Some(failure_message),
        )
        .await;
        let result = Box::pin(compact_session_context_with_limit(
            &state, id, &selection, None, None, &cancel, None,
        ))
        .await
        .unwrap();
        assert_eq!(
            result.compacted_through_sequence,
            plan.compacted_through_sequence
        );
        if completed {
            let after = state
                .sessions
                .session_history_page(
                    id,
                    bcode_session_models::SessionHistoryQuery {
                        cursor: None,
                        limit: 512,
                        direction: bcode_session_models::SessionHistoryDirection::Backward,
                    },
                )
                .await
                .unwrap();
            assert!(
                !after
                    .events
                    .iter()
                    .any(|event| event.sequence > original_sequence + 4
                        && matches!(&event.kind, SessionEventKind::ModelUsage { .. }))
            );
        }
        let (_, active) = load_resolved(&state, id, &key).await.unwrap();
        assert!(active.is_none()); // Successful publication retires converted carry.
        let page = state
            .sessions
            .session_history_page(
                id,
                bcode_session_models::SessionHistoryQuery {
                    cursor: None,
                    limit: 512,
                    direction: bcode_session_models::SessionHistoryDirection::Backward,
                },
            )
            .await
            .unwrap();
        let resumed = page
            .events
            .iter()
            .rev()
            .filter_map(|event| {
                let envelope =
                    bcode_session_models::CompactionSynthesisCheckpoint::from_event(event)
                        .ok()
                        .flatten()?;
                envelope.host_payload
            })
            .find_map(|payload| LocalCheckpoint::decode(&payload).ok())
            .unwrap();
        assert_eq!(
            resumed
                .requests
                .iter()
                .map(|count| u32::from(*count))
                .sum::<u32>(),
            u32::from(!completed)
        );
        assert!(resumed.upgrade.is_some());
        assert!(resumed.packing_policy.starts_with("capacity-summary-v2"));
        assert_eq!(
            resumed.upgrade.as_ref().unwrap().spent_requests,
            if completed { 4 } else { 3 }
        );
        assert_eq!(resumed.next_chunk, resumed.manifest.len());
        assert!(
            resumed
                .evidence
                .current_scope
                .iter()
                .any(|scope| scope.statement == "Inspect only")
        );
        let archived = state
            .sessions
            .session_events_range(id, original_sequence, original_sequence, 1)
            .await
            .unwrap();
        drop(state);
        let envelope =
            bcode_session_models::CompactionSynthesisCheckpoint::from_event(&archived[0])
                .unwrap()
                .unwrap();
        assert_eq!(
            envelope.host_payload.as_deref(),
            Some(original_payload.as_str())
        );
    }

    #[tokio::test]
    async fn ordinary_compaction_automatically_replaces_known_incompatible_checkpoint() {
        Box::pin(automatic_policy_recovery_case(
            true,
            false,
            false,
            RecoverySource::Frozen,
        ))
        .await;
    }

    #[tokio::test]
    async fn ordinary_compaction_never_replays_unsettled_incompatible_checkpoint() {
        Box::pin(automatic_policy_recovery_case(
            false,
            false,
            false,
            RecoverySource::Frozen,
        ))
        .await;
    }

    #[tokio::test]
    async fn ordinary_compaction_does_not_renew_replacement_allowances() {
        Box::pin(automatic_policy_recovery_case(
            true,
            true,
            false,
            RecoverySource::Frozen,
        ))
        .await;
    }

    #[tokio::test]
    async fn manual_compact_abandons_missing_summary_response_through_runtime_admission() {
        Box::pin(automatic_policy_recovery_case(
            false,
            false,
            true,
            RecoverySource::Frozen,
        ))
        .await;
        Box::pin(automatic_policy_recovery_case(
            false,
            true,
            true,
            RecoverySource::Frozen,
        ))
        .await;
    }

    #[derive(Clone, Copy)]
    enum RecoverySource {
        Frozen,
        Expanded,
        ManualExpanded,
    }

    #[tokio::test]
    async fn automatic_policy_recovery_expands_retired_prefix_to_fit_continuation() {
        Box::pin(automatic_policy_recovery_case(
            true,
            false,
            false,
            RecoverySource::Expanded,
        ))
        .await;
        Box::pin(automatic_policy_recovery_case(
            false,
            false,
            false,
            RecoverySource::Expanded,
        ))
        .await;
    }

    #[tokio::test]
    async fn manual_compaction_recovers_history_appended_after_retired_checkpoint() {
        Box::pin(automatic_policy_recovery_case(
            true,
            false,
            false,
            RecoverySource::ManualExpanded,
        ))
        .await;
    }

    #[allow(clippy::too_many_lines)] // Full canonical fixture exercises ordinary admission through publication.
    async fn automatic_policy_recovery_case(
        settled: bool,
        replacement: bool,
        manual: bool,
        source: RecoverySource,
    ) {
        let expand = !matches!(source, RecoverySource::Frozen);
        let manual_expansion = matches!(source, RecoverySource::ManualExpanded);
        let _guard = bcode_fake_provider_plugin::fake_compaction_test_guard().await;
        let root = tempfile::tempdir().unwrap();
        let sessions = bcode_session::SessionManager::default();
        let session = sessions
            .create_session(None, root.path().to_owned())
            .await
            .unwrap();
        let id = session.id;
        drop(session);
        for text in if expand {
            vec!["inspect only", "additional historical work", "continue"]
        } else {
            vec!["inspect only", "continue"]
        } {
            sessions
                .append_event(
                    id,
                    SessionEventKind::UserMessage {
                        client_id: ClientId::new(),
                        text: text.into(),
                        admission: bcode_session_models::TurnAdmissionMetadata::default(),
                    },
                )
                .await
                .unwrap();
            sessions
                .append_event(
                    id,
                    SessionEventKind::AssistantMessage {
                        text: "resolved historical detail; ".repeat(
                            if manual && text == "inspect only" {
                                4000
                            } else {
                                160
                            },
                        ),
                    },
                )
                .await
                .unwrap();
        }
        let state = Arc::new({
            let mut configured = crate::tests::test_server_state_with_fake_provider(sessions);
            configured.startup_config.skills.enabled = false;
            configured.startup_config.invariants.enabled = false;
            configured.startup_config.system_prompt.mode = bcode_config::SystemPromptMode::Replace;
            configured.startup_config.system_prompt.text = Some("Test compaction.".into());
            configured
                .startup_config
                .model
                .compaction
                .keep_recent_tokens = 1;
            configured.startup_config.model.compaction.backend =
                bcode_config::CompactionBackend::Local;
            configured.selected_provider_plugin_id = Some("bcode.fake-provider".into());
            configured.selected_model_id = Some("fake-echo".into());
            configured
        });
        let selection = SessionModelSelection {
            provider_plugin_id: Some("bcode.fake-provider".into()),
            model_id: Some("fake-echo".into()),
            ..SessionModelSelection::default()
        };
        let history = state
            .sessions
            .activated_conversation_snapshot(id)
            .await
            .unwrap()
            .events;
        let planning = CompactionPlanningPolicy::Proactive {
            keep_recent_tokens: 1,
        };
        let plan = structural_compaction_plan_with_policy(
            &history,
            state.startup_config.model.tool_output.context_chars,
            planning,
        )
        .unwrap();
        let transcript = compaction_transcript_from_plan(&plan);
        let chunks = compaction_evidence_input(&transcript, None).unwrap();
        let key = format!(
            "{}:Local",
            operation_key(
                &history,
                &selection,
                state.startup_config.model.tool_output.context_chars,
                planning
            )
            .unwrap()
        );
        append_context_compaction_trace(
            &state,
            id,
            CompactionTraceKind::Started,
            "compaction_started",
            0,
            false,
            None,
        )
        .await;
        let old_plan = if expand {
            frozen_plan(
                &history,
                state.startup_config.model.tool_output.context_chars,
                history[1].sequence,
            )
            .unwrap()
        } else {
            frozen_plan(
                &history,
                state.startup_config.model.tool_output.context_chars,
                plan.compacted_through_sequence,
            )
            .unwrap()
        };
        let old_transcript = compaction_transcript_from_plan(&old_plan);
        let old_chunks = compaction_evidence_input(&old_transcript, None).unwrap();
        let mut old = LocalCheckpoint::create(&old_plan, &old_transcript, &old_chunks).unwrap();
        old.reserve_slot(0).unwrap();
        if manual {
            assert!(chunks.len() >= 8);
            let evidence = CompactionEvidence {
                format: COMPACTION_SUMMARY_FORMAT.into(),
                current_scope: vec![CompactionScope {
                    source_sequence: history[0].sequence,
                    statement: "inspect only".into(),
                    supersedes: Vec::new(),
                }],
                continuation: "Seven accepted batches. Continue inspection.".into(),
            };
            for index in 0..7 {
                if index > 0 {
                    old.reserve_slot(index).unwrap();
                }
                old.accept(
                    &evidence,
                    &BTreeSet::from([history[0].sequence]),
                    &BTreeSet::new(),
                );
            }
            old.reserve_slot(7).unwrap();
        }
        if replacement {
            if manual {
                let budget = resolve_summary_budget(
                    &state,
                    id,
                    &selection,
                    &history,
                    old.boundary,
                    1,
                    &TurnCancelState::default(),
                )
                .await
                .unwrap();
                old.synthesis_policy =
                    Some(CheckpointPolicy::from_policy(synthesis_limits(&budget)).unwrap());
                old.consolidation_requests = vec![0; chunks.len()];
                // Construct the historical fixture's frozen contract directly; binding APIs
                // correctly forbid changing policy after reservations have been consumed.
                old.resolved_policy = Some(serde_json::to_value(&budget).unwrap());
            }
            old.restart_allowance = Some(RestartAllowance {
                max_requests: 8,
                deadline_ms: if manual {
                    1
                } else {
                    current_unix_millis() + 60_000
                },
            });
        }
        old.save(&state, id, &key).await.unwrap();
        for index in 0..if manual { 8 } else { 1 } {
            state
                .sessions
                .append_event(
                    id,
                    SessionEventKind::ModelUsage {
                        turn_id: format!("{id}-compact-{}-old-request-{index}", old.boundary),
                        usage: bcode_session_models::SessionTokenUsage {
                            request_id: Some(format!("old-request-{index}")),
                            terminal: if manual { index < 7 } else { settled },
                            ..Default::default()
                        },
                    },
                )
                .await
                .unwrap();
        }
        // Reproduce a long-lived checkpoint: new compactable history arrives after it was saved.
        if expand {
            for text in [
                "additional work after checkpoint",
                "latest retained exchange",
            ] {
                state
                    .sessions
                    .append_event(
                        id,
                        SessionEventKind::UserMessage {
                            client_id: ClientId::new(),
                            text: text.into(),
                            admission: bcode_session_models::TurnAdmissionMetadata::default(),
                        },
                    )
                    .await
                    .unwrap();
                state
                    .sessions
                    .append_event(
                        id,
                        SessionEventKind::AssistantMessage {
                            text: "resolved new historical detail; ".repeat(160),
                        },
                    )
                    .await
                    .unwrap();
            }
        }
        let expanded_history = state
            .sessions
            .activated_conversation_snapshot(id)
            .await
            .unwrap()
            .events;
        let plan = structural_compaction_plan_with_policy(
            &expanded_history,
            state.startup_config.model.tool_output.context_chars,
            planning,
        )
        .unwrap();
        let transcript = compaction_transcript_from_plan(&plan);
        if expand {
            assert!(plan.compacted_through_sequence > old.checkpoint_sequence);
        }
        let old_payload = old.encode().unwrap();
        append_context_compaction_trace(&state, id, CompactionTraceKind::Finished,
            "compaction_finished", 0, false,
            Some("context compaction failed: invalid or incompatible compaction checkpoint; boundary unchanged".into())).await;
        let result = if manual {
            let completion = crate::session_operations::compact(&state, ClientId::new(), id)
                .await
                .unwrap();
            let result = crate::session_operations::complete_compaction(completion).await;
            assert!(
                result.is_ok(),
                "manual compact failed: {}",
                result
                    .as_ref()
                    .err()
                    .map_or(String::new(), ToString::to_string)
            );
            let pointer = state
                .sessions
                .session_compaction_checkpoint(id, &restart_pointer_key(&key))
                .await
                .unwrap()
                .unwrap();
            let envelope =
                bcode_session_models::CompactionSynthesisCheckpoint::from_event(&pointer)
                    .unwrap()
                    .unwrap();
            let pointer: RestartPointer =
                serde_json::from_str(envelope.host_payload.as_deref().unwrap()).unwrap();
            assert_eq!(pointer.request.version, 2);
            assert_eq!(pointer.abandoned_responses, vec!["old-request-7"]);
            let converted = LocalCheckpoint::decode(&pointer.initial_replacement_payload).unwrap();
            assert_eq!(converted.consumed_requests(), 8);
            assert!(
                converted
                    .evidence
                    .current_scope
                    .iter()
                    .any(|scope| scope.statement == "inspect only")
            );
            let history = state.sessions.session_history(id).await.unwrap();
            assert!(history.iter().any(|event| matches!(&event.kind, SessionEventKind::ContextCompacted { compacted_through_sequence, .. } if *compacted_through_sequence == plan.compacted_through_sequence)));
            result
                .map(|_| CompactionCompletion {
                    message: "manual compaction completed".into(),
                    compacted_through_sequence: plan.compacted_through_sequence,
                })
                .map_err(|error| CompactionError::Provider(error.to_string()))
        } else if manual_expansion {
            let completion = crate::session_operations::compact(&state, ClientId::new(), id)
                .await
                .unwrap();
            crate::session_operations::complete_compaction(completion)
                .await
                .map(|_| CompactionCompletion {
                    message: "manual expanded compaction completed".into(),
                    compacted_through_sequence: plan.compacted_through_sequence,
                })
                .map_err(|error| CompactionError::Provider(error.to_string()))
        } else {
            Box::pin(compact_session_context_with_limit(
                &state,
                id,
                &selection,
                None,
                None,
                &TurnCancelState::default(),
                expand.then_some(CompactionProgressRequirement {
                    minimum_reclaimable_tokens: transcript.estimated_reclaimable_tokens / 2,
                    previous_compacted_through_sequence: Some(old.boundary),
                }),
            ))
            .await
        };
        if manual || (settled && !replacement) {
            assert_eq!(
                result.unwrap().compacted_through_sequence,
                plan.compacted_through_sequence
            );
            let canonical = state.sessions.session_history(id).await.unwrap();
            assert!(canonical.iter().any(|event| matches!(&event.kind,
                SessionEventKind::ContextCompacted { compacted_through_sequence, .. }
                    if *compacted_through_sequence == plan.compacted_through_sequence)));
        } else {
            assert!(matches!(
                result,
                Err(CompactionError::RecoveryRequired(_) | CompactionError::RecoveryBlocked(_))
            ));
            if !replacement {
                let recorded = state.sessions.session_history(id).await.unwrap();
                assert!(recorded.iter().any(|event| matches!(&event.kind,
                    SessionEventKind::TraceEvent { trace }
                    if trace.phase == SessionTracePhase::ContextCompactionFinished
                        && matches!(&trace.payload, SessionTracePayload::ContextCompaction {
                            reason, compacted: false, message: Some(message), ..
                        } if reason == "compaction_recovery_failed" && message.starts_with("context compaction failed:")))));
            }
        }
        let archived = state
            .sessions
            .session_events_range(id, old.checkpoint_sequence, old.checkpoint_sequence, 1)
            .await
            .unwrap();
        let envelope =
            bcode_session_models::CompactionSynthesisCheckpoint::from_event(&archived[0])
                .unwrap()
                .unwrap();
        assert_eq!(envelope.host_payload.as_deref(), Some(old_payload.as_str()));
        assert_eq!(
            state
                .sessions
                .session_compaction_checkpoint(id, &restart_pointer_key(&key))
                .await
                .unwrap()
                .is_some(),
            manual || (settled && !replacement)
        );
        drop(state);
    }

    #[tokio::test]
    async fn archived_checkpoint_read_uses_selected_sequence_after_retirement() {
        let (_, plan, transcript, chunks) = fixture("Do not modify files.");
        let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        let session = state
            .sessions
            .create_session(None, std::env::temp_dir())
            .await
            .unwrap();
        let session_id = session.id;
        drop(session);
        let mut checkpoint = LocalCheckpoint::create(&plan, &transcript, &chunks).unwrap();
        checkpoint.reserve_slot(0).unwrap();
        checkpoint.reserve_slot(0).unwrap();
        checkpoint
            .save(&state, session_id, "archived-operation")
            .await
            .unwrap();
        let sequence = checkpoint.checkpoint_sequence;
        let payload = checkpoint.encode().unwrap();
        state
            .sessions
            .append_compaction_checkpoint(session_id, "archived-operation", None)
            .await
            .unwrap();
        let mut request = bcode_session_models::SessionCompactionRestartRequest {
            version: 1,
            session_id,
            operation_id: uuid::Uuid::new_v4(),
            expected_operation_key: "archived-operation".into(),
            expected_checkpoint_sequence: sequence,
            expected_terminal_sequence: sequence + 2,
            expected_terminal_sha256: "ab".repeat(32),
            expected_tail_marker: None,
            expected_prior_marker: None,
            budget: bcode_session_models::SessionCompactionRestartBudget {
                max_provider_requests: 4,
                timeout_ms: 60_000,
            },
        };
        assert!(
            load(&state, session_id, "archived-operation")
                .await
                .unwrap()
                .is_none()
        );
        let mut restored = load_archived_checkpoint(&state, &request).await.unwrap();
        assert_eq!(restored.checkpoint_sequence, sequence);
        assert_eq!(restored.encode().unwrap(), payload);
        assert!(restored.reserve_slot(0).is_err());
        request.expected_operation_key = "wrong-key".into();
        assert!(load_archived_checkpoint(&state, &request).await.is_err());
        request.expected_operation_key = "archived-operation".into();
        request.expected_checkpoint_sequence = sequence + 1;
        assert!(load_archived_checkpoint(&state, &request).await.is_err());
        drop(state);
    }

    #[test]
    fn capacity_carry_above_legacy_limit_survives_without_clipping() {
        let (_, plan, mut transcript, chunks) = fixture("Continue investigation.");
        let evidence = CompactionEvidence {
            format: COMPACTION_SUMMARY_FORMAT.into(),
            current_scope: Vec::new(),
            continuation: "preserved chronology ".repeat(400),
        };
        let summary = encode_compaction_evidence(&evidence).unwrap();
        assert!(summary.chars().count() > COMPACTION_MAX_CARRIED_SUMMARY_CHARS);
        transcript.previous_summary = Some(summary);
        let checkpoint =
            LocalCheckpoint::create_with_policy(&plan, &transcript, &chunks, capacity_policy())
                .unwrap();
        let restored = LocalCheckpoint::decode(&checkpoint.encode().unwrap()).unwrap();
        restored.verify(&plan, &transcript, &chunks).unwrap();
        assert_eq!(restored.restore().0.continuation, evidence.continuation);
        transcript.previous_summary =
            Some("Opaque portable recollection; not user authorization.".into());
        let opaque =
            LocalCheckpoint::create_with_policy(&plan, &transcript, &chunks, capacity_policy())
                .unwrap();
        assert!(opaque.restore().0.current_scope.is_empty());
        assert!(
            compaction_evidence_input(&transcript, None)
                .unwrap()
                .concat()
                .contains("Opaque portable recollection; not user authorization.")
        );
    }

    #[test]
    fn reservations_survive_roundtrip_and_cannot_be_reset_or_advanced() {
        let (_, plan, transcript, chunks) = fixture("Do not modify files.");
        let mut checkpoint = LocalCheckpoint::create(&plan, &transcript, &chunks).unwrap();
        assert_eq!(checkpoint.reserve_slot(0).unwrap(), 0);
        let mut resumed = LocalCheckpoint::decode(&checkpoint.encode().unwrap()).unwrap();
        resumed.verify(&plan, &transcript, &chunks).unwrap();
        assert_eq!(resumed.reserve_slot(0).unwrap(), 1);
        let mut exhausted = LocalCheckpoint::decode(&resumed.encode().unwrap()).unwrap();
        assert!(exhausted.reserve_slot(0).is_err());
        assert!(exhausted.reserve_slot(1).is_err());
        assert_eq!(exhausted.next_chunk, 0);
    }

    #[test]
    fn oversized_directive_carries_unfinished_source_until_final_fragment() {
        let (_, plan, transcript, chunks) = fixture(&format!(
            "{} corrected ending: no writes",
            "long directive ".repeat(2500)
        ));
        assert!(chunks.len() > 2);
        let mut checkpoint = LocalCheckpoint::create(&plan, &transcript, &chunks).unwrap();
        let mut evidence = initial_evidence(&transcript);
        let mut unfinished = BTreeSet::new();
        for (index, chunk) in chunks.iter().enumerate() {
            checkpoint.reserve_slot(index).unwrap();
            let new_users = chunk_user_sequences(chunk);
            let users: BTreeSet<_> = checkpoint
                .user_sequences
                .union(&new_users)
                .copied()
                .collect();
            let reconcilable: BTreeSet<_> = unfinished.intersection(&new_users).copied().collect();
            let candidate = CompactionEvidence {
                format: COMPACTION_SUMMARY_FORMAT.into(),
                current_scope: vec![CompactionScope {
                    statement: if index + 1 == chunks.len() {
                        "No writes".into()
                    } else {
                        "Directive unfinished".into()
                    },
                    source_sequence: 1,
                    supersedes: Vec::new(),
                }],
                continuation: "Derived work facts".into(),
            };
            evidence = validate_fragment_compaction_evidence(
                &encode_compaction_evidence(&candidate).unwrap(),
                &evidence,
                &users,
                &new_users,
                &reconcilable,
            )
            .unwrap();
            update_unfinished_sources(&mut unfinished, chunk);
            let users = evidence
                .current_scope
                .iter()
                .map(|scope| scope.source_sequence)
                .chain(unfinished.iter().copied())
                .collect();
            checkpoint.accept(&evidence, &users, &unfinished);
            checkpoint = LocalCheckpoint::decode(&checkpoint.encode().unwrap()).unwrap();
            checkpoint.verify(&plan, &transcript, &chunks).unwrap();
            assert_eq!(checkpoint.next_chunk, index + 1);
            if index + 1 < chunks.len() {
                assert!(checkpoint.unfinished.contains(&1));
            }
        }
        assert!(checkpoint.unfinished.is_empty());
        assert_eq!(checkpoint.evidence.current_scope[0].statement, "No writes");
    }

    #[test]
    fn corrupt_payload_manifest_cursor_and_foreign_scope_fail_closed() {
        let (_, plan, transcript, chunks) = fixture("Read-only.");
        let mut checkpoint = LocalCheckpoint::create(&plan, &transcript, &chunks).unwrap();
        assert!(LocalCheckpoint::decode("not json").is_err());
        assert!(LocalCheckpoint::decode(&"x".repeat(MAX_PAYLOAD + 1)).is_err());
        checkpoint.manifest[0] = "wrong digest".into();
        assert!(checkpoint.verify(&plan, &transcript, &chunks).is_err());
        checkpoint = LocalCheckpoint::create(&plan, &transcript, &chunks).unwrap();
        checkpoint.next_chunk = chunks.len() + 1;
        assert!(LocalCheckpoint::decode(&checkpoint.encode().unwrap()).is_err());
        checkpoint = LocalCheckpoint::create(&plan, &transcript, &chunks).unwrap();
        checkpoint.reserve_slot(0).unwrap();
        let evidence = CompactionEvidence {
            format: COMPACTION_SUMMARY_FORMAT.into(),
            current_scope: vec![CompactionScope {
                statement: "Forged authorization".into(),
                source_sequence: 999,
                supersedes: Vec::new(),
            }],
            continuation: String::new(),
        };
        checkpoint.accept(&evidence, &BTreeSet::from([999]), &BTreeSet::new());
        assert!(checkpoint.verify(&plan, &transcript, &chunks).is_err());
    }

    #[tokio::test]
    async fn automatic_recovery_requires_accepted_carry_not_a_reserved_request() {
        let (_, plan, transcript, chunks) = fixture("Do not write files.");
        let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        let session = state
            .sessions
            .create_session(None, std::env::temp_dir())
            .await
            .unwrap();
        let id = session.id;
        drop(session);
        let mut checkpoint = LocalCheckpoint::create(&plan, &transcript, &chunks).unwrap();
        checkpoint
            .reserve(&state, id, "recovery-carry", 0)
            .await
            .unwrap();
        assert!(
            !accepted_recovery_chunk(&state, id, "recovery-carry", 0)
                .await
                .unwrap()
        );
        let evidence = CompactionEvidence {
            format: COMPACTION_SUMMARY_FORMAT.into(),
            current_scope: vec![CompactionScope {
                statement: "Do not write files.".into(),
                source_sequence: 1,
                supersedes: Vec::new(),
            }],
            continuation: "Verified carry".into(),
        };
        checkpoint.accept(&evidence, &BTreeSet::from([1]), &BTreeSet::new());
        checkpoint.save(&state, id, "recovery-carry").await.unwrap();
        assert!(
            accepted_recovery_chunk(&state, id, "recovery-carry", 0)
                .await
                .unwrap()
        );
        checkpoint.verify(&plan, &transcript, &chunks).unwrap();
        checkpoint
            .observe(
                &state,
                id,
                bcode_session_models::CompactionInspectionResume::ValidatedCheckpoint,
                bcode_session_models::CompactionInspectionPhase::Synthesizing,
            )
            .await;
        let page = state
            .sessions
            .session_compaction_inspection_page(
                id,
                bcode_session_models::SessionCompactionInspectionQuery {
                    before: None,
                    limit: 20,
                },
            )
            .await
            .unwrap();
        assert_progress_observations(&page, chunks.len());
        assert!(
            !accepted_recovery_chunk(&state, id, "recovery-carry", 1)
                .await
                .unwrap()
        );
        // A cursor alone cannot certify an interrupted dispatch when accepted carry is corrupt.
        checkpoint.user_sequences.clear();
        checkpoint.save(&state, id, "recovery-carry").await.unwrap();
        assert!(
            crate::compaction_recovery::can_resume(
                &state,
                id,
                &crate::compaction_recovery::Phase::Local {
                    key: "recovery-carry".into(),
                    chunk: 0,
                },
            )
            .await
            .is_err()
        );
        drop(state);
    }

    fn assert_progress_observations(
        page: &bcode_session_models::SessionCompactionInspectionPage,
        chunks: usize,
    ) {
        let observations: Vec<_> = page
            .entries
            .iter()
            .filter_map(|entry| entry.progress.as_ref())
            .collect();
        assert_eq!(observations.len(), 2);
        for observation in &observations {
            observation.validate().expect("valid emitted counts");
        }
        let reserved = observations[0];
        assert_eq!(reserved.chunks.unwrap().done, 0);
        assert_eq!(
            reserved
                .budgets
                .as_ref()
                .unwrap()
                .provider_requests
                .unwrap()
                .done,
            1
        );
        let progress = observations
            .iter()
            .rev()
            .copied()
            .find(|observation| {
                observation.resume
                    == bcode_session_models::CompactionInspectionResume::ValidatedCheckpoint
                    && observation.phase
                        == bcode_session_models::CompactionInspectionPhase::Synthesizing
            })
            .expect("latest typed validated observation");
        assert_eq!(progress.chunks.unwrap().done, 1);
        assert_eq!(
            progress.chunks.unwrap().total,
            u64::try_from(chunks).unwrap()
        );
        assert_eq!(
            progress.resume,
            bcode_session_models::CompactionInspectionResume::ValidatedCheckpoint
        );
        let budgets = progress.budgets.as_ref().unwrap();
        assert_eq!(budgets.provider_requests.unwrap().done, 1);
        assert!(budgets.source_events.is_none());
        assert!(budgets.decoded_bytes.is_none());
    }

    #[tokio::test]
    async fn cancellation_after_reservation_retains_allowance_and_no_accepted_coverage() {
        let (_, plan, transcript, chunks) = fixture("Do not write files.");
        let mut checkpoint = LocalCheckpoint::create(&plan, &transcript, &chunks).unwrap();
        checkpoint.reserve_slot(0).unwrap();
        let cancel = TurnCancelState::default();
        cancel.cancel().await;
        assert!(matches!(
            compaction_evidence_input(&transcript, Some(&cancel)),
            Err(CompactionError::Cancelled)
        ));
        let mut restored = LocalCheckpoint::decode(&checkpoint.encode().unwrap()).unwrap();
        restored.verify(&plan, &transcript, &chunks).unwrap();
        assert_eq!(restored.next_chunk, 0);
        assert_eq!(restored.reserve_slot(0).unwrap(), 1);
        assert!(restored.reserve_slot(0).is_err());
    }

    #[test]
    fn accepted_carry_requires_continuation_and_exact_scope_bookkeeping_on_decode() {
        let (_, plan, transcript, chunks) = fixture("Do not modify files.");
        let mut checkpoint = LocalCheckpoint::create(&plan, &transcript, &chunks).unwrap();
        checkpoint.reserve_slot(0).unwrap();
        let evidence = CompactionEvidence {
            format: COMPACTION_SUMMARY_FORMAT.into(),
            current_scope: vec![CompactionScope {
                statement: "Do not modify files.".into(),
                source_sequence: 1,
                supersedes: Vec::new(),
            }],
            continuation: "Inspection remains unfinished.".into(),
        };
        checkpoint.accept(&evidence, &BTreeSet::from([1]), &BTreeSet::new());
        let valid = checkpoint.encode().unwrap();
        LocalCheckpoint::decode(&valid).unwrap();
        for (field, value) in [
            ("user_sequences", serde_json::json!([])),
            ("user_sequences", serde_json::json!([1, 999])),
            ("unfinished", serde_json::json!([999])),
            ("requests", serde_json::json!(vec![0; chunks.len()])),
        ] {
            let mut corrupt: serde_json::Value = serde_json::from_str(&valid).unwrap();
            corrupt[field] = value;
            assert!(LocalCheckpoint::decode(&corrupt.to_string()).is_err());
        }
        for field in ["continuation", "current_scope"] {
            let mut corrupt: serde_json::Value = serde_json::from_str(&valid).unwrap();
            if field == "continuation" {
                corrupt["evidence"][field] = serde_json::json!(" \n\t");
            } else {
                corrupt["evidence"][field][0]["statement"] = serde_json::json!(" ");
            }
            assert!(LocalCheckpoint::decode(&corrupt.to_string()).is_err());
        }
    }

    #[tokio::test]
    async fn exhausted_next_chunk_preserves_durable_accepted_and_unfinished_carry() {
        let (_, plan, transcript, chunks) = fixture(&"unfinished directive ".repeat(2500));
        assert!(chunks.len() > 2);
        let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        let session = state
            .sessions
            .create_session(None, std::env::temp_dir())
            .await
            .unwrap();
        let id = session.id;
        drop(session);
        let key = "exhausted-with-carry";
        let mut checkpoint = LocalCheckpoint::create(&plan, &transcript, &chunks).unwrap();
        checkpoint.reserve(&state, id, key, 0).await.unwrap();
        let evidence = CompactionEvidence {
            format: COMPACTION_SUMMARY_FORMAT.into(),
            current_scope: vec![CompactionScope {
                statement: "Directive remains unfinished.".into(),
                source_sequence: 1,
                supersedes: Vec::new(),
            }],
            continuation: "Await the remaining chronological fragments.".into(),
        };
        let mut unfinished = BTreeSet::new();
        update_unfinished_sources(&mut unfinished, &chunks[0]);
        assert_eq!(unfinished, BTreeSet::from([1]));
        checkpoint.accept(&evidence, &BTreeSet::from([1]), &unfinished);
        checkpoint.save(&state, id, key).await.unwrap();
        assert_eq!(checkpoint.reserve(&state, id, key, 1).await.unwrap(), 0);
        assert_eq!(checkpoint.reserve(&state, id, key, 1).await.unwrap(), 1);
        let cancel = TurnCancelState::default();
        cancel.cancel().await;
        assert!(matches!(
            compaction_evidence_input(&transcript, Some(&cancel)),
            Err(CompactionError::Cancelled)
        ));
        let mut restored = load(&state, id, key).await.unwrap().unwrap();
        restored.verify(&plan, &transcript, &chunks).unwrap();
        let durable_sequence = restored.checkpoint_sequence;
        let durable_payload = restored.encode().unwrap();
        assert!(restored.reserve(&state, id, key, 1).await.is_err());
        assert!(restored.reserve(&state, id, key, 2).await.is_err());
        assert_eq!(restored.encode().unwrap(), durable_payload);
        let durable = load(&state, id, key).await.unwrap().unwrap();
        assert_eq!(durable.checkpoint_sequence, durable_sequence);
        assert_eq!(durable.encode().unwrap(), durable_payload);
        assert_eq!(durable.restore().1, BTreeSet::from([1]));
        assert_eq!(durable.restore().2, unfinished);
        assert_eq!(
            encode_compaction_evidence(&durable.restore().0).unwrap(),
            encode_compaction_evidence(&evidence).unwrap()
        );
        assert!(accepted_recovery_chunk(&state, id, key, 0).await.unwrap());
        assert!(!accepted_recovery_chunk(&state, id, key, 1).await.unwrap());
        let mut legacy: serde_json::Value = serde_json::from_str(&durable_payload).unwrap();
        legacy.as_object_mut().unwrap().remove("packing_policy");
        let mut legacy = LocalCheckpoint::decode(&legacy.to_string()).unwrap();
        legacy.verify(&plan, &transcript, &chunks).unwrap();
        assert_eq!(legacy.packing_policy, legacy_packing_policy());
        assert!(legacy.reserve_slot(1).is_err());
        assert_eq!(legacy.requests, durable.requests);
        assert_eq!(legacy.unfinished, durable.unfinished);
        assert_eq!(legacy.next_chunk, durable.next_chunk);
        drop(state);
    }

    #[test]
    fn compatibility_and_prior_marker_change_namespace_but_followup_does_not() {
        let (mut history, _, _, _) = fixture("Scope.");
        let selection = SessionModelSelection::default();
        let key = operation_key(
            &history,
            &selection,
            2000,
            CompactionPlanningPolicy::OverflowRecovery,
        )
        .unwrap();
        assert_ne!(
            key,
            operation_key(
                &history,
                &selection,
                1000,
                CompactionPlanningPolicy::OverflowRecovery
            )
            .unwrap()
        );
        assert_ne!(
            key,
            operation_key(
                &history,
                &selection,
                2000,
                CompactionPlanningPolicy::Proactive {
                    keep_recent_tokens: 100
                }
            )
            .unwrap()
        );
        let mut changed = selection.clone();
        changed.model_id = Some("different".into());
        assert_ne!(
            key,
            operation_key(
                &history,
                &changed,
                2000,
                CompactionPlanningPolicy::OverflowRecovery
            )
            .unwrap()
        );
        history[1].kind = SessionEventKind::ContextCompacted {
            summary: "Prior summary".into(),
            compacted_through_sequence: 1,
        };
        assert_ne!(
            key,
            operation_key(
                &history,
                &selection,
                2000,
                CompactionPlanningPolicy::OverflowRecovery
            )
            .unwrap()
        );
    }
}
