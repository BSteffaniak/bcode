//! Offline comparative quality scoring of captured consumer observations.
//!
//! Fixtures are ground truth, never provider responses. This module does not invoke models,
//! replay session history, persist request input, or certify application integration.

use bcode_model::judgement::Feature;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Representative acceptance inputs, loaded only when explicitly requested by a harness.
pub const FIXTURE_JSON: &str = include_str!("../fixtures/automatic-quality-v1.json");

/// Versioned fixture boundary. Unknown fields and versions are rejected.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualitySuite {
    /// Fixture format version.
    pub schema_version: u32,
    /// Independent caller-labeled cases.
    pub cases: Vec<QualityCase>,
}

/// One representative input and its independent expected semantic result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityCase {
    /// Stable identity used to pair arms.
    pub id: String,
    /// Actual consumer being evaluated.
    pub feature: Feature,
    /// Request-only synthetic acceptance input; not a provider answer.
    pub input: serde_json::Value,
    /// Independent ground truth.
    pub expected: Expected,
    /// A control where an unsupported diagnosis or reordering is a false positive.
    pub negative_control: bool,
}

/// Caller-owned semantic ground truth; ranking prefixes leave remaining ties unconstrained.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Expected {
    /// Insufficient signal intentionally suppresses an advisory call/result.
    Absent,
    /// Exact allowlisted labels, including explicit uncertainty.
    Labels {
        /// Expected semantic dimensions.
        values: BTreeMap<String, String>,
    },
    /// A complete candidate permutation with a ground-truth relevant prefix.
    Ranking {
        /// Every original candidate, including optional irrelevant candidates.
        candidates: Vec<String>,
        /// Ordered relevant prefix; all other candidates may appear in any order.
        prefix: Vec<String>,
    },
}

/// Execution arm; fallback arms must preserve the captured global-off baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityArm {
    /// All optional features disabled before discovery.
    GlobalOff,
    /// Only this consumer disabled before discovery.
    FeatureOff,
    /// Auto with an eligible verified offline test credential.
    AutoEligible,
    /// Auto with no eligible credential.
    AutoNoKey,
    /// Auto with a deterministic provider failure.
    AutoError,
}

const ARMS: [QualityArm; 5] = [
    QualityArm::GlobalOff,
    QualityArm::FeatureOff,
    QualityArm::AutoEligible,
    QualityArm::AutoNoKey,
    QualityArm::AutoError,
];

/// Actual normalized consumer output. Absence is a baseline, not an invented category.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum QualityOutput {
    /// Consumer emitted no advisory result.
    Absent,
    /// Captured semantic labels, not raw model output.
    Labels {
        /// Actual allowlisted dimensions.
        values: BTreeMap<String, String>,
    },
    /// Captured candidate ordering after the real consumer applied its policy.
    Ranking {
        /// Actual full ordering, including protected entries.
        ids: Vec<String>,
    },
}

/// Observation provenance. Synthetic output exercises scoring but cannot prove improvement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityPath {
    /// Public application consumer path, using an offline provider.
    ApplicationConsumer,
    /// Domain consumer execution only; not evidence of application hookup.
    DomainConsumer,
    /// Scorer unit-test data; never product evidence.
    ScorerFixture,
}

/// Provider operations measured by the harness, not estimated from final output.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityCalls {
    /// Actual discovery operations.
    pub discovery: u64,
    /// Actual verification operations.
    pub verification: u64,
    /// Actual inference operations.
    pub inference: u64,
}

/// One captured result. This contains no fixture inputs, credentials, or provider metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityObservation {
    /// Fixture identity.
    pub case_id: String,
    /// Configuration/credential arm exercised by the real harness.
    pub arm: QualityArm,
    /// Executed consumer boundary.
    pub path: QualityPath,
    /// Bounded public operation or test identifier; never a raw diagnostic or secret.
    pub operation: String,
    /// Final consumer output, not a provider answer.
    pub output: QualityOutput,
    /// Actual centrally routed optional-provider operations.
    pub calls: QualityCalls,
    /// Measured canonical event/permission/terminal authority preservation.
    pub authority_preserved: bool,
    /// Measured absence of implicit fixture input persistence.
    pub request_only: bool,
}

/// Per-observation measurements, kept independent of advisory categories.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityMeasurement {
    /// Fixture identity.
    pub case_id: String,
    /// Executed arm.
    pub arm: QualityArm,
    /// Exact label accuracy or relevant-prefix position accuracy, in [0, 1].
    pub accuracy: f64,
    /// Incorrect advisory result on a negative control (absence is not a false positive).
    pub false_positive: bool,
    /// Safety/opt-out/fallback constraints verified against captured facts.
    pub constraints_passed: bool,
}

/// Report contains measurements, never an asserted or mocked improvement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityReport {
    /// Report compatibility version.
    pub schema_version: u32,
    /// All supplied observations scored, including non-product evidence.
    pub measurements: Vec<QualityMeasurement>,
    /// Product-path off/auto pairs with all five arms supplied and constraints satisfied.
    pub eligible_pairs: usize,
    /// Mean eligible auto-minus-off accuracy. None means no product comparison available.
    pub mean_accuracy_delta: Option<f64>,
    /// Missing arms, non-application paths, and violated constraints block acceptance.
    pub gaps: Vec<String>,
}

/// Load the bundled independent acceptance inputs.
///
/// # Errors
/// Returns a decoding error for unsupported fixture representation.
pub fn representative_quality_suite() -> Result<QualitySuite, serde_json::Error> {
    serde_json::from_str(FIXTURE_JSON)
}

fn accuracy(expected: &Expected, output: &QualityOutput) -> f64 {
    match (expected, output) {
        (Expected::Absent, QualityOutput::Absent) => 1.0,
        (Expected::Labels { values: expected }, QualityOutput::Labels { values }) => {
            if expected.is_empty() || values.keys().ne(expected.keys()) {
                return 0.0;
            }
            let correct = expected
                .iter()
                .filter(|(key, value)| values.get(*key) == Some(*value))
                .count();
            correct as f64 / expected.len() as f64
        }
        (Expected::Ranking { candidates, prefix }, QualityOutput::Ranking { ids }) => {
            let actual: BTreeSet<_> = ids.iter().collect();
            let original: BTreeSet<_> = candidates.iter().collect();
            if ids.len() != candidates.len()
                || actual.len() != ids.len()
                || actual != original
                || prefix.is_empty()
            {
                return 0.0;
            }
            let correct = prefix
                .iter()
                .zip(ids)
                .filter(|(expected, actual)| expected == actual)
                .count();
            correct as f64 / prefix.len() as f64
        }
        _ => 0.0,
    }
}

/// Score actual off/auto observations without executing or persisting fixture requests.
/// Missing observations yield explicit gaps, not fabricated baseline or auto scores.
///
/// # Errors
/// Rejects unknown versions, duplicate/unknown identities, invalid ground truth, or empty locators.
pub fn compare_quality(
    suite: &QualitySuite,
    observations: &[QualityObservation],
) -> Result<QualityReport, String> {
    if suite.schema_version != 1 || suite.cases.is_empty() {
        return Err("unsupported or empty quality suite".to_owned());
    }
    let mut cases = BTreeMap::new();
    for case in &suite.cases {
        if case.id.is_empty() || cases.insert(case.id.as_str(), case).is_some() {
            return Err("duplicate or empty quality case".to_owned());
        }
        match &case.expected {
            Expected::Labels { values } if values.is_empty() => {
                return Err("empty label ground truth".to_owned());
            }
            Expected::Ranking { candidates, prefix } => {
                let unique: BTreeSet<_> = candidates.iter().collect();
                let relevant: BTreeSet<_> = prefix.iter().collect();
                if candidates.is_empty()
                    || unique.len() != candidates.len()
                    || prefix.is_empty()
                    || relevant.len() != prefix.len()
                    || !relevant.is_subset(&unique)
                {
                    return Err("invalid ranking ground truth".to_owned());
                }
            }
            _ => {}
        }
    }
    let mut captured = BTreeMap::new();
    for observation in observations {
        if !cases.contains_key(observation.case_id.as_str())
            || observation.operation.trim().is_empty()
            || observation.operation.len() > 256
            || captured
                .insert((observation.case_id.as_str(), observation.arm), observation)
                .is_some()
        {
            return Err("unknown/duplicate observation or invalid operation locator".to_owned());
        }
    }
    let mut report = QualityReport {
        schema_version: 1,
        measurements: Vec::new(),
        eligible_pairs: 0,
        mean_accuracy_delta: None,
        gaps: Vec::new(),
    };
    let mut delta = 0.0;
    for case in &suite.cases {
        let baseline = captured.get(&(case.id.as_str(), QualityArm::GlobalOff));
        let mut eligible = true;
        let mut scores = BTreeMap::new();
        for arm in ARMS {
            let Some(observation) = captured.get(&(case.id.as_str(), arm)) else {
                report.gaps.push(format!("{}: missing {arm:?}", case.id));
                eligible = false;
                continue;
            };
            let off = matches!(arm, QualityArm::GlobalOff | QualityArm::FeatureOff);
            let zero_calls = observation.calls.discovery == 0
                && observation.calls.verification == 0
                && observation.calls.inference == 0;
            let fallback = arm != QualityArm::AutoEligible;
            let preserved =
                !fallback || baseline.is_some_and(|baseline| baseline.output == observation.output);
            let constraints = observation.authority_preserved
                && observation.request_only
                && (!off || zero_calls)
                && (arm != QualityArm::AutoNoKey || observation.calls.inference == 0)
                && (arm != QualityArm::AutoEligible
                    || matches!(case.expected, Expected::Absent)
                    || observation.calls.inference > 0)
                && preserved;
            if !constraints {
                report.gaps.push(format!(
                    "{}: {arm:?} violates captured safety/opt-out/fallback constraints",
                    case.id
                ));
                eligible = false;
            }
            if observation.path != QualityPath::ApplicationConsumer {
                report.gaps.push(format!(
                    "{}: {arm:?} lacks application consumer evidence",
                    case.id
                ));
                eligible = false;
            }
            let score = accuracy(&case.expected, &observation.output);
            scores.insert(arm, score);
            report.measurements.push(QualityMeasurement {
                case_id: case.id.clone(),
                arm,
                accuracy: score,
                false_positive: case.negative_control
                    && score < 1.0
                    && observation.output != QualityOutput::Absent,
                constraints_passed: constraints,
            });
        }
        if eligible {
            report.eligible_pairs += 1;
            delta += scores[&QualityArm::AutoEligible] - scores[&QualityArm::GlobalOff];
        }
    }
    if report.eligible_pairs > 0 {
        report.mean_accuracy_delta = Some(delta / report.eligible_pairs as f64);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_consumers_cannot_claim_improvement() {
        let suite = representative_quality_suite().unwrap();
        assert_eq!(suite.cases.len(), 18);
        let report = compare_quality(&suite, &[]).unwrap();
        assert_eq!(report.eligible_pairs, 0);
        assert!(report.mean_accuracy_delta.is_none());
        assert_eq!(report.gaps.len(), suite.cases.len() * ARMS.len());
        for feature in [
            Feature::ToolResultTriage,
            Feature::ContextReranking,
            Feature::ProgressMonitoring,
            Feature::Recommendations,
            Feature::SessionCategorization,
            Feature::EvalCategorization,
        ] {
            assert!(
                suite
                    .cases
                    .iter()
                    .any(|case| case.feature == feature && case.negative_control)
            );
            assert!(
                suite
                    .cases
                    .iter()
                    .any(|case| case.feature == feature && !case.negative_control)
            );
        }
    }

    #[test]
    fn ranking_scores_actual_full_permutation_not_provider_claims() {
        let expected = Expected::Ranking {
            candidates: vec!["a".into(), "b".into(), "c".into()],
            prefix: vec!["b".into(), "c".into()],
        };
        assert!(
            (accuracy(
                &expected,
                &QualityOutput::Ranking {
                    ids: vec!["b".into(), "c".into(), "a".into()]
                }
            ) - 1.0)
                .abs()
                < f64::EPSILON
        );
        assert!(
            (accuracy(
                &expected,
                &QualityOutput::Ranking {
                    ids: vec!["b".into(), "a".into(), "c".into()]
                }
            ) - 0.5)
                .abs()
                < f64::EPSILON
        );
        assert!(
            accuracy(
                &expected,
                &QualityOutput::Ranking {
                    ids: vec!["b".into(), "b".into(), "a".into()]
                }
            )
            .abs()
                < f64::EPSILON
        );
    }

    fn observation(case: &QualityCase, arm: QualityArm) -> QualityObservation {
        QualityObservation {
            case_id: case.id.clone(),
            arm,
            path: QualityPath::ScorerFixture,
            operation: "scorer-unit-test".into(),
            output: QualityOutput::Absent,
            calls: QualityCalls {
                inference: u64::from(arm == QualityArm::AutoEligible),
                ..QualityCalls::default()
            },
            authority_preserved: true,
            request_only: true,
        }
    }

    #[test]
    fn synthetic_answers_and_off_calls_cannot_pass_product_acceptance() {
        let mut suite = representative_quality_suite().unwrap();
        suite.cases.truncate(1);
        let case = &suite.cases[0];
        let mut observations: Vec<_> = ARMS.into_iter().map(|arm| observation(case, arm)).collect();
        let Expected::Labels { values } = &case.expected else {
            panic!("expected label case")
        };
        observations[2].output = QualityOutput::Labels {
            values: values.clone(),
        };
        let report = compare_quality(&suite, &observations).unwrap();
        assert!((report.measurements[2].accuracy - 1.0).abs() < f64::EPSILON);
        assert!(report.mean_accuracy_delta.is_none());
        observations[0].calls.discovery = 1;
        let report = compare_quality(&suite, &observations).unwrap();
        assert!(!report.measurements[0].constraints_passed);
    }

    #[test]
    fn fallback_must_match_actual_baseline() {
        let mut suite = representative_quality_suite().unwrap();
        suite.cases.truncate(1);
        let case = &suite.cases[0];
        let mut observations: Vec<_> = ARMS.into_iter().map(|arm| observation(case, arm)).collect();
        observations[4].output = QualityOutput::Labels {
            values: BTreeMap::from([("category".into(), "uncertain".into())]),
        };
        let report = compare_quality(&suite, &observations).unwrap();
        assert!(!report.measurements[4].constraints_passed);
    }
}
