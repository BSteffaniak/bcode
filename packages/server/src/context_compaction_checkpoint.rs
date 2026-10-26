//! Local synthesis checkpoints: bounded operational evidence, never model instructions.

use super::*;
use sha2::Sha256;

const CHECKPOINT_FORMAT: &str = "bcode.local-compaction.v1";
const MAX_PAYLOAD: usize = bcode_session_models::MAX_COMPACTION_CHECKPOINT_PAYLOAD_BYTES;

fn legacy_packing_policy() -> String {
    "legacy-chars-v1".into()
}

fn invalid_checkpoint() -> CompactionError {
    CompactionError::Provider(
        "invalid or incompatible compaction checkpoint; boundary unchanged".into(),
    )
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
pub(super) struct LocalCheckpoint {
    format: String,
    #[serde(default = "legacy_packing_policy")]
    pub(super) packing_policy: String,
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

    pub(super) fn decode(payload: &str) -> Result<Self, CompactionError> {
        if payload.len() > MAX_PAYLOAD {
            return Err(invalid_checkpoint());
        }
        let checkpoint: Self = serde_json::from_str(payload).map_err(|_| invalid_checkpoint())?;
        if checkpoint.format != CHECKPOINT_FORMAT
            || checkpoint.manifest.is_empty()
            || checkpoint.manifest.len() > COMPACTION_MAX_CHUNKS
            || checkpoint.requests.len() != checkpoint.manifest.len()
            || checkpoint.next_chunk > checkpoint.manifest.len()
            || checkpoint.requests.iter().any(|count| *count > 2)
            || checkpoint.requests[..checkpoint.next_chunk].contains(&0)
            || checkpoint
                .requests
                .iter()
                .skip(checkpoint.next_chunk.saturating_add(1))
                .any(|count| *count != 0)
            || checkpoint.evidence.format != COMPACTION_SUMMARY_FORMAT
            || encode_compaction_evidence(&checkpoint.evidence)?
                .chars()
                .count()
                > COMPACTION_MAX_CARRIED_SUMMARY_CHARS
        {
            return Err(invalid_checkpoint());
        }
        Ok(checkpoint)
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
        let mut unfinished = BTreeSet::new();
        let mut known = prior_scope_sequences(transcript.previous_summary.as_deref());
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
                    || digest(&self.evidence)? != digest(&initial_evidence(transcript))?))
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
    pub(super) async fn reserve(
        &mut self,
        state: &ServerState,
        session_id: SessionId,
        key: &str,
        index: usize,
    ) -> Result<u8, CompactionError> {
        let attempt = self.reserve_slot(index)?;
        self.save(state, session_id, key).await?;
        Ok(attempt)
    }

    fn reserve_slot(&mut self, index: usize) -> Result<u8, CompactionError> {
        if index != self.next_chunk {
            return Err(invalid_checkpoint());
        }
        let count = self
            .requests
            .get_mut(index)
            .ok_or_else(invalid_checkpoint)?;
        if *count >= 2 {
            return Err(CompactionError::Provider(
                "compaction request/repair budget exhausted; boundary unchanged".into(),
            ));
        }
        let attempt = *count;
        *count += 1;
        Ok(attempt)
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
