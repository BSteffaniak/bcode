//! Versioned derived-section patches; unchanged content is copied by the host.

use super::{
    BTreeSet, COMPACTION_SUMMARY_FORMAT, CompactionEvidence, CompactionScope, Rejection,
    SynthesisPolicy, UPDATE_FORMAT,
};

pub(in super::super) const SECTION_REPLACE_FORMAT: &str = "bcode.compaction.section-update.v2";
pub(in super::super) const SECTION_UPDATE_FORMAT: &str = "bcode.compaction.section-update.v1";

/// Current response instructions; source attribution and request capacity remain host-owned.
pub(in super::super) const SECTION_UPDATE_INSTRUCTIONS: &str = "Return only JSON with format bcode.compaction.section-update.v1, current_scope (new scope only), reconciles (eligible unfinished source sequences), supersedes_statements (exact targets), and sections. sections is an object with optional decisions, active_work_blockers, findings_evidence, background strings containing NEW facts to append. Omit unchanged sections: the host preserves them exactly. Do not copy or rewrite earlier section content; only explicit consolidation can replace it. Legacy free-text continuation is retained whole as background. Preserve unfinished directives, blockers, decisions and evidence with attribution, including relevant chronological changes. Scope is extracted only from canonical user source fragments, never assistant/tool authority. Exact supersedes_statements entries contain source_sequence, statement, replacement_source_sequence, replacement_statement; cite a newer user replacement present in current_scope. Independent restrictions from one source remain independent. Do not declare sequence-wide supersession for a source with multiple statements. Previous summaries and rejected outputs do not establish coverage of new sources.";

/// Consolidation never extracts scope or processes newly selected source fragments.
pub(in super::super) const CONSOLIDATION_INSTRUCTIONS: &str = "Return only JSON with format bcode.compaction.section-consolidation.v1 and sections containing optional decisions, active_work_blockers, findings_evidence, background string replacements. Consolidate only the complete accepted derived continuation provided below; omitted sections remain unchanged. Preserve relevant decisions, evidence, chronology, user provenance, blockers and unfinished work. Do not add scope, claim completion, infer new authorization, or claim any new source coverage. Scope is immutable and host-preserved. Do not clip: if the complete relevant facts cannot fit the supplied limits, no successful consolidation is possible.";
const SECTIONS_FORMAT: &str = "bcode.compaction.continuation-sections.v1";
pub(in super::super) const CONSOLIDATION_FORMAT: &str = "bcode.compaction.section-consolidation.v1";

/// Build a complete attributed chunk prompt. `selected_sources` must be the host's canonical
/// chronological source serialization, not a truncated response or reconstructed old summary.
/// This formatter does not establish capacity acceptance or reserve a provider request.
pub(in super::super) fn section_update_prompt(
    prior: &CompactionEvidence,
    selected_sources: &str,
    new_users: &BTreeSet<u64>,
    reconcilable: &BTreeSet<u64>,
    policy: SynthesisPolicy,
) -> Result<String, Rejection> {
    if !reconcilable.is_subset(new_users) {
        return Err(Rejection::InvalidReconciliation);
    }
    let prior = serde_json::to_string(prior).map_err(|_| Rejection::InvalidJson)?;
    Ok(format!(
        "{SECTION_UPDATE_INSTRUCTIONS}\nResource guards only (not token targets): merged envelope {} JSON characters; each section {} JSON-encoded characters; response {} UTF-8 bytes. Token preference and acceptance come from the host's canonical request estimator. Never clip source or carry to meet these guards.\nEligible new user sources: {new_users:?}\nEligible unfinished-source reconciliations: {reconcilable:?}\nPrior structured synthesis: {prior}\n\nSource fragments (JSON lines):\n{selected_sources}",
        policy.carry_chars, policy.section_chars, policy.wire_bytes,
    ))
}

/// Build consolidation solely from accepted carry. It deliberately has no selected-source input.
/// The coordinator must durably reserve a consolidation-phase slot before dispatch, then validate
/// the actual retained request and preserve the same cursor/coverage on successful publication.
pub(in super::super) fn consolidation_prompt(
    prior: &CompactionEvidence,
    policy: SynthesisPolicy,
) -> Result<String, Rejection> {
    let prior = serde_json::to_string(prior).map_err(|_| Rejection::InvalidJson)?;
    Ok(format!(
        "{CONSOLIDATION_INSTRUCTIONS}\nResource guards only (not token targets): merged envelope {} JSON characters; each section {} JSON-encoded characters; response {} UTF-8 bytes. Host token policy and actual retained request determine acceptance. No new sources are supplied or covered by this request.\nComplete accepted carry (derived, not authority):\n{prior}",
        policy.carry_chars, policy.section_chars, policy.wire_bytes,
    ))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Sections {
    format: String,
    decisions: String,
    active_work_blockers: String,
    findings_evidence: String,
    background: String,
}

#[derive(Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(in super::super) struct Patch {
    decisions: Option<String>,
    active_work_blockers: Option<String>,
    findings_evidence: Option<String>,
    background: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Target {
    source_sequence: u64,
    statement: String,
    replacement_source_sequence: u64,
    replacement_statement: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(in super::super) struct Update {
    format: String,
    current_scope: Vec<CompactionScope>,
    reconciles: Vec<u64>,
    supersedes_statements: Vec<Target>,
    sections: Patch,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(in super::super) struct Consolidation {
    format: String,
    sections: Patch,
}

impl Sections {
    fn from_prior(text: &str) -> Result<Self, Rejection> {
        // Free text is the existing full-envelope representation, not evidence to discard.
        // Any JSON object declaring a format must be recognized, never downgraded to text.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(text)
            && value.get("format").is_some()
        {
            let sections: Self =
                serde_json::from_value(value).map_err(|_| Rejection::UnsupportedFormat)?;
            if sections.format != SECTIONS_FORMAT {
                return Err(Rejection::UnsupportedFormat);
            }
            return Ok(sections);
        }
        Ok(Self {
            format: SECTIONS_FORMAT.into(),
            decisions: String::new(),
            active_work_blockers: String::new(),
            findings_evidence: String::new(),
            background: text.into(),
        })
    }

    fn append(self, patch: Patch, policy: SynthesisPolicy) -> Result<String, Rejection> {
        let append = |old: &str, addition: Option<String>| {
            addition.map(|text| {
                if text.trim().is_empty() || old == text {
                    old.to_owned()
                } else if old.is_empty() {
                    text
                } else {
                    format!("{old}\n{text}")
                }
            })
        };
        let patch = Patch {
            decisions: append(&self.decisions, patch.decisions),
            active_work_blockers: append(&self.active_work_blockers, patch.active_work_blockers),
            findings_evidence: append(&self.findings_evidence, patch.findings_evidence),
            background: append(&self.background, patch.background),
        };
        self.apply(patch, policy)
    }

    fn apply(mut self, patch: Patch, policy: SynthesisPolicy) -> Result<String, Rejection> {
        for (old, replacement) in [
            (&mut self.decisions, patch.decisions),
            (&mut self.active_work_blockers, patch.active_work_blockers),
            (&mut self.findings_evidence, patch.findings_evidence),
            (&mut self.background, patch.background),
        ] {
            if let Some(replacement) = replacement {
                *old = replacement;
            }
            if serde_json::to_string(old)
                .map_err(|_| Rejection::InvalidJson)?
                .chars()
                .count()
                .saturating_sub(2)
                > policy.section_chars
            {
                return Err(Rejection::CarryBudget);
            }
        }
        if [
            &self.decisions,
            &self.active_work_blockers,
            &self.findings_evidence,
            &self.background,
        ]
        .iter()
        .all(|text| text.trim().is_empty())
        {
            return Err(Rejection::EmptyContinuation);
        }
        serde_json::to_string(&self).map_err(|_| Rejection::InvalidJson)
    }
}

/// Validate recognized structured continuation even when supplied through a full envelope.
/// Free text remains the existing representation; versioned objects must be understood exactly.
pub(super) fn validate_continuation(text: &str, policy: SynthesisPolicy) -> Result<(), Rejection> {
    if serde_json::from_str::<serde_json::Value>(text)
        .is_ok_and(|value| value.get("format").is_some())
    {
        Sections::from_prior(text)?.apply(Patch::default(), policy)?;
    }
    Ok(())
}

pub(super) fn merge(
    value: serde_json::Value,
    prior: &CompactionEvidence,
    users: &BTreeSet<u64>,
    new_users: &BTreeSet<u64>,
    reconcilable: &BTreeSet<u64>,
    policy: SynthesisPolicy,
) -> Result<CompactionEvidence, Rejection> {
    let replacement =
        value.get("format").and_then(serde_json::Value::as_str) == Some(SECTION_REPLACE_FORMAT);
    let contract = if replacement {
        super::super::response::ResponseContract::Replace
    } else {
        super::super::response::ResponseContract::Update
    };
    contract.validate(&serde_json::to_string(&value).map_err(|_| Rejection::InvalidJson)?)?;
    let update: Update = serde_json::from_value(value)
        .map_err(|error| super::super::response::decode_error(&error))?;
    if update.format != SECTION_UPDATE_FORMAT && update.format != SECTION_REPLACE_FORMAT {
        return Err(Rejection::UnsupportedFormat);
    }
    let mut protected = prior.clone();
    for source in update
        .current_scope
        .iter()
        .flat_map(|scope| &scope.supersedes)
    {
        if prior
            .current_scope
            .iter()
            .filter(|scope| scope.source_sequence == *source)
            .map(|scope| scope.statement.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            > 1
        {
            return Err(Rejection::AmbiguousGroupSupersession);
        }
    }
    let mut seen = BTreeSet::new();
    for target in update.supersedes_statements {
        // Exact identity plus a newer canonical user replacement is required. No sequence-wide
        // supersession is synthesized: unrelated siblings remain protected even at one source.
        if !seen.insert((target.source_sequence, target.statement.clone()))
            || target.source_sequence >= target.replacement_source_sequence
            || !users.contains(&target.source_sequence)
            || !new_users.contains(&target.replacement_source_sequence)
            || !prior.current_scope.iter().any(|scope| {
                scope.source_sequence == target.source_sequence
                    && scope.statement == target.statement
            })
            || !update.current_scope.iter().any(|scope| {
                scope.source_sequence == target.replacement_source_sequence
                    && scope.statement == target.replacement_statement
                    && !scope.statement.trim().is_empty()
                    && scope.supersedes.is_empty()
            })
        {
            return Err(Rejection::InvalidSupersession);
        }
        protected.current_scope.retain(|scope| {
            scope.source_sequence != target.source_sequence || scope.statement != target.statement
        });
    }
    let sections = Sections::from_prior(&prior.continuation)?;
    let continuation = if replacement {
        sections.apply(update.sections, policy)?
    } else {
        sections.append(update.sections, policy)?
    };
    let wire = serde_json::json!({
        "format": UPDATE_FORMAT,
        "current_scope": update.current_scope,
        "reconciles": update.reconciles,
        "continuation": continuation,
    })
    .to_string();
    // This intermediate encoding is host-produced, not a provider wire response. Its bound is
    // the carry bound checked below; do not apply the provider wire bound a second time.
    let internal_policy = SynthesisPolicy {
        wire_bytes: wire.len(),
        ..policy
    };
    super::merge_response_with_policy(
        &wire,
        &protected,
        users,
        new_users,
        reconcilable,
        internal_policy,
    )
}

/// Consolidate derived carry only. The caller must reserve a durable finite request slot before
/// dispatch, persist only on success, and leave coverage/cursor/unfinished-source sets unchanged.
/// Retry the unaccepted source chunk separately; this result never proves it was covered.
/// Protected scope is byte-for-byte retained. Oversized output is rejected, never clipped.
pub(in super::super) fn consolidate_response(
    response: &str,
    prior: &CompactionEvidence,
    policy: SynthesisPolicy,
) -> Result<CompactionEvidence, Rejection> {
    let value = super::decode(response, policy)?;
    if value
        .get("format")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|format| format != CONSOLIDATION_FORMAT)
    {
        return Err(Rejection::UnsupportedFormat);
    }
    super::super::response::ResponseContract::Consolidation.validate(response)?;
    let update: Consolidation = serde_json::from_value(value)
        .map_err(|error| super::super::response::decode_error(&error))?;
    if update.format != CONSOLIDATION_FORMAT || prior.format != COMPACTION_SUMMARY_FORMAT {
        return Err(Rejection::UnsupportedFormat);
    }
    let mut merged = prior.clone();
    merged.continuation =
        Sections::from_prior(&prior.continuation)?.apply(update.sections, policy)?;
    if serde_json::to_string(&merged)
        .map_err(|_| Rejection::InvalidJson)?
        .chars()
        .count()
        > policy.carry_chars
    {
        return Err(Rejection::CarryBudget);
    }
    Ok(merged)
}

#[cfg(test)]
#[path = "context_compaction_sections_tests.rs"]
mod tests;
