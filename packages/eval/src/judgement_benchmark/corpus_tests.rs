//! All-six conversion calibration using explicit fixture-contract oracles, not inferred truth.
use super::*;
use std::collections::BTreeMap;

#[test]
fn all_six_real_inputs_map_when_fixture_contract_supplies_independent_oracles() {
    let mut corpus: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../fixtures/evals/judgement/held-out.json"
    ))
    .unwrap();
    // Typed calibration oracles are intentionally test-only. Corpus lane must author/review
    // the production oracle fields and refresh freeze digest; these are not live evidence.
    let oracles = BTreeMap::from([
        (
            "held-context-unicode",
            serde_json::json!({"kind":"ranking","relevance":{"bytes":0.0,"points":3.0},"k":1,"costs":{"bytes":1,"points":1},"budget":1,"ideal_admission_gain":7.0,"required":[],"protected_positions":{}}),
        ),
        (
            "held-context-protected",
            serde_json::json!({"kind":"ranking","relevance":{"policy":3.0,"source":2.0,"attack":0.0},"k":2,"costs":{"policy":1,"source":1,"attack":1},"budget":2,"ideal_admission_gain":10.0,"required":["policy"],"protected_positions":{"policy":0}}),
        ),
        (
            "held-recommend-search",
            serde_json::json!({"kind":"ranking","relevance":{"search":3.0,"edit":0.0,"read":0.0},"k":1,"costs":{"search":1,"edit":1,"read":1},"budget":1,"ideal_admission_gain":7.0,"required":[],"protected_positions":{}}),
        ),
        (
            "held-recommend-tie",
            serde_json::json!({"kind":"ranking","relevance":{"reader_a":1.0,"reader_b":1.0},"k":2,"costs":{"reader_a":1,"reader_b":1},"budget":1,"ideal_admission_gain":1.0,"required":[],"protected_positions":{}}),
        ),
        (
            "held-progress-drift",
            serde_json::json!({"kind":"progress","label":"drift","onset_ms":100,"deadline_ms":200}),
        ),
        (
            "held-progress-unknown",
            serde_json::json!({"kind":"progress","label":null,"onset_ms":0,"deadline_ms":200}),
        ),
    ]);
    for case in corpus["cases"].as_array_mut().unwrap() {
        if let Some(oracle) = oracles.get(case["id"].as_str().unwrap()) {
            case["oracle"] = oracle.clone();
        }
    }
    let plan = parse_corpus_plan(
        &corpus.to_string(),
        "test-only-authored-calibration".into(),
        "calibration-v2".into(),
        1,
        super::tests::plan().gates,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(plan.tasks.len(), 12);
    for feature in FEATURES {
        assert!(plan.tasks.iter().any(|task| task.feature == feature));
    }
    assert!(
        compare_benchmark(&plan, &[])
            .unwrap()
            .features
            .iter()
            .all(|report| report.verdict == Verdict::Inconclusive)
    );
}
