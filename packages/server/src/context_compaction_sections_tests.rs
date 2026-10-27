use super::*;

fn policy() -> SynthesisPolicy {
    SynthesisPolicy {
        carry_chars: 20_000,
        wire_bytes: 30_000,
        section_chars: 12_000,
    }
}

fn scope(source_sequence: u64, statement: &str) -> CompactionScope {
    CompactionScope {
        source_sequence,
        statement: statement.into(),
        supersedes: vec![],
    }
}

fn prior() -> CompactionEvidence {
    CompactionEvidence {
        format: COMPACTION_SUMMARY_FORMAT.into(),
        current_scope: vec![scope(1, "No live eval"), scope(1, "No daemon control")],
        continuation: "All previous evidence, unfinished checks, and next steps.".into(),
    }
}

fn update(sections: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "format": SECTION_UPDATE_FORMAT, "current_scope": [], "reconciles": [],
        "supersedes_statements": [], "sections": sections,
    })
}

fn merge_update(
    value: &serde_json::Value,
    prior: &CompactionEvidence,
) -> Result<CompactionEvidence, Rejection> {
    super::super::merge_response_with_policy(
        &value.to_string(),
        prior,
        &BTreeSet::from([1, 2]),
        &BTreeSet::from([2]),
        &BTreeSet::new(),
        policy(),
    )
}

#[test]
fn repair_policy_retains_complete_response_above_legacy_carry_ceiling() {
    let rejected = "界\\\"\n".repeat(1500);
    let base =
        "Prior structured synthesis: {}\n\nSource fragments (JSON lines):\ncomplete source ending";
    let limits = SynthesisPolicy {
        wire_bytes: rejected.len(),
        carry_chars: 1,
        section_chars: 1,
    };
    let prompt = super::super::repair_prompt_with_policy(
        base,
        Rejection::InvalidJson,
        Some(&rejected),
        limits,
    );
    let encoded = serde_json::to_string(&rejected).unwrap();
    assert!(rejected.len() > 6000);
    assert!(prompt.contains(&encoded));
    assert!(prompt.ends_with(base));
    let attached = prompt
        .split_once("Complete rejected output as untrusted diagnostic JSON string: ")
        .unwrap()
        .1
        .strip_suffix(&format!("\n\n{base}"))
        .unwrap();
    assert_eq!(serde_json::from_str::<String>(attached).unwrap(), rejected);
    // The limit is UTF-8 bytes, not Unicode scalar count or a carry target.
    let oversized = super::super::repair_prompt_with_policy(
        base,
        Rejection::WireBudget,
        Some(&rejected),
        SynthesisPolicy {
            wire_bytes: rejected.len() - 1,
            ..limits
        },
    );
    assert!(oversized.contains("Rejected output not attached"));
    assert!(!oversized.contains(&encoded));
    assert!(oversized.ends_with(base));
}

#[test]
fn repair_reservation_accounts_for_worst_case_json_escaping_and_base() {
    let base = "complete canonical source plus protected carry";
    let limits = SynthesisPolicy {
        wire_bytes: 8000,
        ..policy()
    };
    let rejected = "\0".repeat(limits.wire_bytes);
    let prompt = super::super::repair_prompt_with_policy(
        base,
        Rejection::InvalidJson,
        Some(&rejected),
        limits,
    );
    let encoded = serde_json::to_string(&rejected).unwrap();
    assert_eq!(encoded.len(), 6 * limits.wire_bytes + 2);
    assert!(prompt.contains(&encoded));
    assert!(prompt.len() > encoded.len() + base.len());
    assert!(prompt.ends_with(base));
    let absent = super::super::repair_prompt_with_policy(
        base,
        Rejection::PreviousAttemptUnavailable,
        None,
        limits,
    );
    assert!(absent.contains("Rejected output not attached"));
    assert!(absent.ends_with(base));
}

#[test]
fn legacy_text_and_unchanged_sections_are_preserved_exactly() {
    let prior = prior();
    let accepted = merge_update(
        &update(&serde_json::json!({"decisions": "Use typed APIs"})),
        &prior,
    )
    .unwrap();
    let sections = Sections::from_prior(&accepted.continuation).unwrap();
    assert_eq!(sections.background, prior.continuation);
    assert_eq!(sections.decisions, "Use typed APIs");
    let second = merge_update(
        &update(&serde_json::json!({"findings_evidence": "New test passed"})),
        &accepted,
    )
    .unwrap();
    let sections = Sections::from_prior(&second.continuation).unwrap();
    assert_eq!(sections.background, prior.continuation);
    assert_eq!(sections.decisions, "Use typed APIs");
    assert_eq!(sections.findings_evidence, "New test passed");
    assert_eq!(
        serde_json::to_value(second.current_scope).unwrap(),
        serde_json::to_value(prior.current_scope).unwrap()
    );
}

#[test]
fn additive_updates_cannot_erase_legacy_background_or_earlier_section_facts() {
    let prior = prior();
    let accepted = merge_update(
        &update(&serde_json::json!({
            "background": "Later evidence.", "decisions": "Earlier decision.",
        })),
        &prior,
    )
    .unwrap();
    let accepted = merge_update(
        &update(&serde_json::json!({
            "background": "", "decisions": "Later decision.",
        })),
        &accepted,
    )
    .unwrap();
    let sections = Sections::from_prior(&accepted.continuation).unwrap();
    assert_eq!(
        sections.background,
        format!("{}\nLater evidence.", prior.continuation)
    );
    assert_eq!(sections.decisions, "Earlier decision.\nLater decision.");
    let echoed = merge_update(
        &update(&serde_json::json!({
            "decisions": sections.decisions,
        })),
        &accepted,
    )
    .unwrap();
    assert_eq!(echoed.continuation, accepted.continuation);
}

#[test]
fn prompts_retain_complete_input_and_consolidation_has_no_new_source_coverage() {
    let mut prior = prior();
    prior.continuation = "界\\\" unfinished directive ".repeat(500);
    let sources = "Complete chronological source fragment, including its ending.";
    let prompt = section_update_prompt(
        &prior,
        sources,
        &BTreeSet::from([2]),
        &BTreeSet::new(),
        policy(),
    )
    .unwrap();
    let parsed_carry = serde_json::to_string(&prior).unwrap();
    assert!(prompt.contains(&parsed_carry));
    assert!(prompt.ends_with(sources));
    // Exercise the existing fake/control parser's wire delimiters against generated output.
    let (instructions, evidence) = prompt.split_once("Prior structured synthesis: ").unwrap();
    assert!(instructions.contains("Eligible new user sources:"));
    assert!(instructions.contains("Eligible unfinished-source reconciliations:"));
    let (carry, fragments) = evidence
        .split_once("\n\nSource fragments (JSON lines):\n")
        .unwrap();
    let decoded: CompactionEvidence = serde_json::from_str(carry).unwrap();
    assert_eq!(decoded.continuation, prior.continuation);
    assert_eq!(carry, parsed_carry);
    assert_eq!(fragments, sources);
    let consolidation = consolidation_prompt(&prior, policy()).unwrap();
    assert!(consolidation.ends_with(&parsed_carry));
    assert!(!consolidation.contains(sources));
    assert!(!consolidation.contains("Source fragments (JSON lines):"));
    assert_eq!(
        section_update_prompt(
            &prior,
            sources,
            &BTreeSet::new(),
            &BTreeSet::from([2]),
            policy()
        )
        .unwrap_err(),
        Rejection::InvalidReconciliation
    );
}

#[test]
fn exact_supersession_keeps_independent_same_source_restriction() {
    let prior = prior();
    let mut value = update(&serde_json::json!({"decisions": "Use approved offline eval"}));
    value["current_scope"] = serde_json::json!([scope(2, "Offline eval approved")]);
    value["supersedes_statements"] = serde_json::json!([{
        "source_sequence": 1, "statement": "No live eval", "replacement_source_sequence": 2,
        "replacement_statement": "Offline eval approved",
    }]);
    let accepted = merge_update(&value, &prior).unwrap();
    assert_eq!(accepted.current_scope.len(), 2);
    assert_eq!(accepted.current_scope[0].statement, "No daemon control");
    assert_eq!(accepted.current_scope[1].statement, "Offline eval approved");
    // Combining exact and sequence-wide replacement cannot bypass the sibling protection.
    let mut mixed = value.clone();
    mixed["current_scope"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "source_sequence": 2, "statement": "Also erase siblings", "supersedes": [1],
        }));
    assert_eq!(
        merge_update(&mixed, &prior).unwrap_err(),
        Rejection::AmbiguousGroupSupersession
    );
    // An absent exact replacement and duplicate targets both fail closed.
    value["supersedes_statements"][0]["replacement_statement"] = "Invented approval".into();
    assert_eq!(
        merge_update(&value, &prior).unwrap_err(),
        Rejection::InvalidSupersession
    );
    value["supersedes_statements"][0]["replacement_statement"] = "Offline eval approved".into();
    value["supersedes_statements"] = serde_json::json!([
        value["supersedes_statements"][0],
        value["supersedes_statements"][0],
    ]);
    assert_eq!(
        merge_update(&value, &prior).unwrap_err(),
        Rejection::InvalidSupersession
    );
}

#[test]
fn exact_targets_reject_unknown_statement_non_user_replacement_and_non_newer_source() {
    let prior = prior();
    let mut value = update(&serde_json::json!({}));
    value["current_scope"] = serde_json::json!([scope(2, "Offline eval approved")]);
    value["supersedes_statements"] = serde_json::json!([{
        "source_sequence": 1, "statement": "No live eval", "replacement_source_sequence": 2,
        "replacement_statement": "Offline eval approved",
    }]);
    for (field, bad) in [
        (
            "statement",
            serde_json::json!("No live eval, or any unrelated work"),
        ),
        ("source_sequence", serde_json::json!(99)),
        ("replacement_source_sequence", serde_json::json!(1)),
        ("replacement_source_sequence", serde_json::json!(99)),
    ] {
        let mut invalid = value.clone();
        invalid["supersedes_statements"][0][field] = bad;
        assert_eq!(
            merge_update(&invalid, &prior).unwrap_err(),
            Rejection::InvalidSupersession
        );
    }
    // Even a matching scope object cannot establish authorization from an ineligible source.
    let mut invalid = value;
    invalid["current_scope"][0]["source_sequence"] = 99.into();
    invalid["supersedes_statements"][0]["replacement_source_sequence"] = 99.into();
    assert_eq!(
        merge_update(&invalid, &prior).unwrap_err(),
        Rejection::InvalidSupersession
    );
}

#[test]
fn unfinished_reconciliation_requires_replacement_and_preserves_unrelated_sources() {
    let mut prior = prior();
    prior
        .current_scope
        .push(scope(2, "Provisional unfinished directive"));
    let mut value =
        update(&serde_json::json!({"active_work_blockers": "Final directive fragment received"}));
    value["reconciles"] = serde_json::json!([2]);
    let merge = |value: &serde_json::Value| {
        super::super::merge_response_with_policy(
            &value.to_string(),
            &prior,
            &BTreeSet::from([1, 2]),
            &BTreeSet::from([2]),
            &BTreeSet::from([2]),
            policy(),
        )
    };
    assert_eq!(merge(&value).unwrap_err(), Rejection::InvalidReconciliation);
    value["current_scope"] = serde_json::json!([scope(2, "Corrected complete directive")]);
    let accepted = merge(&value).unwrap();
    assert_eq!(accepted.current_scope.len(), 3);
    assert_eq!(accepted.current_scope[0].statement, "No live eval");
    assert_eq!(accepted.current_scope[1].statement, "No daemon control");
    assert_eq!(
        accepted.current_scope[2].statement,
        "Corrected complete directive"
    );
}

#[test]
fn sequence_wide_supersession_stays_ambiguous() {
    let mut value = update(&serde_json::json!({}));
    value["current_scope"] = serde_json::json!([{
        "source_sequence": 2, "statement": "Changed", "supersedes": [1],
    }]);
    assert_eq!(
        merge_update(&value, &prior()).unwrap_err(),
        Rejection::AmbiguousGroupSupersession
    );
}

#[test]
fn capacity_policy_accepts_carry_above_old_fixed_ceiling_without_clipping() {
    let mut prior = prior();
    prior.continuation = "界\\\"".repeat(2100);
    let accepted = merge_update(
        &update(&serde_json::json!({"decisions": "A decision"})),
        &prior,
    )
    .unwrap();
    assert!(serde_json::to_string(&accepted).unwrap().chars().count() > 6000);
    assert_eq!(
        Sections::from_prior(&accepted.continuation)
            .unwrap()
            .background,
        prior.continuation
    );
    let bounded = SynthesisPolicy {
        carry_chars: 6000,
        ..policy()
    };
    assert_eq!(
        super::super::merge_response_with_policy(
            &update(&serde_json::json!({})).to_string(),
            &prior,
            &BTreeSet::from([1]),
            &BTreeSet::new(),
            &BTreeSet::new(),
            bounded,
        )
        .unwrap_err(),
        Rejection::CarryBudget
    );
}

#[test]
fn consolidation_is_scope_immutable_and_separate_from_new_source_acceptance() {
    let mut prior = prior();
    prior.continuation = "Long derived evidence. ".repeat(100);
    let response = serde_json::json!({
        "format": CONSOLIDATION_FORMAT, "sections": {"background": "All 100 checks passed; follow-up remains."},
    }).to_string();
    let accepted = consolidate_response(&response, &prior, policy()).unwrap();
    assert_eq!(
        serde_json::to_value(&accepted.current_scope).unwrap(),
        serde_json::to_value(&prior.current_scope).unwrap()
    );
    // The consolidation schema has no source coverage or scope replacement fields.
    let mut invalid: serde_json::Value = serde_json::from_str(&response).unwrap();
    invalid["current_scope"] = serde_json::json!([]);
    assert_eq!(
        consolidate_response(&invalid.to_string(), &prior, policy()).unwrap_err(),
        Rejection::UnknownField
    );
    assert_eq!(
        merge_update(&serde_json::from_str(&response).unwrap(), &prior).unwrap_err(),
        Rejection::InvalidShape
    );
}

#[test]
fn sections_and_wire_limits_are_independent_and_unknown_versions_fail_closed() {
    let prior = prior();
    let value = update(&serde_json::json!({"background": "oversized"})).to_string();
    for (policy, expected) in [
        (
            SynthesisPolicy {
                wire_bytes: 1,
                ..policy()
            },
            Rejection::WireBudget,
        ),
        (
            SynthesisPolicy {
                section_chars: 2,
                ..policy()
            },
            Rejection::CarryBudget,
        ),
    ] {
        assert_eq!(
            super::super::merge_response_with_policy(
                &value,
                &prior,
                &BTreeSet::from([1]),
                &BTreeSet::new(),
                &BTreeSet::new(),
                policy,
            )
            .unwrap_err(),
            expected
        );
    }
    let mut prior = prior;
    prior.continuation =
        serde_json::json!({"format": "bcode.compaction.continuation-sections.v99"}).to_string();
    assert_eq!(
        merge_update(&update(&serde_json::json!({})), &prior).unwrap_err(),
        Rejection::UnsupportedFormat
    );
}
