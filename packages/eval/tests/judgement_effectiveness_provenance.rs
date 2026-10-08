//! Scenario manifest mapping and the declaration/proof boundary.
use bcode_eval::judgement_benchmark::effectiveness::{
    DatasetSplit, EffectivenessEvidenceScope, EffectivenessPlan, compare_effectiveness,
};

#[test]
fn planned_scenario_manifest_is_not_an_execution_capture_or_plan() {
    // Shape from context_scenarios.equivalent_pairs: these are declarations.
    let manifest = serde_json::json!({
        "schema_version": 1,
        "corpus_digest": "frozen-development-corpus",
        "oracle_digest": "independent-oracle-source",
        "scenario_family": "ordinary_coding",
        "evidence_scope": "planned_not_executed",
        "pairs": [{
            "pair_id": "retry-window-v1-r0", "scenario_id": "retry-window-v1",
            "repetition": 0, "source_order": ["task-policy", "contract-current", "contract-archive", "observability"],
            "shared_input_digest": "declared-shared-input",
            "selection": {"model": "declared-model"}, "allowance": {"context_bytes": 256},
            "arms": ["off", "on"]
        }]
    });
    assert!(serde_json::from_value::<EffectivenessPlan>(manifest).is_err());
}

#[test]
fn empty_development_schedule_never_claims_verified_consumer_evidence() {
    let plan = EffectivenessPlan {
        schema_version: 1,
        preregistration_id: "development-registration".into(),
        dataset_id: "context-coding-development-v1".into(),
        split: DatasetSplit::Development,
        tasks: Vec::new(),
        minimum_families: 5,
        minimum_gain: 0.1,
        maximum_extra_cost_usd: 0.02,
        maximum_extra_latency_ms: 100.0,
    };
    let report = compare_effectiveness(&plan, &[]).unwrap();
    assert_eq!(report.schema_version, 2);
    assert_eq!(
        report.evidence_scope,
        EffectivenessEvidenceScope::CallerDeclarationsOnly
    );
    assert!(
        report
            .features
            .iter()
            .all(|feature| !feature.promotion_eligible)
    );
}
