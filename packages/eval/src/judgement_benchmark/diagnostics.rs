//! Per-arm descriptive diagnostics retain raw counts and scheduled denominators.
use super::{
    Arm, BenchmarkObservation, BenchmarkTask, ConsumerOutput, Oracle, TerminalOutcome, score_output,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One class's confusion-derived measurements; undefined precision/recall stay unknown.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassMetrics {
    /// Ground truth support.
    pub support: usize,
    /// Predicted count.
    pub predicted: usize,
    /// True positives.
    pub true_positive: usize,
    /// True positives / predictions, unknown when no predictions.
    pub precision: Option<f64>,
    /// True positives / support, unknown when no ground truth support.
    pub recall: Option<f64>,
    /// 2 TP / (support + predictions), unknown when both are absent.
    pub f1: Option<f64>,
}
/// Independent diagnostic summary, conditional on evaluated live consumer outputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmDiagnostics {
    /// Treatment arm.
    pub arm: Arm,
    /// All scheduled trials for this arm.
    pub scheduled: usize,
    /// Eligible successful outputs; missing/failed attempts are not erased from schedule.
    pub evaluated: usize,
    /// Evaluated / scheduled; unknown on empty schedule.
    pub evaluation_coverage: Option<f64>,
    /// Full ground-truth -> predicted confusion counts; abstention is `<abstain>`.
    pub confusion: BTreeMap<String, BTreeMap<String, usize>>,
    /// Per-class metrics including abstention and observed unknown labels.
    pub classes: BTreeMap<String, ClassMetrics>,
    /// Mean F1 over supported truth classes, including abstention controls.
    pub macro_f1: Option<f64>,
    /// Explicit abstention count, not missing capture.
    pub abstentions: usize,
    /// Non-abstentions / evaluated classification/progress outputs.
    pub prediction_coverage: Option<f64>,
    /// Raw ranking component measurements, one per eligible ranking output.
    pub ranking: Vec<RankingMetrics>,
    /// Progress positive oracle count among evaluated outputs.
    pub positive_events: usize,
    /// Progress negative oracle count among evaluated outputs.
    pub negative_events: usize,
    /// Timely correct progress advice count.
    pub detections: usize,
    /// False advice on a negative or incorrect/early/late positive event.
    pub false_alerts: usize,
    /// Positive events without timely correct advice.
    pub missed_events: usize,
    /// Timely true-positive detection delays; absence never becomes zero delay.
    pub detection_delays_ms: Vec<u64>,
}
/// Separately visible ranking quality components.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RankingMetrics {
    /// Task identity.
    pub task_id: String,
    /// Scheduled repetition.
    pub repetition: u32,
    /// Structural/admission validity.
    pub constraints_passed: bool,
    /// Graded NDCG at oracle k, zero for invalid outputs.
    pub ndcg: f64,
    /// Positive relevance candidates found in top k / all positive candidates.
    /// Unknown when no relevant candidates exist.
    pub recall_at_k: Option<f64>,
    /// Actual admitted gain / independently optimal feasible gain.
    pub admission_score: f64,
}
fn ratio(numerator: usize, denominator: usize) -> Option<f64> {
    (denominator > 0).then(|| numerator as f64 / denominator as f64)
}
fn gain(value: f64) -> f64 {
    value.exp2() - 1.0
}
fn dcg(values: impl Iterator<Item = f64>) -> f64 {
    values
        .enumerate()
        .map(|(index, value)| gain(value) / ((index + 2) as f64).log2())
        .sum()
}
pub(super) fn ranking_metrics(
    task: &BenchmarkTask,
    row: &BenchmarkObservation,
    output: &ConsumerOutput,
) -> Option<RankingMetrics> {
    let Oracle::Ranking {
        relevance,
        k,
        ideal_admission_gain,
        ..
    } = &task.oracle
    else {
        return None;
    };
    let ConsumerOutput::Ranking { order, admitted } = output else {
        return None;
    };
    let valid = score_output(&task.oracle, output).constraints_passed;
    let positive = relevance.values().filter(|value| **value > 0.0).count();
    let mut ideal: Vec<_> = relevance.values().copied().collect();
    ideal.sort_by(|a, b| b.total_cmp(a));
    let denominator = dcg(ideal.into_iter().take(*k));
    let ndcg = if !valid {
        0.0
    } else if denominator > 0.0 {
        dcg(order.iter().take(*k).map(|id| relevance[id])) / denominator
    } else {
        1.0
    };
    let admission_score = if !valid {
        0.0
    } else if *ideal_admission_gain > 0.0 {
        admitted.iter().map(|id| gain(relevance[id])).sum::<f64>() / ideal_admission_gain
    } else {
        1.0
    };
    let found = if valid {
        order
            .iter()
            .take(*k)
            .filter(|id| relevance[*id] > 0.0)
            .count()
    } else {
        0
    };
    Some(RankingMetrics {
        task_id: task.task_id.clone(),
        repetition: row.repetition,
        constraints_passed: valid,
        ndcg,
        recall_at_k: ratio(found, positive),
        admission_score,
    })
}
impl ArmDiagnostics {
    pub(super) const fn new(arm: Arm, scheduled: usize) -> Self {
        Self {
            arm,
            scheduled,
            evaluated: 0,
            evaluation_coverage: None,
            confusion: BTreeMap::new(),
            classes: BTreeMap::new(),
            macro_f1: None,
            abstentions: 0,
            prediction_coverage: None,
            ranking: Vec::new(),
            positive_events: 0,
            negative_events: 0,
            detections: 0,
            false_alerts: 0,
            missed_events: 0,
            detection_delays_ms: Vec::new(),
        }
    }
    pub(super) fn register_oracle(&mut self, oracle: &Oracle) {
        match oracle {
            Oracle::Classification { labels, .. } => {
                for label in labels.keys() {
                    self.confusion.entry(label.clone()).or_default();
                }
                self.confusion.entry("<abstain>".into()).or_default();
            }
            Oracle::Progress { label, .. } => {
                if let Some(label) = label {
                    self.confusion.entry(label.clone()).or_default();
                }
                self.confusion.entry("<abstain>".into()).or_default();
            }
            Oracle::Ranking { .. } => {}
        }
    }
    pub(super) fn observe(&mut self, task: &BenchmarkTask, row: &BenchmarkObservation) {
        if row.outcome != TerminalOutcome::Succeeded
            || (row.arm == Arm::On && row.evidence.inference_calls == 0)
        {
            return;
        }
        let Some(output) = &row.output else {
            return;
        };
        self.evaluated += 1;
        if let Some(metrics) = ranking_metrics(task, row, output) {
            self.ranking.push(metrics);
            return;
        }
        let (truth, predicted) = match (&task.oracle, output) {
            (
                Oracle::Classification {
                    labels,
                    abstention_credit,
                },
                ConsumerOutput::Classification { label },
            ) => {
                let best = labels
                    .iter()
                    .filter(|(_, credit)| **credit > *abstention_credit)
                    .max_by(|a, b| a.1.total_cmp(b.1));
                (
                    best.map_or_else(|| "<abstain>".into(), |(label, _)| label.clone()),
                    label.clone(),
                )
            }
            (
                Oracle::Progress { label, .. },
                ConsumerOutput::Progress {
                    label: predicted, ..
                },
            ) => {
                let score = score_output(&task.oracle, output);
                if label.is_some() {
                    self.positive_events += 1;
                    if let Some(delay) = score.detection_delay_ms {
                        self.detections += 1;
                        self.detection_delays_ms.push(delay);
                    } else {
                        self.missed_events += 1;
                        if predicted.is_some() {
                            self.false_alerts += 1;
                        }
                    }
                } else {
                    self.negative_events += 1;
                    self.false_alerts += usize::from(predicted.is_some());
                }
                (
                    label.clone().unwrap_or_else(|| "<abstain>".into()),
                    predicted.clone(),
                )
            }
            _ => (
                "<invalid-output-truth>".into(),
                Some("<invalid-output>".into()),
            ),
        };
        self.abstentions += usize::from(predicted.is_none());
        let predicted = predicted.unwrap_or_else(|| "<abstain>".into());
        *self
            .confusion
            .entry(truth)
            .or_default()
            .entry(predicted)
            .or_default() += 1;
    }
    pub(super) fn finish(&mut self) {
        self.evaluation_coverage = ratio(self.evaluated, self.scheduled);
        let labels: std::collections::BTreeSet<_> = self
            .confusion
            .keys()
            .chain(self.confusion.values().flat_map(BTreeMap::keys))
            .cloned()
            .collect();
        let classified: usize = self.confusion.values().flat_map(BTreeMap::values).sum();
        self.prediction_coverage = ratio(classified - self.abstentions, classified);
        for truth in &labels {
            let row = self.confusion.entry(truth.clone()).or_default();
            for predicted in &labels {
                row.entry(predicted.clone()).or_insert(0);
            }
        }
        for label in labels {
            let support = self
                .confusion
                .get(&label)
                .map_or(0, |row| row.values().sum());
            let predicted = self
                .confusion
                .values()
                .map(|row| row.get(&label).copied().unwrap_or(0))
                .sum();
            let tp = self
                .confusion
                .get(&label)
                .and_then(|row| row.get(&label))
                .copied()
                .unwrap_or(0);
            self.classes.insert(
                label,
                ClassMetrics {
                    support,
                    predicted,
                    true_positive: tp,
                    precision: ratio(tp, predicted),
                    recall: ratio(tp, support),
                    f1: ratio(tp * 2, support + predicted),
                },
            );
        }
        let values: Vec<_> = self
            .classes
            .values()
            .filter(|class| class.support > 0)
            .filter_map(|class| class.f1)
            .collect();
        self.macro_f1 =
            (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64);
    }
}
