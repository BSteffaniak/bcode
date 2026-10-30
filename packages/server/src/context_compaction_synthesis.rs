//! Separate contracts for untrusted full summaries and host-preserving synthesis updates.

#[cfg(test)]
use super::COMPACTION_MAX_CARRIED_SUMMARY_CHARS;

use super::{BTreeSet, COMPACTION_SUMMARY_FORMAT, CompactionEvidence, CompactionScope};

#[path = "context_compaction_sections.rs"]
pub(super) mod sections;
pub(super) use sections::{
    SECTION_UPDATE_FORMAT, consolidate_response, consolidation_prompt, section_update_prompt,
};

/// Serialization/allocation guards derived from the resolved budget's independent resource
/// bounds. These are neither preferred summary targets nor token-capacity acceptance criteria.
/// The coordinator must validate the complete retained request with canonical semantic estimates
/// after merging; passing these guards alone never authorizes publication.
#[derive(Debug, Clone, Copy)]
pub(super) struct SynthesisPolicy {
    pub carry_chars: usize,
    pub wire_bytes: usize,
    pub section_chars: usize,
}

#[cfg(test)]
impl SynthesisPolicy {
    pub(super) const LEGACY: Self = Self {
        carry_chars: COMPACTION_MAX_CARRIED_SUMMARY_CHARS,
        wire_bytes: COMPACTION_MAX_CARRIED_SUMMARY_CHARS * 4,
        section_chars: COMPACTION_MAX_CARRIED_SUMMARY_CHARS,
    };
}

pub(super) const UPDATE_FORMAT: &str = "bcode.compaction.scope-update.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Rejection {
    WireBudget,
    InvalidJson,
    Schema {
        field: &'static str,
        rule: &'static str,
    },
    InvalidShape,
    UnknownField,
    JsonSyntax {
        line: usize,
        column: usize,
    },
    UnsupportedFormat,
    EmptyContinuation,
    CarryBudget,
    InvalidSource,
    InvalidSupersession,
    AmbiguousGroupSupersession,
    UnauthorizedScopeChange,
    MissingScope,
    InvalidReconciliation,
    IncompleteOutput,
    PreviousAttemptUnavailable,
}

impl Rejection {
    pub(super) fn guidance(self) -> String {
        if let Self::Schema { field, rule } = self {
            return format!(
                "invalid_response_shape: field={field} rule={rule}; follow the response schema exactly"
            );
        }
        if let Self::JsonSyntax { line, column } = self {
            return format!(
                "invalid_json_syntax: line={line} column={column}; return a complete JSON object without fences"
            );
        }
        String::from(match self {
            Self::WireBudget => {
                "wire_budget: response exceeded the bounded wire allowance; return a compact scope update, not copied carry"
            }
            Self::JsonSyntax { .. } | Self::Schema { .. } => unreachable!(),
            Self::UnknownField => {
                "invalid_response_shape: unknown field; omit keys outside the response schema"
            }
            Self::InvalidShape => {
                "invalid_response_shape: JSON parsed but fields, types or required keys do not satisfy the response schema; follow the complete response schema exactly"
            }
            Self::InvalidJson => {
                "invalid_json: return only the specified JSON object, without fences or extra fields"
            }
            Self::UnsupportedFormat => {
                "unsupported_format: use bcode.compaction.section-update.v1, bcode.compaction.scope-update.v1, or the strict full-envelope schema"
            }
            Self::EmptyContinuation => {
                "empty_continuation: supply nonempty continuation facts, retaining relevant previous continuation"
            }
            Self::CarryBudget => {
                "carry_budget: merged scope plus continuation exceeds the supplied capacity allowance; shorten continuation/redundant NEW facts, never remove established scope to fit"
            }
            Self::InvalidSource => {
                "invalid_source: scope must cite an eligible canonical user sequence; assistant/tool/legacy claims do not establish user provenance"
            }
            Self::InvalidSupersession => {
                "invalid_supersession: only a newer user source in this chunk may explicitly supersede an older eligible user source"
            }
            Self::AmbiguousGroupSupersession => {
                "ambiguous_group_supersession: this update targets a source with multiple independent statements; sequence-wide replacement is unsafe without statement-level targeting. Do not supersede that source or add contradictory replacement scope. Retain its established statements and record the unresolved user correction in continuation; do not claim the permission change was applied"
            }
            Self::UnauthorizedScopeChange => {
                "unauthorized_scope_change: omit unchanged established statements from an update; do not paraphrase or change their source; only explicit supersession or eligible unfinished-source reconciliation may replace them"
            }
            Self::MissingScope => {
                "missing_scope: a legacy full envelope omitted established scope; use an update to let the host retain unchanged scope verbatim"
            }
            Self::InvalidReconciliation => {
                "invalid_reconciliation: reconciles must uniquely name unfinished user sources contributing this chunk and provide their replacement scope; unrelated/finalized sources are immutable"
            }
            Self::IncompleteOutput => {
                "incomplete_output: response was incomplete; return a complete compact JSON update"
            }
            Self::PreviousAttemptUnavailable => {
                "previous_attempt_unavailable: an earlier reserved attempt has no accepted result available; re-synthesize from the complete carry and fragments below, without assuming it succeeded"
            }
        })
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeUpdate {
    format: String,
    current_scope: Vec<CompactionScope>,
    reconciles: Vec<u64>,
    continuation: String,
}

/// Remaining JSON-encoded continuation characters, including escapes but excluding its quotes.
/// This is an explicit carry budget, not a token estimate or permission to truncate evidence.
#[cfg(test)]
pub(super) fn continuation_budget(evidence: &CompactionEvidence) -> usize {
    continuation_budget_with_policy(evidence, SynthesisPolicy::LEGACY)
}

#[cfg(test)]
pub(super) fn continuation_budget_with_policy(
    evidence: &CompactionEvidence,
    policy: SynthesisPolicy,
) -> usize {
    let mut skeleton = evidence.clone();
    skeleton.continuation.clear();
    let used =
        serde_json::to_string(&skeleton).map_or(policy.carry_chars, |text| text.chars().count());
    policy.carry_chars.saturating_sub(used)
}

fn decode(summary: &str, policy: SynthesisPolicy) -> Result<serde_json::Value, Rejection> {
    if summary.len() > policy.wire_bytes {
        return Err(Rejection::WireBudget);
    }
    serde_json::from_str(summary).map_err(|error| super::response::decode_error(&error))
}

/// Strict full-envelope validation: missing or paraphrased carried scope is never repaired here.
#[cfg(test)]
pub(super) fn validate_full(
    summary: &str,
    prior: &CompactionEvidence,
    user_sequences: &BTreeSet<u64>,
    new_user_sequences: &BTreeSet<u64>,
    reconcilable: &BTreeSet<u64>,
) -> Result<CompactionEvidence, Rejection> {
    validate_full_with_policy(
        summary,
        prior,
        user_sequences,
        new_user_sequences,
        reconcilable,
        SynthesisPolicy::LEGACY,
    )
}

pub(super) fn validate_full_with_policy(
    summary: &str,
    prior: &CompactionEvidence,
    user_sequences: &BTreeSet<u64>,
    new_user_sequences: &BTreeSet<u64>,
    reconcilable: &BTreeSet<u64>,
    policy: SynthesisPolicy,
) -> Result<CompactionEvidence, Rejection> {
    let evidence = serde_json::from_value(decode(summary, policy)?)
        .map_err(|error| super::response::decode_error(&error))?;
    validate_evidence(
        evidence,
        prior,
        user_sequences,
        new_user_sequences,
        reconcilable,
        policy,
    )
}

fn validate_evidence(
    evidence: CompactionEvidence,
    prior: &CompactionEvidence,
    user_sequences: &BTreeSet<u64>,
    new_user_sequences: &BTreeSet<u64>,
    reconcilable: &BTreeSet<u64>,
    policy: SynthesisPolicy,
) -> Result<CompactionEvidence, Rejection> {
    if !reconcilable.is_subset(new_user_sequences) {
        return Err(Rejection::InvalidReconciliation);
    }
    if evidence.format != COMPACTION_SUMMARY_FORMAT {
        return Err(Rejection::UnsupportedFormat);
    }
    if evidence.continuation.trim().is_empty() {
        return Err(Rejection::EmptyContinuation);
    }
    sections::validate_continuation(&evidence.continuation, policy)?;
    if serde_json::to_string(&evidence)
        .map_err(|_| Rejection::InvalidJson)?
        .chars()
        .count()
        > policy.carry_chars
    {
        return Err(Rejection::CarryBudget);
    }
    for scope in &evidence.current_scope {
        if scope.statement.trim().is_empty()
            || !user_sequences.contains(&scope.source_sequence)
            || (!new_user_sequences.contains(&scope.source_sequence)
                && !prior.current_scope.iter().any(|old| {
                    old.source_sequence == scope.source_sequence && old.statement == scope.statement
                }))
        {
            return Err(Rejection::InvalidSource);
        }
        if scope.supersedes.iter().any(|source| {
            *source >= scope.source_sequence
                || !user_sequences.contains(source)
                || !new_user_sequences.contains(&scope.source_sequence)
        }) {
            return Err(Rejection::InvalidSupersession);
        }
    }
    for scope in &prior.current_scope {
        if reconcilable.contains(&scope.source_sequence) {
            // Reconciliation permits revising this provisional source, not silently retiring
            // it when its final fragment arrives. Require replacement scope under the same
            // canonical source even for legacy full-envelope responses.
            if !evidence
                .current_scope
                .iter()
                .any(|next| next.source_sequence == scope.source_sequence)
            {
                return Err(Rejection::InvalidReconciliation);
            }
            continue;
        }
        if !evidence.current_scope.iter().any(|next| {
            (next.source_sequence == scope.source_sequence && next.statement == scope.statement)
                || (new_user_sequences.contains(&next.source_sequence)
                    && next.supersedes.contains(&scope.source_sequence))
        }) {
            return Err(Rejection::MissingScope);
        }
    }
    for scope in &evidence.current_scope {
        if !reconcilable.contains(&scope.source_sequence)
            && prior
                .current_scope
                .iter()
                .any(|old| old.source_sequence == scope.source_sequence)
            && !prior.current_scope.iter().any(|old| {
                old.source_sequence == scope.source_sequence && old.statement == scope.statement
            })
        {
            return Err(Rejection::UnauthorizedScopeChange);
        }
    }
    Ok(evidence)
}

/// Only this explicit update contract permits omitted unchanged scope. Legacy envelopes remain
/// strict. The host copies protected statements, applies declared replacements, then validates the
/// entire result with the same provenance, supersession, preservation and size rules.
#[cfg(test)]
pub(super) fn merge_response(
    summary: &str,
    prior: &CompactionEvidence,
    user_sequences: &BTreeSet<u64>,
    new_user_sequences: &BTreeSet<u64>,
    reconcilable: &BTreeSet<u64>,
) -> Result<CompactionEvidence, Rejection> {
    merge_response_with_policy(
        summary,
        prior,
        user_sequences,
        new_user_sequences,
        reconcilable,
        SynthesisPolicy::LEGACY,
    )
}

pub(super) fn merge_response_with_policy(
    summary: &str,
    prior: &CompactionEvidence,
    user_sequences: &BTreeSet<u64>,
    new_user_sequences: &BTreeSet<u64>,
    reconcilable: &BTreeSet<u64>,
    policy: SynthesisPolicy,
) -> Result<CompactionEvidence, Rejection> {
    let result = merge_response_inner(
        summary,
        prior,
        user_sequences,
        new_user_sequences,
        reconcilable,
        policy,
    );
    if let Some(context) = bcode_xray::CaptureContext::current() {
        context.emit(
            "context",
            "compaction_synthesis_merge",
            serde_json::json!({
                "accepted": result.is_ok(),
                "new_sources": new_user_sequences.len(),
                "reconcilable_sources": reconcilable.len(),
            }),
            false,
        );
        if let Ok(evidence) = &result {
            crate::xray_context::sensitive(&context, "compaction_synthesis_candidate", evidence);
        }
    }
    result
}

fn merge_response_inner(
    summary: &str,
    prior: &CompactionEvidence,
    user_sequences: &BTreeSet<u64>,
    new_user_sequences: &BTreeSet<u64>,
    reconcilable: &BTreeSet<u64>,
    policy: SynthesisPolicy,
) -> Result<CompactionEvidence, Rejection> {
    let value = decode(summary, policy)?;
    if value
        .get("format")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|format| {
            format == SECTION_UPDATE_FORMAT || format == sections::SECTION_REPLACE_FORMAT
        })
    {
        return sections::merge(
            value,
            prior,
            user_sequences,
            new_user_sequences,
            reconcilable,
            policy,
        );
    }
    if value.get("format").and_then(serde_json::Value::as_str) != Some(UPDATE_FORMAT) {
        return validate_full_with_policy(
            summary,
            prior,
            user_sequences,
            new_user_sequences,
            reconcilable,
            policy,
        );
    }
    let update: ScopeUpdate =
        serde_json::from_value(value).map_err(|error| super::response::decode_error(&error))?;
    if update.format != UPDATE_FORMAT {
        return Err(Rejection::UnsupportedFormat);
    }
    let reconciles = update.reconciles.iter().copied().collect::<BTreeSet<_>>();
    if reconciles.len() != update.reconciles.len()
        || !reconciles.is_subset(reconcilable)
        || !reconciles.is_subset(new_user_sequences)
        || reconciles.iter().any(|source| {
            !update
                .current_scope
                .iter()
                .any(|scope| scope.source_sequence == *source)
        })
    {
        return Err(Rejection::InvalidReconciliation);
    }
    let superseded = update
        .current_scope
        .iter()
        .flat_map(|scope| scope.supersedes.iter().copied())
        .collect::<BTreeSet<_>>();
    // Sequence-only targeting cannot distinguish independent restrictions from one user
    // message. Reject before filtering any carry; legacy full envelopes keep their existing
    // explicit group-replacement semantics, but updates must not silently erase siblings.
    for source in &superseded {
        let statements = prior
            .current_scope
            .iter()
            .filter(|scope| scope.source_sequence == *source)
            .map(|scope| scope.statement.as_str())
            .collect::<BTreeSet<_>>();
        if statements.len() > 1 {
            return Err(Rejection::AmbiguousGroupSupersession);
        }
    }
    let mut merged = CompactionEvidence {
        format: COMPACTION_SUMMARY_FORMAT.into(),
        current_scope: prior
            .current_scope
            .iter()
            .filter(|scope| {
                !reconciles.contains(&scope.source_sequence)
                    && !superseded.contains(&scope.source_sequence)
            })
            .cloned()
            .collect(),
        continuation: update.continuation,
    };
    for scope in update.current_scope {
        // Exact echoes are harmless compatibility, but may not introduce new supersession claims.
        if merged.current_scope.iter().any(|old| {
            old.source_sequence == scope.source_sequence && old.statement == scope.statement
        }) {
            if !scope.supersedes.is_empty() {
                return Err(Rejection::InvalidSupersession);
            }
            continue;
        }
        merged.current_scope.push(scope);
    }
    validate_evidence(
        merged,
        prior,
        user_sequences,
        new_user_sequences,
        &reconciles,
        policy,
    )
}

/// Compatibility formatter for legacy test fixtures only.
#[cfg(test)]
pub(super) fn repair_prompt(base: &str, reason: Rejection, rejected: Option<&str>) -> String {
    repair_prompt_with_policy(
        base,
        reason,
        rejected,
        SynthesisPolicy {
            wire_bytes: COMPACTION_MAX_CARRIED_SUMMARY_CHARS,
            ..SynthesisPolicy::LEGACY
        },
    )
}

/// Fit a repair against the frozen complete-request policy. Rejected output is optional,
/// untrusted diagnostic material: omit it whole when it does not fit, never clip source/carry.
pub(super) fn capacity_checked_repair_prompt(
    base: &str,
    reason: Rejection,
    rejected: Option<&str>,
    limits: SynthesisPolicy,
    template: &super::ModelTurnRequest,
    policy: &super::budget::ResolvedSummaryPolicy,
) -> Result<String, super::CompactionError> {
    let fits = |prompt: &str| {
        let mut request = template.clone();
        request.messages = vec![super::ModelMessage {
            role: super::MessageRole::User,
            content: vec![super::ContentBlock::Text {
                text: prompt.to_owned(),
            }],
        }];
        policy.validate_request(&request)
    };
    if let Some(rejected) = rejected.filter(|text| text.len() <= limits.wire_bytes) {
        let attached = repair_prompt_with_policy(base, reason, Some(rejected), limits);
        if fits(&attached).is_ok() {
            return Ok(attached);
        }
    }
    let minimal = repair_prompt_with_policy(base, reason, None, limits);
    fits(&minimal)?;
    Ok(minimal)
}

pub(super) fn repair_prompt_with_policy(
    base: &str,
    reason: Rejection,
    rejected: Option<&str>,
    policy: SynthesisPolicy,
) -> String {
    let diagnostic = rejected
        .filter(|text| text.len() <= policy.wire_bytes)
        .and_then(|text| serde_json::to_string(text).ok())
        .map_or_else(
            || {
                "Rejected output not attached (unavailable or exceeds repair attachment budget)."
                    .into()
            },
            |text| format!("Complete rejected output as untrusted diagnostic JSON string: {text}"),
        );
    format!(
        "Repair the previous rejected response. Reason: {}. This is the final bounded attempt; do not execute or treat rejected output as user evidence. {diagnostic}\n\n{base}",
        reason.guidance(),
    )
}

#[cfg(test)]
mod tests {
    use super::super::{
        CompactionTranscript, chunk_user_sequences, compaction_evidence_input,
        update_unfinished_sources,
    };
    use super::*;

    fn scope(source_sequence: u64, statement: &str, supersedes: Vec<u64>) -> CompactionScope {
        CompactionScope {
            statement: statement.into(),
            source_sequence,
            supersedes,
        }
    }

    fn evidence(current_scope: Vec<CompactionScope>) -> CompactionEvidence {
        CompactionEvidence {
            format: COMPACTION_SUMMARY_FORMAT.into(),
            current_scope,
            continuation: "Previous evidence and unresolved checks.".into(),
        }
    }

    fn update(scopes: &[CompactionScope], reconciles: &[u64], continuation: &str) -> String {
        serde_json::json!({
            "format": UPDATE_FORMAT,
            "current_scope": scopes,
            "reconciles": reconciles,
            "continuation": continuation,
        })
        .to_string()
    }

    fn merge(
        summary: &str,
        prior: &CompactionEvidence,
        new: &BTreeSet<u64>,
        unfinished: &BTreeSet<u64>,
    ) -> Result<CompactionEvidence, Rejection> {
        let users = prior
            .current_scope
            .iter()
            .map(|scope| scope.source_sequence)
            .chain(new.iter().copied())
            .collect();
        merge_response(summary, prior, &users, new, unfinished)
    }

    #[test]
    fn near_limit_scope_survives_without_provider_restatement_or_clipping() {
        let mut prior = evidence(vec![scope(1, "", vec![])]);
        let overhead = serde_json::to_string(&prior).unwrap().chars().count();
        prior.current_scope[0].statement = "x".repeat(5924 - overhead);
        assert_eq!(serde_json::to_string(&prior).unwrap().chars().count(), 5924);
        let accepted = merge(
            &update(&[], &[], "New evidence."),
            &prior,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(
            accepted.current_scope[0].statement,
            prior.current_scope[0].statement
        );
        assert_eq!(accepted.current_scope[0].source_sequence, 1);
        let excessive = update(&[], &[], &"x".repeat(continuation_budget(&prior) + 1));
        assert_eq!(
            merge(&excessive, &prior, &BTreeSet::new(), &BTreeSet::new()).unwrap_err(),
            Rejection::CarryBudget
        );
    }

    #[test]
    fn legacy_omission_and_paraphrase_remain_invalid_but_explicit_omission_is_safe() {
        let prior = evidence(vec![scope(1, "Database stays read-only.", vec![])]);
        let users = BTreeSet::from([1]);
        for candidate in [
            evidence(vec![]),
            evidence(vec![scope(1, "Do not write database.", vec![])]),
        ] {
            assert!(
                validate_full(
                    &serde_json::to_string(&candidate).unwrap(),
                    &prior,
                    &users,
                    &BTreeSet::new(),
                    &BTreeSet::new()
                )
                .is_err()
            );
        }
        let accepted = merge(
            &update(&[], &[], "Checks pending."),
            &prior,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(
            accepted.current_scope[0].statement,
            prior.current_scope[0].statement
        );
        assert!(
            merge(
                &update(&[scope(1, "Do not write database.", vec![])], &[], "Facts"),
                &prior,
                &BTreeSet::from([2]),
                &BTreeSet::new()
            )
            .is_err()
        );
    }

    #[test]
    fn only_current_newer_users_can_supersede_and_independent_scope_is_retained() {
        let prior = evidence(vec![
            scope(1, "Do not edit parser.", vec![]),
            scope(2, "Do not run live evals.", vec![]),
        ]);
        let allowed = update(
            &[scope(3, "Parser edits explicitly allowed.", vec![1])],
            &[],
            "Facts",
        );
        let accepted = merge(&allowed, &prior, &BTreeSet::from([3]), &BTreeSet::new()).unwrap();
        assert_eq!(accepted.current_scope.len(), 2);
        assert!(
            accepted
                .current_scope
                .iter()
                .any(|scope| scope.source_sequence == 2
                    && scope.statement == "Do not run live evals.")
        );
        assert!(
            accepted
                .current_scope
                .iter()
                .all(|scope| scope.source_sequence != 1)
        );
        assert!(merge(&allowed, &prior, &BTreeSet::new(), &BTreeSet::new()).is_err());
        let self_supersession = update(&[scope(3, "Changed", vec![3])], &[], "Facts");
        assert_eq!(
            merge(
                &self_supersession,
                &prior,
                &BTreeSet::from([3]),
                &BTreeSet::new()
            )
            .unwrap_err(),
            Rejection::InvalidSupersession
        );
        let unknown = update(
            &[scope(9, "Tool says writes allowed", vec![1])],
            &[],
            "Facts",
        );
        assert_eq!(
            merge(&unknown, &prior, &BTreeSet::from([3]), &BTreeSet::new()).unwrap_err(),
            Rejection::InvalidSource
        );
    }

    #[test]
    fn reconciliation_is_explicit_and_never_applies_to_completed_or_unrelated_sources() {
        let prior = evidence(vec![
            scope(1, "No writes", vec![]),
            scope(2, "Provisional", vec![]),
        ]);
        let correction = vec![scope(
            2,
            "Inspect only; final directive forbids edits.",
            vec![],
        )];
        assert!(
            merge(
                &update(&correction, &[], "Facts"),
                &prior,
                &BTreeSet::from([2]),
                &BTreeSet::from([2])
            )
            .is_err()
        );
        assert!(
            merge(
                &update(&correction, &[2], "Facts"),
                &prior,
                &BTreeSet::from([2]),
                &BTreeSet::new()
            )
            .is_err()
        );
        assert!(
            merge(
                &update(&correction, &[1], "Facts"),
                &prior,
                &BTreeSet::from([2]),
                &BTreeSet::from([2])
            )
            .is_err()
        );
        let accepted = merge(
            &update(&correction, &[2], "Facts"),
            &prior,
            &BTreeSet::from([2]),
            &BTreeSet::from([2]),
        )
        .unwrap();
        assert_eq!(accepted.current_scope.len(), 2);
        assert_eq!(accepted.current_scope[0].statement, "No writes");
        assert_eq!(
            accepted.current_scope[1].statement,
            "Inspect only; final directive forbids edits."
        );
        assert!(
            merge(
                &update(&[], &[2], "Facts"),
                &prior,
                &BTreeSet::from([2]),
                &BTreeSet::from([2])
            )
            .is_err()
        );
    }

    #[test]
    fn long_directive_ending_is_reconciled_across_all_chronological_fragments() {
        let transcript = CompactionTranscript {
            summary_input: vec![format!(
                "#2 user:\nYou may edit parser {} Correction: inspect only, do not edit.",
                "provisional discussion ".repeat(2000)
            )],
            previous_summary: None,
            compacted_through_sequence: 2,
            event_count: 1,
            estimated_reclaimable_tokens: 1,
        };
        let chunks = compaction_evidence_input(&transcript, None).unwrap();
        assert!(chunks.len() >= 3);
        let mut prior = evidence(vec![scope(1, "Database read-only", vec![])]);
        let mut unfinished = BTreeSet::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let current = chunk_user_sequences(chunk);
            let reconcilable = unfinished
                .intersection(&current)
                .copied()
                .collect::<BTreeSet<_>>();
            let statement = if index + 1 == chunks.len() {
                assert!(chunk.contains("do not edit"));
                "Inspect only, do not edit."
            } else {
                "Provisional: parser edits may be allowed."
            };
            prior = merge(
                &update(
                    &[scope(2, statement, vec![])],
                    &reconcilable.iter().copied().collect::<Vec<_>>(),
                    "Prior evidence retained.",
                ),
                &prior,
                &current,
                &reconcilable,
            )
            .unwrap();
            update_unfinished_sources(&mut unfinished, chunk);
            assert_eq!(prior.current_scope[0].statement, "Database read-only");
        }
        assert!(unfinished.is_empty());
        assert_eq!(
            prior.current_scope[1].statement,
            "Inspect only, do not edit."
        );
        assert!(
            merge(
                &update(&[scope(2, "Edits allowed", vec![])], &[2], "Facts"),
                &prior,
                &BTreeSet::from([3]),
                &BTreeSet::new()
            )
            .is_err()
        );
    }

    #[test]
    fn legacy_final_fragment_requires_replacement_for_every_reconcilable_prior_source() {
        let prior = evidence(vec![
            scope(1, "Database read-only", vec![]),
            scope(2, "Provisional parser permission", vec![]),
            scope(3, "Provisional testing permission", vec![]),
        ]);
        let current = BTreeSet::from([2, 3]);
        let users = BTreeSet::from([1, 2, 3]);
        let replacement = evidence(vec![
            scope(1, "Database read-only", vec![]),
            scope(2, "Inspect parser only; do not edit.", vec![]),
            scope(3, "Offline tests only.", vec![]),
        ]);
        let accepted = validate_full(
            &serde_json::to_string(&replacement).unwrap(),
            &prior,
            &users,
            &current,
            &current,
        )
        .unwrap();
        assert_eq!(
            accepted.current_scope[1].statement,
            "Inspect parser only; do not edit."
        );
        for omitted in [2, 3] {
            let mut incomplete = replacement.clone();
            incomplete
                .current_scope
                .retain(|scope| scope.source_sequence != omitted);
            let response = serde_json::to_string(&incomplete).unwrap();
            assert_eq!(
                validate_full(&response, &prior, &users, &current, &current).unwrap_err(),
                Rejection::InvalidReconciliation
            );
            // The production legacy fallback must enforce the same rule.
            assert_eq!(
                merge_response(&response, &prior, &users, &current, &current).unwrap_err(),
                Rejection::InvalidReconciliation
            );
        }
    }

    #[test]
    fn same_source_independent_constraints_survive_updates_and_explicit_group_replacement() {
        let prior = evidence(vec![
            scope(1, "Parser read-only", vec![]),
            scope(1, "Never run live evals", vec![]),
            scope(2, "Database read-only", vec![]),
        ]);
        let unchanged = merge(
            &update(&[scope(1, "Parser read-only", vec![])], &[], "Facts"),
            &prior,
            &BTreeSet::from([3]),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(unchanged.current_scope.len(), 3);
        let partial_legacy = evidence(vec![
            scope(1, "Parser read-only", vec![]),
            scope(2, "Database read-only", vec![]),
        ]);
        assert_eq!(
            merge(
                &serde_json::to_string(&partial_legacy).unwrap(),
                &prior,
                &BTreeSet::from([3]),
                &BTreeSet::new()
            )
            .unwrap_err(),
            Rejection::MissingScope
        );
        // Updates reject sequence-wide replacement of independent constraints even when the
        // provider offers to restate a sibling: that restatement is not precise targeting.
        let replacement = vec![
            scope(3, "Parser edits explicitly allowed", vec![1]),
            scope(3, "Never run live evals", vec![]),
        ];
        for changes in [vec![replacement[0].clone()], replacement.clone()] {
            assert_eq!(
                merge(
                    &update(&changes, &[], "Facts"),
                    &prior,
                    &BTreeSet::from([3]),
                    &BTreeSet::new(),
                )
                .unwrap_err(),
                Rejection::AmbiguousGroupSupersession
            );
        }
        let repaired = merge(
            &update(&[], &[], "Unresolved user correction: parser edits requested; permission change not applied."),
            &prior, &BTreeSet::from([3]), &BTreeSet::new(),
        ).unwrap();
        assert_eq!(repaired.current_scope.len(), 3);
        assert_eq!(repaired.current_scope[0].statement, "Parser read-only");
        assert_eq!(repaired.current_scope[1].statement, "Never run live evals");
        let diagnostic = repair_prompt(
            "complete evidence",
            Rejection::AmbiguousGroupSupersession,
            None,
        );
        assert!(diagnostic.contains("statement-level targeting"));
        assert!(diagnostic.contains("do not claim the permission change was applied"));
        // Legacy full envelopes retain their baseline explicit source-group contract.
        let response = serde_json::to_string(&evidence(vec![
            scope(2, "Database read-only", vec![]),
            replacement[0].clone(),
            replacement[1].clone(),
        ]))
        .unwrap();
        let accepted = merge(&response, &prior, &BTreeSet::from([3]), &BTreeSet::new()).unwrap();
        assert_eq!(accepted.current_scope.len(), 3);
        assert!(
            accepted
                .current_scope
                .iter()
                .all(|scope| scope.source_sequence != 1)
        );
        assert!(
            accepted
                .current_scope
                .iter()
                .any(|scope| scope.statement == "Never run live evals")
        );
        assert!(
            accepted
                .current_scope
                .iter()
                .any(|scope| scope.statement == "Database read-only")
        );
    }

    #[test]
    fn prior_summary_carry_and_strict_legacy_provider_responses_remain_compatible() {
        let prior = evidence(vec![scope(7, "Keep independent restrictions.", vec![])]);
        let transcript = CompactionTranscript {
            previous_summary: Some(serde_json::to_string(&prior).unwrap()),
            summary_input: vec![],
            compacted_through_sequence: 7,
            event_count: 1,
            estimated_reclaimable_tokens: 1,
        };
        let carried = super::super::checkpoint::initial_evidence(&transcript);
        assert_eq!(
            carried.current_scope[0].statement,
            prior.current_scope[0].statement
        );
        assert_eq!(carried.continuation, prior.continuation);
        let accepted = merge(
            &serde_json::to_string(&carried).unwrap(),
            &prior,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(accepted.continuation, prior.continuation);
        assert!(
            merge(
                &format!("```json\n{}\n```", update(&[], &[], "Facts")),
                &prior,
                &BTreeSet::new(),
                &BTreeSet::new(),
            )
            .is_err()
        );
    }

    #[test]
    fn continuation_budget_counts_serialized_escapes_and_overflow_is_rejected() {
        let prior = evidence(vec![scope(1, "Constraint", vec![])]);
        let budget = continuation_budget(&prior);
        let at_limit = merge(
            &update(&[], &[], &"a".repeat(budget)),
            &prior,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_string(&at_limit).unwrap().chars().count(),
            6000
        );
        // Include a non-whitespace fact so this exercises escaped size overflow rather than
        // the independent empty-continuation guard (which intentionally runs first).
        let escaped_overflow = format!("Fact{}", "\n".repeat(budget / 2 + 1));
        assert_eq!(
            merge(
                &update(&[], &[], &escaped_overflow),
                &prior,
                &BTreeSet::new(),
                &BTreeSet::new()
            )
            .unwrap_err(),
            Rejection::CarryBudget
        );
        assert_eq!(
            merge(
                &update(&[], &[], "\n\t "),
                &prior,
                &BTreeSet::new(),
                &BTreeSet::new()
            )
            .unwrap_err(),
            Rejection::EmptyContinuation
        );
    }

    #[test]
    fn repair_includes_specific_reason_and_whole_untrusted_response_with_bounded_attachment() {
        let base = "Complete source fragments and prior carry";
        let rejected = "{malformed response}";
        let prompt = repair_prompt(base, Rejection::InvalidJson, Some(rejected));
        assert!(prompt.contains(&Rejection::InvalidJson.guidance()));
        assert!(prompt.contains(&serde_json::to_string(rejected).unwrap()));
        assert!(prompt.ends_with(base));
        let large = "not canonical evidence".repeat(1000);
        let prompt = repair_prompt(base, Rejection::WireBudget, Some(&large));
        assert!(!prompt.contains(&large));
        assert!(prompt.contains("not attached"));
        assert!(prompt.ends_with(base));
    }
}
