//! Independent semantic scorers. No model probabilities are treated as ground truth.
use super::{ConsumerOutput, Oracle, OutputScore};
use std::collections::BTreeSet;

fn gain(relevance: f64) -> f64 {
    relevance.exp2() - 1.0
}
fn dcg(values: impl Iterator<Item = f64>) -> f64 {
    values
        .enumerate()
        .map(|(index, value)| gain(value) / ((index + 2) as f64).log2())
        .sum()
}
const fn score(
    quality: f64,
    constraints_passed: bool,
    detection_delay_ms: Option<u64>,
) -> OutputScore {
    OutputScore {
        quality,
        constraints_passed,
        detection_delay_ms,
    }
}

/// Score a captured consumer output against an independent validated oracle.
/// Invalid structure or unsafe admission scores zero; absent capture should not call this API.
#[must_use]
pub fn score_output(oracle: &Oracle, output: &ConsumerOutput) -> OutputScore {
    if !validate_oracle(oracle) {
        return score(0.0, false, None);
    }
    match (oracle, output) {
        (
            Oracle::Classification {
                labels,
                abstention_credit,
            },
            ConsumerOutput::Classification { label },
        ) => label.as_ref().map_or_else(
            || score(*abstention_credit, true, None),
            |label| {
                labels.get(label).map_or_else(
                    || score(0.0, false, None),
                    |credit| score(*credit, true, None),
                )
            },
        ),
        (
            Oracle::Ranking {
                relevance,
                k,
                costs,
                budget,
                ideal_admission_gain,
                required,
                protected_positions,
            },
            ConsumerOutput::Ranking { order, admitted },
        ) => {
            let unique: BTreeSet<_> = order.iter().collect();
            let selected: BTreeSet<_> = admitted.iter().collect();
            let cost = admitted
                .iter()
                .try_fold(0_u64, |sum, id| sum.checked_add(*costs.get(id)?));
            let valid = unique.len() == order.len()
                && order.len() == relevance.len()
                && order.iter().all(|id| relevance.contains_key(id))
                && selected.len() == admitted.len()
                && admitted.iter().all(|id| relevance.contains_key(id))
                && required.iter().all(|id| selected.contains(id))
                && protected_positions
                    .iter()
                    .all(|(id, position)| order.get(*position) == Some(id))
                && cost.is_some_and(|value| value <= *budget);
            if !valid {
                return score(0.0, false, None);
            }
            let mut ideal: Vec<_> = relevance.values().copied().collect();
            ideal.sort_by(|a, b| b.total_cmp(a));
            let denominator = dcg(ideal.into_iter().take(*k));
            let ranking = if denominator > 0.0 {
                dcg(order.iter().take(*k).map(|id| relevance[id])) / denominator
            } else {
                1.0
            };
            let admission_gain: f64 = admitted.iter().map(|id| gain(relevance[id])).sum();
            // Incorrect oracle calibration must never manufacture >1 utility.
            if admission_gain > *ideal_admission_gain + 1e-9 {
                return score(0.0, false, None);
            }
            let admission = if *ideal_admission_gain > 0.0 {
                admission_gain / ideal_admission_gain
            } else {
                1.0
            };
            score(ranking.midpoint(admission), true, None)
        }
        (
            Oracle::Progress {
                label: expected,
                onset_ms,
                deadline_ms,
            },
            ConsumerOutput::Progress { label, detected_ms },
        ) => {
            if label.is_none() {
                return score(
                    if expected.is_none() && detected_ms.is_none() {
                        1.0
                    } else {
                        0.0
                    },
                    detected_ms.is_none(),
                    None,
                );
            }
            if label != expected
                || detected_ms.is_none_or(|time| time < *onset_ms || time > *deadline_ms)
            {
                return score(0.0, false, None);
            }
            let delay = detected_ms.unwrap_or(*onset_ms) - onset_ms;
            let window = deadline_ms - onset_ms;
            let quality = if window == 0 {
                1.0
            } else {
                1.0 - delay as f64 / window as f64
            };
            score(quality, true, Some(delay))
        }
        _ => score(0.0, false, None),
    }
}

pub(super) fn validate_oracle(oracle: &Oracle) -> bool {
    let unit = |value: f64| value.is_finite() && (0.0..=1.0).contains(&value);
    match oracle {
        Oracle::Classification {
            labels,
            abstention_credit,
        } => {
            !labels.is_empty()
                && labels.keys().all(|id| !id.is_empty())
                && labels.values().all(|value| unit(*value))
                && unit(*abstention_credit)
        }
        Oracle::Ranking {
            relevance,
            k,
            costs,
            budget,
            ideal_admission_gain,
            required,
            protected_positions,
        } => {
            let required_unique: BTreeSet<_> = required.iter().collect();
            let positions: BTreeSet<_> = protected_positions.values().collect();
            let required_cost = required
                .iter()
                .try_fold(0_u64, |sum, id| sum.checked_add(*costs.get(id)?));
            !relevance.is_empty()
                && *k > 0
                && *k <= relevance.len()
                && relevance.keys().eq(costs.keys())
                && relevance.keys().all(|id| !id.is_empty())
                && relevance
                    .values()
                    .all(|value| value.is_finite() && (0.0..=4.0).contains(value))
                && ideal_admission_gain.is_finite()
                && *ideal_admission_gain >= 0.0
                && *ideal_admission_gain
                    <= relevance.values().map(|value| gain(*value)).sum::<f64>()
                && required_unique.len() == required.len()
                && required.iter().all(|id| relevance.contains_key(id))
                && required_cost.is_some_and(|value| value <= *budget)
                && required.iter().map(|id| gain(relevance[id])).sum::<f64>()
                    <= *ideal_admission_gain
                && positions.len() == protected_positions.len()
                && protected_positions
                    .iter()
                    .all(|(id, position)| relevance.contains_key(id) && *position < relevance.len())
        }
        Oracle::Progress {
            label,
            onset_ms,
            deadline_ms,
        } => onset_ms <= deadline_ms && label.as_ref().is_none_or(|value| !value.is_empty()),
    }
}
