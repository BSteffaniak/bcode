//! Explicit corpus conversion. No graded relevance or event timing is inferred from prose.
use super::{BenchmarkPlan, BenchmarkTask, Gates, Oracle};
use bcode_model::judgement::Feature;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    split: String,
    truth_provenance: String,
    cases: Vec<Case>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    feature: Feature,
    #[serde(rename = "control")]
    _control: String,
    #[serde(rename = "input")]
    _input: serde_json::Value,
    expected: Expected,
    #[serde(default)]
    oracle: Option<Oracle>,
    #[serde(rename = "rationale")]
    _rationale: String,
    #[serde(rename = "task")]
    _task: String,
    #[serde(rename = "files")]
    _files: BTreeMap<String, String>,
    #[serde(rename = "answer")]
    _answer: serde_json::Value,
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Expected {
    Labels {
        values: BTreeMap<String, String>,
    },
    Absent,
    Ranking {
        prefix: Vec<String>,
        all_ids: Vec<String>,
    },
}

fn label(feature: Feature, values: &BTreeMap<String, String>) -> Option<String> {
    let key = match feature {
        Feature::ToolResultTriage => "category",
        Feature::EvalCategorization => "failure_category",
        Feature::ProgressMonitoring => "state",
        Feature::SessionCategorization if values.len() == 2 => {
            return Some(format!("{}/{}", values.get("task")?, values.get("domain")?));
        }
        _ => return None,
    };
    (values.len() == 1)
        .then(|| values.get(key).cloned())
        .flatten()
}

/// Convert bounded corpus JSON to the shared plan without changing its oracle semantics.
///
/// The immutable oracle ID must identify external review/freeze status honestly; authored
/// synthetic truth is not relabeled as independently collected evidence.
///
/// Cases may embed the exact typed `oracle`; `supplements` is an alternative keyed by case
/// ID, not an override. Prefix-only ranking and untimed progress require independently
/// authored graded/admission and onset/deadline truth. Duplicate sources are rejected.
/// Caller owns gates, provenance and schedule; no fixture inputs/answers enter the scorer plan.
///
/// # Errors
/// Rejects oversized (>1 MiB), unknown/version-incompatible corpus data, malformed labels,
/// duplicate IDs, unknown supplements, or missing/incompatible oracle supplements. Error
/// strings identify the mapping gap without reproducing task content or secrets.
pub fn parse_corpus_plan(
    json: &str,
    oracle_id: String,
    preregistration_id: String,
    repetitions: u32,
    gates: Gates,
    supplements: &BTreeMap<String, Oracle>,
) -> Result<BenchmarkPlan, String> {
    if json.len() > 1024 * 1024 {
        return Err("corpus exceeds bounded conversion input".into());
    }
    let mut source: serde_json::Value =
        serde_json::from_str(json).map_err(|_| "invalid corpus schema")?;
    let version = source["schema_version"]
        .as_u64()
        .ok_or("missing corpus version")?;
    if !matches!(version, 1 | 3) {
        return Err("unsupported corpus version".into());
    }
    if version == 3 {
        source["schema_version"] = serde_json::json!(1);
        for case in source["cases"]
            .as_array_mut()
            .ok_or("invalid corpus cases")?
        {
            let object = case.as_object_mut().ok_or("invalid corpus case")?;
            if object.remove("family_id").is_none() || object.remove("fault").is_none() {
                return Err("v3 corpus provenance missing".into());
            }
            let expected = object.get_mut("expected").ok_or("missing expected truth")?;
            match expected["kind"].as_str() {
                Some("ranking") => {
                    let allowed = [
                        "kind",
                        "prefix",
                        "all_ids",
                        "relevance",
                        "costs",
                        "budget",
                        "k",
                        "required",
                        "protected_positions",
                        "ideal_admission_gain",
                    ];
                    let fields = expected.as_object().ok_or("invalid ranking truth")?;
                    if fields.len() != allowed.len()
                        || fields.keys().any(|key| !allowed.contains(&key.as_str()))
                    {
                        return Err("unsupported v3 ranking fields".into());
                    }
                    let mut oracle = expected.clone();
                    let fields = oracle.as_object_mut().ok_or("invalid ranking truth")?;
                    fields.remove("prefix");
                    fields.remove("all_ids");
                    let ids = expected["all_ids"].clone();
                    *expected = serde_json::json!({"kind":"ranking","prefix":[],"all_ids":ids});
                    object.insert("oracle".into(), oracle);
                }
                Some("progress") => {
                    let oracle = expected.clone();
                    *expected = match oracle["label"].as_str() {
                        Some(label) => {
                            serde_json::json!({"kind":"labels","values":{"state":label}})
                        }
                        None if oracle["label"].is_null() => serde_json::json!({"kind":"absent"}),
                        _ => return Err("invalid progress label".into()),
                    };
                    object.insert("oracle".into(), oracle);
                }
                Some("labels" | "absent") => {}
                _ => return Err("unsupported expected truth".into()),
            }
        }
    }
    let corpus: Corpus = serde_json::from_value(source).map_err(|_| "invalid corpus schema")?;
    if corpus.schema_version != 1
        || !matches!(
            corpus.split.as_str(),
            "development" | "held_out" | "held-out" | "independent_public"
        )
        || corpus.truth_provenance != "authored_synthetic"
    {
        return Err("unsupported corpus version, split or truth provenance".into());
    }
    let mut tasks = Vec::new();
    let mut ids = std::collections::BTreeSet::new();
    for case in corpus.cases {
        if !ids.insert(case.id.clone()) {
            return Err("duplicate corpus case identity".into());
        }
        if case.oracle.is_some() && supplements.contains_key(&case.id) {
            return Err(format!("{}: duplicate embedded/supplement oracle", case.id));
        }
        let supplied = case.oracle.as_ref().or_else(|| supplements.get(&case.id));
        let oracle = match (&case.expected, case.feature) {
            (
                Expected::Labels { values },
                Feature::ToolResultTriage
                | Feature::SessionCategorization
                | Feature::EvalCategorization,
            ) => {
                let expected = label(case.feature, values)
                    .ok_or_else(|| format!("{}: classification label mapping mismatch", case.id))?;
                Oracle::Classification {
                    labels: BTreeMap::from([(expected, 1.0)]),
                    abstention_credit: 0.0,
                }
            }
            (
                Expected::Absent,
                Feature::ToolResultTriage
                | Feature::SessionCategorization
                | Feature::EvalCategorization,
            ) => {
                // Empty string is never a valid captured label; a vocabulary placeholder allows
                // all non-abstentions to score zero without inventing a correct positive label.
                Oracle::Classification {
                    labels: BTreeMap::from([("__negative_control__".into(), 0.0)]),
                    abstention_credit: 1.0,
                }
            }
            (
                Expected::Ranking { prefix, all_ids },
                Feature::ContextReranking | Feature::Recommendations,
            ) => {
                let Some(oracle @ Oracle::Ranking { relevance, k, .. }) = supplied else {
                    return Err(format!(
                        "{}: ranking lacks graded relevance/cost/budget/admission oracle",
                        case.id
                    ));
                };
                let ids: std::collections::BTreeSet<_> = all_ids.iter().collect();
                if ids.len() != all_ids.len()
                    || relevance.keys().collect::<std::collections::BTreeSet<_>>() != ids
                    || *k < prefix.len()
                    || prefix.iter().any(|id| !relevance.contains_key(id))
                {
                    return Err(format!(
                        "{}: ranking supplement candidate/prefix mismatch",
                        case.id
                    ));
                }
                let mut previous = f64::INFINITY;
                for id in prefix {
                    if relevance[id] > previous {
                        return Err(format!(
                            "{}: ranking supplement contradicts prefix",
                            case.id
                        ));
                    }
                    previous = relevance[id];
                }
                if relevance
                    .iter()
                    .any(|(id, value)| !prefix.contains(id) && *value > previous)
                {
                    return Err(format!(
                        "{}: ranking supplement contradicts prefix",
                        case.id
                    ));
                }
                oracle.clone()
            }
            (expected, Feature::ProgressMonitoring) => {
                let Some(
                    oracle @ Oracle::Progress {
                        label: supplied, ..
                    },
                ) = supplied
                else {
                    return Err(format!(
                        "{}: progress lacks independent onset/deadline oracle",
                        case.id
                    ));
                };
                let required = match expected {
                    Expected::Absent => None,
                    Expected::Labels { values } => {
                        Some(label(case.feature, values).ok_or("progress label mapping mismatch")?)
                    }
                    Expected::Ranking { .. } => {
                        return Err("progress cannot use ranking truth".into());
                    }
                };
                if *supplied != required {
                    return Err(format!("{}: progress supplement label mismatch", case.id));
                }
                oracle.clone()
            }
            _ => return Err(format!("{}: unsupported feature oracle mapping", case.id)),
        };
        if supplied.is_some() && matches!(oracle, Oracle::Classification { .. }) {
            return Err(format!(
                "{}: classification supplement would replace exact fixture truth",
                case.id
            ));
        }
        tasks.push(BenchmarkTask {
            task_id: case.id,
            feature: case.feature,
            repetitions,
            oracle,
        });
    }
    if supplements.keys().any(|id| !ids.contains(id)) {
        return Err("unknown oracle supplement identity".into());
    }
    let plan = BenchmarkPlan {
        schema_version: 1,
        oracle_id,
        preregistration_id,
        tasks,
        gates,
    };
    super::comparison::validate_plan(&plan)?;
    Ok(plan)
}
