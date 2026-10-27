//! Explicit fixture simulation, not a model of natural-language authorization.
//!
//! With `fake_compaction_fixture = "scope-v1"`, user-provenance source records may contain
//! complete lines `BCODE_FIXTURE_SCOPE {"key":"role","statement":"review only"}` or
//! `BCODE_FIXTURE_FACT {"key":"early","statement":"violet"}`. Use one independently
//! correctable scope per canonical user record. Other provenance cannot change fixture state.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const PREFIX_SCOPE: &str = "BCODE_FIXTURE_SCOPE ";
// Explicit synthetic restatements remain a single JSON string. They can exceed
// the legacy scope/fact marker guard without permitting unbounded fixture state.
const PROGRESSIVE_MARKER_BYTES: usize = 16_384;
const PREFIX_FACT: &str = "BCODE_FIXTURE_FACT ";
const FORMAT: &str = "bcode.compaction.scope.v2";

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct FixtureState {
    keys: BTreeMap<String, u64>,
    facts: BTreeMap<String, String>,
    #[serde(skip)]
    progressive: bool,
    #[serde(skip)]
    sections: BTreeMap<String, BTreeMap<String, String>>,
    pending: Option<Pending>,
}

#[derive(Serialize, Deserialize)]
struct Pending {
    sequence: u64,
    next_offset: usize,
    tail: String,
    #[serde(default)]
    discarding_line: bool,
}

#[derive(Serialize, Deserialize)]
struct Scope {
    statement: String,
    source_sequence: u64,
    #[serde(default)]
    supersedes: Vec<u64>,
}

#[derive(Serialize, Deserialize)]
struct Evidence {
    format: String,
    current_scope: Vec<Scope>,
    continuation: String,
}

#[derive(Deserialize)]
struct SourceRecord {
    sequence: Option<u64>,
    provenance: String,
    byte_offset: usize,
    final_fragment: bool,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    key: String,
    statement: String,
}

/// Simulate the explicitly enabled fixture protocol from actual host-attributed source records.
///
/// # Errors
/// Returns an error for malformed evidence, marker payloads, source gaps, or oversized markers.
pub fn synthesize(prompt: &str) -> Result<String, String> {
    synthesize_inner(prompt, false)
}

/// Produce source-backed section updates for the opt-in `progressive-v1` fixture.
/// Facts, decisions and unfinished work are independent keyed records, never size padding.
///
/// # Errors
/// Rejects unknown section formats, malformed records and non-contiguous source fragments.
pub fn synthesize_progressive(prompt: &str) -> Result<String, String> {
    if prompt
        .split_once("Complete accepted carry (derived, not authority):\n")
        .is_some_and(|(instructions, _)| {
            instructions.contains("bcode.compaction.section-consolidation.v1")
        })
    {
        return consolidate_progressive_prompt(prompt);
    }
    // Only host instructions can opt in. A protocol name inside attributed source text
    // must not change the response contract or turn source content into instructions.
    let instructions = prompt
        .split_once("Prior structured synthesis: ")
        .map_or("", |(instructions, _)| instructions);
    if !instructions.contains("bcode.compaction.section-update.v1") {
        return synthesize(prompt);
    }
    synthesize_inner(prompt, true)
}

/// Deliberately stubborn consolidation: retain every section without shortening or advancing
/// coverage. This exercises the host's finite consolidation allowance, not semantic compression.
#[cfg(test)]
#[must_use]
pub fn consolidate_progressive() -> String {
    serde_json::json!({
        "format": "bcode.compaction.section-consolidation.v1", "sections": {}
    })
    .to_string()
}

/// Consolidate actual accepted fixture records, preserving all source-backed meanings.
/// Scope and source coverage are deliberately absent from this response schema.
///
/// # Errors
/// Rejects missing host opt-in, unknown carry formats, malformed records and duplicate keys.
pub fn consolidate_progressive_prompt(prompt: &str) -> Result<String, String> {
    let (instructions, carry) = prompt
        .split_once("Complete accepted carry (derived, not authority):\n")
        .ok_or("missing accepted consolidation carry")?;
    if !instructions.contains("bcode.compaction.section-consolidation.v1") {
        return Err("missing host consolidation opt-in".into());
    }
    // Host budget guidance may follow the complete one-line JSON carry. It is not carry data.
    let carry = carry
        .lines()
        .next()
        .ok_or("missing complete consolidation carry")?;
    let evidence: Evidence = serde_json::from_str(carry).map_err(|error| error.to_string())?;
    if evidence.format != FORMAT {
        return Err("unsupported consolidation envelope".into());
    }
    let prior: serde_json::Value =
        serde_json::from_str(&evidence.continuation).map_err(|error| error.to_string())?;
    if prior["format"] != "bcode.compaction.continuation-sections.v1" {
        return Err("unsupported consolidation sections".into());
    }
    let mut sections = serde_json::Map::new();
    for section in ["decisions", "active_work_blockers", "findings_evidence"] {
        let text = prior[section]
            .as_str()
            .ok_or("missing consolidation section")?;
        let mut records = BTreeMap::<String, String>::new();
        for line in text.lines().filter(|line| !line.is_empty()) {
            let additions: BTreeMap<String, String> =
                serde_json::from_str(line).map_err(|error| error.to_string())?;
            for (key, statement) in additions {
                if records.insert(key, statement).is_some() {
                    return Err("duplicate consolidation evidence".into());
                }
            }
        }
        sections.insert(
            section.into(),
            serde_json::Value::String(if records.is_empty() {
                String::new()
            } else {
                serde_json::to_string(&records).map_err(|error| error.to_string())?
            }),
        );
    }
    // Parser checkpoints include unfinished source endings. Retain background exactly;
    // compacting them would require independently proving equivalence of parser state.
    sections.insert(
        "background".into(),
        prior["background"]
            .as_str()
            .ok_or("missing consolidation background")?
            .into(),
    );
    serde_json::to_string(&serde_json::json!({
        "format": "bcode.compaction.section-consolidation.v1", "sections": sections
    }))
    .map_err(|error| error.to_string())
}

fn restore_fixture_state(
    evidence: &Evidence,
    progressive: bool,
) -> Result<(FixtureState, String), String> {
    let mut background_base = evidence.continuation.clone();
    let state = if progressive {
        let mut state = FixtureState::default();
        if evidence.continuation.starts_with('{') {
            let sections: serde_json::Value =
                serde_json::from_str(&evidence.continuation).map_err(|error| error.to_string())?;
            if sections["format"] != "bcode.compaction.continuation-sections.v1" {
                return Err("unsupported progressive continuation format".into());
            }
            let background = sections["background"].as_str().unwrap_or("{}");
            background_base = if background.starts_with("BCODE_FIXTURE_STATE_V1 ") {
                String::new()
            } else {
                background
                    .split_once("\nBCODE_FIXTURE_STATE_V1 ")
                    .map_or_else(|| background.to_owned(), |(base, _)| base.to_owned())
            };
            if let Some(checkpoint) = background
                .lines()
                .rev()
                .find_map(|line| line.strip_prefix("BCODE_FIXTURE_STATE_V1 "))
            {
                state = serde_json::from_str(checkpoint).map_err(|error| error.to_string())?;
            } else if background.starts_with("{\"keys\":") {
                // Compatibility with the initial explicit progressive fixture checkpoint.
                state = serde_json::from_str(background.lines().last().unwrap_or("{}"))
                    .map_err(|error| error.to_string())?;
                background_base.clear();
            }
            for section in ["decisions", "active_work_blockers", "findings_evidence"] {
                let text = sections[section]
                    .as_str()
                    .ok_or("missing progressive section")?;
                let mut records = BTreeMap::new();
                for line in text.lines().filter(|line| !line.is_empty()) {
                    let additions: BTreeMap<String, String> =
                        serde_json::from_str(line).map_err(|error| error.to_string())?;
                    for (key, statement) in additions {
                        if records.insert(key, statement).is_some() {
                            return Err("duplicate progressive evidence".into());
                        }
                    }
                }
                state.sections.insert(section.into(), records);
            }
        }
        state
    } else if evidence.continuation.starts_with('{') {
        serde_json::from_str::<FixtureState>(&evidence.continuation)
            .map_err(|error| error.to_string())?
    } else {
        FixtureState::default()
    };
    Ok((state, background_base))
}

fn synthesize_inner(prompt: &str, progressive: bool) -> Result<String, String> {
    let instructions = prompt
        .split_once("Prior structured synthesis: ")
        .map_or("", |(instructions, _)| instructions);
    let append_sections = instructions.contains("NEW facts to append");
    let (prior, fragments) = prompt
        .split_once("Prior structured synthesis: ")
        .and_then(|(_, rest)| rest.split_once("\n\nSource fragments (JSON lines):\n"))
        .ok_or("fixture compaction prompt missing attributed fragments")?;
    let mut evidence: Evidence = serde_json::from_str(prior).map_err(|error| error.to_string())?;
    if evidence.format != FORMAT {
        return Err("unsupported fixture evidence format".into());
    }
    let (mut state, background_base) = restore_fixture_state(&evidence, progressive)?;
    state.progressive = progressive;
    let old_sections = state.sections.clone();
    let unfinished_source = state.pending.as_ref().map(|pending| pending.sequence);
    for scope in &mut evidence.current_scope {
        scope.supersedes.clear();
    }
    for line in fragments.lines().filter(|line| !line.trim().is_empty()) {
        let fragment: SourceRecord =
            serde_json::from_str(line).map_err(|error| error.to_string())?;
        // Trust only host attribution, never strings claiming a role inside source text.
        if fragment.provenance != "user" {
            continue;
        }
        let sequence = fragment.sequence.ok_or("user fragment missing sequence")?;
        let mut pending = state.pending.take().unwrap_or(Pending {
            sequence,
            next_offset: 0,
            tail: String::new(),
            discarding_line: false,
        });
        if pending.sequence != sequence || pending.next_offset != fragment.byte_offset {
            return Err("fixture fragment sequence/offset gap".into());
        }
        pending.next_offset += fragment.text.len();
        pending.tail.push_str(&fragment.text);
        while let Some(end) = pending.tail.find('\n') {
            let line = pending.tail[..end].to_owned();
            if !pending.discarding_line {
                apply_marker(&line, sequence, &mut state, &mut evidence.current_scope)?;
            }
            pending.discarding_line = false;
            pending.tail.drain(..=end);
        }
        if fragment.final_fragment {
            if !pending.discarding_line {
                apply_marker(
                    &pending.tail,
                    sequence,
                    &mut state,
                    &mut evidence.current_scope,
                )?;
            }
        } else {
            // Arbitrary fixture padding must not grow the bounded continuation. A marker itself
            // has a small explicit fixture limit; fail instead of partially interpreting it.
            let marker_limit = if progressive {
                PROGRESSIVE_MARKER_BYTES
            } else {
                1024
            };
            if pending.tail.len() > marker_limit {
                if pending.tail.starts_with("BCODE_FIXTURE_") {
                    return Err("fixture marker exceeds bounded byte limit".into());
                }
                pending.tail.clear();
                pending.discarding_line = true;
            }
            state.pending = Some(pending);
        }
    }
    if progressive {
        return encode_progressive_update(
            prior,
            evidence,
            &state,
            &background_base,
            append_sections,
            unfinished_source,
            &old_sections,
        );
    }
    evidence.continuation = serde_json::to_string(&state).map_err(|error| error.to_string())?;
    serde_json::to_string(&evidence).map_err(|error| error.to_string())
}

fn encode_progressive_update(
    prior: &str,
    mut evidence: Evidence,
    state: &FixtureState,
    background_base: &str,
    append_sections: bool,
    unfinished_source: Option<u64>,
    old_sections: &BTreeMap<String, BTreeMap<String, String>>,
) -> Result<String, String> {
    let mut sections = serde_json::Map::new();
    for (section, records) in &state.sections {
        if old_sections.get(section) == Some(records) {
            continue;
        }
        let additions: BTreeMap<_, _> = records
            .iter()
            .filter(|(key, _)| {
                !old_sections
                    .get(section)
                    .is_some_and(|old| old.contains_key(*key))
            })
            .collect();
        sections.insert(
            section.clone(),
            serde_json::Value::String(
                serde_json::to_string(&if append_sections {
                    additions
                } else {
                    records.iter().collect()
                })
                .map_err(|error| error.to_string())?,
            ),
        );
    }
    let checkpoint = format!(
        "BCODE_FIXTURE_STATE_V1 {}",
        serde_json::to_string(&state).map_err(|error| error.to_string())?
    );
    let background = if append_sections || background_base.is_empty() {
        checkpoint
    } else {
        format!("{background_base}\n{checkpoint}")
    };
    sections.insert("background".into(), serde_json::Value::String(background));
    // New/corrected scope only. The host, not the fake provider, preserves unchanged scope.
    let old: Evidence = serde_json::from_str(prior).map_err(|error| error.to_string())?;
    evidence.current_scope.retain(|scope| {
        !old.current_scope.iter().any(|prior| {
            prior.source_sequence == scope.source_sequence && prior.statement == scope.statement
        })
    });
    let reconciles: Vec<u64> = unfinished_source
        .filter(|sequence| {
            evidence
                .current_scope
                .iter()
                .any(|scope| scope.source_sequence == *sequence)
        })
        .into_iter()
        .collect();
    serde_json::to_string(&serde_json::json!({
        "format": "bcode.compaction.section-update.v1",
        "current_scope": evidence.current_scope,
        "reconciles": reconciles, "supersedes_statements": [], "sections": sections,
    }))
    .map_err(|error| error.to_string())
}

fn apply_marker(
    line: &str,
    sequence: u64,
    state: &mut FixtureState,
    scopes: &mut Vec<Scope>,
) -> Result<(), String> {
    if state.progressive {
        for (prefix, section) in [
            ("BCODE_FIXTURE_FACT ", "findings_evidence"),
            ("BCODE_FIXTURE_DECISION ", "decisions"),
            ("BCODE_FIXTURE_BLOCKER ", "active_work_blockers"),
            ("BCODE_FIXTURE_INSTRUCTION ", "active_work_blockers"),
        ] {
            if let Some(payload) = line.strip_prefix(prefix) {
                let marker: Marker =
                    serde_json::from_str(payload).map_err(|error| error.to_string())?;
                if marker.key.is_empty()
                    || marker.statement.is_empty()
                    || payload.len() > PROGRESSIVE_MARKER_BYTES
                {
                    return Err("invalid progressive fixture marker".into());
                }
                let records = state.sections.entry(section.into()).or_default();
                if let Some(existing) = records.get(&marker.key) {
                    if existing != &marker.statement {
                        return Err("conflicting immutable progressive fixture record".into());
                    }
                } else {
                    records.insert(marker.key, marker.statement);
                }
                return Ok(());
            }
        }
    }
    let (scope, payload) = if let Some(payload) = line.strip_prefix(PREFIX_SCOPE) {
        (true, payload)
    } else if let Some(payload) = line.strip_prefix(PREFIX_FACT) {
        (false, payload)
    } else {
        return Ok(());
    };
    let marker: Marker = serde_json::from_str(payload).map_err(|error| error.to_string())?;
    if marker.key.is_empty() || marker.statement.is_empty() || payload.len() > 1024 {
        return Err("invalid fixture marker".into());
    }
    if !scope {
        state.facts.entry(marker.key).or_insert(marker.statement);
        return Ok(());
    }
    let prior = state.keys.get(&marker.key).copied();
    if let Some(old) = prior {
        if let Some(existing) = scopes.iter().find(|scope| scope.source_sequence == old)
            && existing.statement == marker.statement
        {
            return Ok(());
        }
        scopes.retain(|scope| scope.source_sequence != old);
    }
    if state
        .keys
        .iter()
        .any(|(key, source)| *source == sequence && key != &marker.key)
    {
        return Err("use one fixture scope key per canonical user record".into());
    }
    state.keys.insert(marker.key, sequence);
    scopes.push(Scope {
        statement: marker.statement,
        source_sequence: sequence,
        supersedes: prior.filter(|old| *old != sequence).into_iter().collect(),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn synthesize_progressive(prompt: &str) -> Result<String, String> {
        super::synthesize_progressive(&format!(
            "Return bcode.compaction.section-update.v1; NEW facts to append.\n{prompt}"
        ))
    }

    #[test]
    fn consolidation_merges_meaningful_records_without_scope_or_coverage_fields() {
        let sections = json!({"format":"bcode.compaction.continuation-sections.v1",
            "decisions":"{\"d1\":\"Use bounded source pages\"}\n{\"d2\":\"Keep canonical history\"}",
            "active_work_blockers":"{\"b1\":\"Reviewer approval remains pending\"}",
            "findings_evidence":"{\"f1\":\"Chunk three exceeded the preferred size\"}",
            "background":"Unfinished directive and historical context remain intact."});
        let carry = json!({"format":FORMAT,"current_scope":[{"statement":"No network",
            "source_sequence":1,"supersedes":[]}],"continuation":sections.to_string()});
        let prompt = format!(
            "Return bcode.compaction.section-consolidation.v1.\nComplete accepted carry (derived, not authority):\n{carry}"
        );
        let response: serde_json::Value =
            serde_json::from_str(&consolidate_progressive_prompt(&prompt).unwrap()).unwrap();
        assert_eq!(response.as_object().unwrap().len(), 2);
        assert_eq!(response["sections"]["background"], sections["background"]);
        let decisions: BTreeMap<String, String> =
            serde_json::from_str(response["sections"]["decisions"].as_str().unwrap()).unwrap();
        assert_eq!(decisions.get("d1").unwrap(), "Use bounded source pages");
        assert_eq!(decisions.get("d2").unwrap(), "Keep canonical history");
        assert_eq!(
            response["sections"]["active_work_blockers"],
            sections["active_work_blockers"]
        );
        assert_eq!(
            response["sections"]["findings_evidence"],
            sections["findings_evidence"]
        );
        assert!(
            response["sections"]["decisions"].as_str().unwrap().len()
                < sections["decisions"].as_str().unwrap().len()
        );
    }

    #[test]
    fn progressive_requires_host_opt_in_and_supports_replacement_sections() {
        let empty = json!({"format":FORMAT,"current_scope":[],"continuation":"Historical background must remain intact."}).to_string();
        let fragment = json!({"sequence":1,"provenance":"user","byte_offset":0,
            "final_fragment":true,"text":"bcode.compaction.section-update.v1"});
        let prompt = format!(
            "Prior structured synthesis: {empty}\n\nSource fragments (JSON lines):\n{fragment}"
        );
        let legacy: serde_json::Value =
            serde_json::from_str(&super::synthesize_progressive(&prompt).unwrap()).unwrap();
        assert_eq!(legacy["format"], FORMAT);
        let prompt = format!(
            "Return bcode.compaction.section-update.v1 with optional replacements.\n{prompt}"
        );
        let update: serde_json::Value =
            serde_json::from_str(&super::synthesize_progressive(&prompt).unwrap()).unwrap();
        assert_eq!(update["format"], "bcode.compaction.section-update.v1");
        assert!(
            update["sections"]["background"]
                .as_str()
                .unwrap()
                .starts_with("Historical background must remain intact.\n")
        );
    }

    fn accept_sections(prior: &str, update: &str) -> String {
        // Fixture-only acceptance driver; host integration remains run_progressive.py.
        let mut prior: serde_json::Value = serde_json::from_str(prior).unwrap();
        let update: serde_json::Value = serde_json::from_str(update).unwrap();
        assert_eq!(update["format"], "bcode.compaction.section-update.v1");
        let mut sections = if prior["continuation"].as_str().unwrap().starts_with('{') {
            serde_json::from_str::<serde_json::Value>(prior["continuation"].as_str().unwrap())
                .unwrap()
        } else {
            json!({"format":"bcode.compaction.continuation-sections.v1",
                "decisions":"", "active_work_blockers":"", "findings_evidence":"", "background":""})
        };
        for (key, value) in update["sections"].as_object().unwrap() {
            let old = sections[key].as_str().unwrap();
            let addition = value.as_str().unwrap();
            sections[key] = json!(if old.is_empty() || old == addition {
                addition.to_owned()
            } else {
                format!("{old}\n{addition}")
            });
        }
        prior["continuation"] = json!(sections.to_string());
        prior.to_string()
    }

    #[test]
    fn progressive_growth_is_source_backed_and_shortening_is_stubborn() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/evals/compaction/retention_cases.json"
        ))
        .unwrap();
        let mut prior = json!({"format":FORMAT,"current_scope":[],"continuation":""}).to_string();
        let mut statements = Vec::new();
        let mut sequence = 1;
        for case in corpus["cases"].as_array().unwrap() {
            for batch in case["batches"].as_array().unwrap() {
                for check in batch["oracle"].as_array().unwrap() {
                    let statement = format!(
                        "Historical snapshot {}, batch {}, sources {}: {}",
                        case["id"],
                        batch["id"],
                        check["sources"],
                        check["proposition"].as_str().unwrap()
                    );
                    statements.push(statement.clone());
                    let marker = format!(
                        "BCODE_FIXTURE_FACT {}",
                        json!({
                            "key":format!("snapshot-{sequence}"), "statement":statement
                        })
                    );
                    let fragment = json!({"sequence":sequence,"provenance":"user",
                        "byte_offset":0,"final_fragment":true,"text":marker});
                    let prompt = format!(
                        "bcode.compaction.section-update.v1 NEW facts to append\nPrior structured synthesis: {prior}\n\nSource fragments (JSON lines):\n{fragment}"
                    );
                    let update = synthesize_progressive(&prompt).unwrap();
                    prior = accept_sections(&prior, &update);
                    let accepted: serde_json::Value = serde_json::from_str(&prior).unwrap();
                    let sections: serde_json::Value =
                        serde_json::from_str(accepted["continuation"].as_str().unwrap()).unwrap();
                    let records: Vec<serde_json::Value> = sections["findings_evidence"]
                        .as_str()
                        .unwrap()
                        .lines()
                        .map(|line| serde_json::from_str(line).unwrap())
                        .collect();
                    for expected in &statements {
                        assert!(records.iter().any(|record| {
                            record
                                .as_object()
                                .unwrap()
                                .values()
                                .any(|value| value.as_str() == Some(expected.as_str()))
                        }));
                    }
                    sequence += 1;
                }
            }
        }
        let accepted: serde_json::Value = serde_json::from_str(&prior).unwrap();
        assert!(accepted["continuation"].as_str().unwrap().chars().count() > 6000);
        for _ in 0..2 {
            let consolidation: serde_json::Value =
                serde_json::from_str(&consolidate_progressive()).unwrap();
            assert_eq!(
                consolidation["format"],
                "bcode.compaction.section-consolidation.v1"
            );
            assert_eq!(consolidation["sections"], json!({}));
        }
        // Repeated compaction contributes nothing, rather than re-copying known-size carry.
        let update: serde_json::Value = serde_json::from_str(
            &synthesize_progressive(&format!(
                "Prior structured synthesis: {prior}\n\nSource fragments (JSON lines):\n"
            ))
            .unwrap(),
        )
        .unwrap();
        assert!(update["sections"].get("findings_evidence").is_none());
    }

    #[test]
    fn repeated_progressive_json_marker_preserves_unicode_across_fragments_and_duplicates() {
        let empty = json!({"format":FORMAT,"current_scope":[],"continuation":""}).to_string();
        let proposition = "監査 🧭 e\u{301}: no edits; no network; retain q1,q3";
        let statement = [proposition; 32].join(" Fixture restatement: ");
        let text = format!(
            "BCODE_FIXTURE_BLOCKER {}",
            json!({"key":"q1,q3/restriction","statement":statement})
        );
        assert!(text.len() > 1024);
        let split = text
            .char_indices()
            .map(|(index, _)| index)
            .find(|index| *index > 1100)
            .unwrap();
        let first_fragment = json!({"sequence":4,"provenance":"user","byte_offset":0,
            "final_fragment":false,"text":&text[..split]});
        let first = synthesize_progressive(&format!(
            "bcode.compaction.section-update.v1 NEW facts to append\nPrior structured synthesis: {empty}\n\nSource fragments (JSON lines):\n{first_fragment}"
        )).unwrap();
        let prior = accept_sections(&empty, &first);
        let last_fragment = json!({"sequence":4,"provenance":"user","byte_offset":split,
            "final_fragment":true,"text":&text[split..]});
        let update = synthesize_progressive(&format!(
            "bcode.compaction.section-update.v1 NEW facts to append\nPrior structured synthesis: {prior}\n\nSource fragments (JSON lines):\n{last_fragment}"
        )).unwrap();
        let accepted: serde_json::Value =
            serde_json::from_str(&accept_sections(&prior, &update)).unwrap();
        let sections: serde_json::Value =
            serde_json::from_str(accepted["continuation"].as_str().unwrap()).unwrap();
        let records: BTreeMap<String, String> =
            serde_json::from_str(sections["active_work_blockers"].as_str().unwrap()).unwrap();
        assert_eq!(records["q1,q3/restriction"], statement);
        let mut state = FixtureState {
            progressive: true,
            ..FixtureState::default()
        };
        let mut scopes = Vec::new();
        apply_marker(&text, 4, &mut state, &mut scopes).unwrap();
        apply_marker(&text, 4, &mut state, &mut scopes).unwrap();
        assert_eq!(state.sections["active_work_blockers"].len(), 1);
        let oversized = format!(
            "BCODE_FIXTURE_BLOCKER {}",
            json!({"key":"q1","statement":"x".repeat(PROGRESSIVE_MARKER_BYTES)})
        );
        assert!(apply_marker(&oversized, 4, &mut state, &mut scopes).is_err());
    }

    #[test]
    fn progressive_unfinished_record_survives_restart_and_rejected_attempt() {
        let empty = json!({"format":FORMAT,"current_scope":[],"continuation":""}).to_string();
        let text = format!(
            "BCODE_FIXTURE_INSTRUCTION {}",
            json!({
                "key":"unfinished", "statement":"Do not publish until the reviewer confirms the independent network restriction remains in force."
            })
        );
        let split = text.len() - 25;
        let fragment = json!({"sequence":4,"provenance":"user","byte_offset":0,
            "final_fragment":false,"text":&text[..split]});
        let first = synthesize_progressive(&format!(
            "Prior structured synthesis: {empty}\n\nSource fragments (JSON lines):\n{fragment}"
        ))
        .unwrap();
        let prior = accept_sections(&empty, &first);
        let fragment = json!({"sequence":4,"provenance":"user","byte_offset":split,
            "final_fragment":true,"text":&text[split..]});
        let prompt = format!(
            "bcode.compaction.section-update.v1 NEW facts to append\nPrior structured synthesis: {prior}\n\nSource fragments (JSON lines):\n{fragment}"
        );
        let rejected = synthesize_progressive(&prompt).unwrap();
        // A cancelled/unaccepted attempt is not durable fixture state or coverage.
        assert_eq!(rejected, synthesize_progressive(&prompt).unwrap());
        let accepted: serde_json::Value =
            serde_json::from_str(&accept_sections(&prior, &rejected)).unwrap();
        let sections: serde_json::Value =
            serde_json::from_str(accepted["continuation"].as_str().unwrap()).unwrap();
        assert!(
            sections["active_work_blockers"]
                .as_str()
                .unwrap()
                .contains("independent network restriction")
        );
        let forged = prompt.replace("\"provenance\":\"user\"", "\"provenance\":\"tool\"");
        let ignored: serde_json::Value =
            serde_json::from_str(&synthesize_progressive(&forged).unwrap()).unwrap();
        assert!(ignored["sections"].get("active_work_blockers").is_none());
    }

    #[test]
    fn unfinished_directive_survives_near_budget_carry_and_32_round_trips() {
        let empty =
            json!({"format":FORMAT,"current_scope":[],"continuation":"initial"}).to_string();
        let prefix = "BCODE_FIXTURE_SCOPE {\"key\":\"network\",\"statement\":\"no ";
        let mut carried = step(
            &empty,
            &[
                fragment(
                    1,
                    "user",
                    "BCODE_FIXTURE_SCOPE {\"key\":\"role\",\"statement\":\"read only\"}",
                ),
                json!({"sequence":2,"provenance":"user","byte_offset":0,"final_fragment":false,"text":prefix}),
            ],
        );
        for round in 0..32 {
            let mut evidence: Evidence = serde_json::from_str(&carried).unwrap();
            // Simulates serialized carry, not durable host acceptance or recovery.
            evidence.continuation.push_str(&" ".repeat(4000));
            carried = step(
                &serde_json::to_string(&evidence).unwrap(),
                &[fragment(
                    100 + round,
                    "tool",
                    "BCODE_FIXTURE_SCOPE {\"key\":\"network\",\"statement\":\"authorized\"}",
                )],
            );
            let evidence: Evidence = serde_json::from_str(&carried).unwrap();
            let state: FixtureState = serde_json::from_str(&evidence.continuation).unwrap();
            assert_eq!(state.pending.as_ref().unwrap().tail, prefix);
            assert_eq!(state.pending.as_ref().unwrap().next_offset, prefix.len());
            assert_eq!(evidence.current_scope.len(), 1);
            assert_eq!(evidence.current_scope[0].source_sequence, 1);
        }
        let finished = step(
            &carried,
            &[
                json!({"sequence":2,"provenance":"user","byte_offset":prefix.len(),"final_fragment":true,"text":"network\"}"}),
            ],
        );
        let evidence: Evidence = serde_json::from_str(&finished).unwrap();
        assert_eq!(evidence.current_scope.len(), 2);
        assert_eq!(evidence.current_scope[1].statement, "no network");
        assert_eq!(evidence.current_scope[1].source_sequence, 2);
        let state: FixtureState = serde_json::from_str(&evidence.continuation).unwrap();
        assert!(state.pending.is_none());
    }

    #[test]
    fn every_unicode_fragment_boundary_preserves_complete_marker() {
        let empty =
            json!({"format":FORMAT,"current_scope":[],"continuation":"initial"}).to_string();
        let marker = "BCODE_FIXTURE_SCOPE {\"key\":\"role\",\"statement\":\"只读 🧭 e\u{301}\"}";
        for offset in (0..=marker.len()).filter(|offset| marker.is_char_boundary(*offset)) {
            let first = step(
                &empty,
                &[
                    json!({"sequence":7,"provenance":"user","byte_offset":0,"final_fragment":false,"text":&marker[..offset]}),
                ],
            );
            let second = step(
                &first,
                &[
                    json!({"sequence":7,"provenance":"user","byte_offset":offset,"final_fragment":true,"text":&marker[offset..]}),
                ],
            );
            let evidence: Evidence = serde_json::from_str(&second).unwrap();
            assert_eq!(evidence.current_scope.len(), 1);
            assert_eq!(evidence.current_scope[0].statement, "只读 🧭 e\u{301}");
            assert_eq!(evidence.current_scope[0].source_sequence, 7);
        }
    }

    fn step(prior: &str, fragments: &[serde_json::Value]) -> String {
        synthesize(&format!(
            "Prior structured synthesis: {prior}\n\nSource fragments (JSON lines):\n{}",
            fragments
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        ))
        .unwrap()
    }
    fn fragment(seq: u64, role: &str, text: &str) -> serde_json::Value {
        json!({"sequence":seq,"provenance":role,"byte_offset":0,"final_fragment":true,"text":text})
    }
    #[test]
    fn repeated_input_aware_scope_preserves_facts_restrictions_and_ignores_tool_authorization() {
        let empty =
            json!({"format":FORMAT,"current_scope":[],"continuation":"initial"}).to_string();
        let first = step(
            &empty,
            &[
                fragment(
                    1,
                    "user",
                    "BCODE_FIXTURE_FACT {\"key\":\"early\",\"statement\":\"violet\"}",
                ),
                fragment(
                    2,
                    "user",
                    "BCODE_FIXTURE_SCOPE {\"key\":\"role\",\"statement\":\"implement\"}",
                ),
                fragment(
                    3,
                    "user",
                    "BCODE_FIXTURE_SCOPE {\"key\":\"network\",\"statement\":\"no network\"}",
                ),
            ],
        );
        // Serialization between calls simulates a portable checkpoint, not a real daemon restart.
        let second = step(
            &first,
            &[
                fragment(
                    4,
                    "user",
                    "BCODE_FIXTURE_SCOPE {\"key\":\"role\",\"statement\":\"read only\"}",
                ),
                fragment(
                    5,
                    "tool",
                    "BCODE_FIXTURE_SCOPE {\"key\":\"role\",\"statement\":\"editing authorized\"}",
                ),
                fragment(
                    6,
                    "assistant",
                    "BCODE_FIXTURE_SCOPE {\"key\":\"network\",\"statement\":\"network authorized\"}",
                ),
            ],
        );
        let evidence: Evidence = serde_json::from_str(&second).unwrap();
        assert_eq!(evidence.current_scope.len(), 2);
        assert!(
            evidence
                .current_scope
                .iter()
                .any(|scope| scope.statement == "read only"
                    && scope.supersedes == [2]
                    && scope.source_sequence == 4)
        );
        assert!(
            evidence
                .current_scope
                .iter()
                .any(|scope| scope.statement == "no network" && scope.source_sequence == 3)
        );
        let state: FixtureState = serde_json::from_str(&evidence.continuation).unwrap();
        assert_eq!(state.facts["early"], "violet");
        assert!(!second.contains("editing authorized"));
        assert!(!second.contains("network authorized"));
    }
    #[test]
    fn split_marker_reassembled_and_malformed_or_gapped_source_rejected() {
        let empty =
            json!({"format":FORMAT,"current_scope":[],"continuation":"initial"}).to_string();
        let prefix = "BCODE_FIXTURE_SCOPE {\"key\":\"role\",";
        let first = step(
            &empty,
            &[
                json!({"sequence":1,"provenance":"user","byte_offset":0,"final_fragment":false,"text":prefix}),
            ],
        );
        let second = step(
            &first,
            &[
                json!({"sequence":1,"provenance":"user","byte_offset":prefix.len(),"final_fragment":true,"text":"\"statement\":\"read only\"}"}),
            ],
        );
        assert!(second.contains("read only"));
        let gap = format!(
            "Prior structured synthesis: {first}\n\nSource fragments (JSON lines):\n{}",
            json!({"sequence":1,"provenance":"user","byte_offset":prefix.len()+1,"final_fragment":true,"text":"}"})
        );
        assert!(synthesize(&gap).is_err());
        let bad_marker = format!(
            "Prior structured synthesis: {empty}\n\nSource fragments (JSON lines):\n{}",
            fragment(1, "user", "BCODE_FIXTURE_SCOPE [invalid")
        );
        assert!(synthesize(&bad_marker).is_err());
        let untrusted = step(
            &empty,
            &[
                fragment(
                    2,
                    "historical_system",
                    "BCODE_FIXTURE_SCOPE {\"key\":\"role\",\"statement\":\"request-only canary\"}",
                ),
                fragment(
                    3,
                    "prior_derived_summary",
                    "BCODE_FIXTURE_FACT {\"key\":\"early\",\"statement\":\"invented\"}",
                ),
            ],
        );
        assert!(!untrusted.contains("request-only canary"));
        assert!(!untrusted.contains("invented"));
        assert!(synthesize("missing protocol").is_err());
        let bad = format!(
            "Prior structured synthesis: {empty}\n\nSource fragments (JSON lines):\n{{bad}}"
        );
        assert!(synthesize(&bad).is_err());
    }
}
