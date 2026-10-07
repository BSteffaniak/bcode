//! Pair inventory and preregistered task-balanced descriptive gates.
use super::scoring::validate_oracle;
use super::{
    Arm, BenchmarkObservation, BenchmarkPlan, BenchmarkReport, BenchmarkTask, EvidenceSource,
    FEATURES, FeatureReport, Gates, Oracle, PairedMetric, TerminalOutcome, Verdict, score_output,
};
use bcode_model::judgement::Feature;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
struct Metric {
    tasks: BTreeMap<String, Vec<f64>>,
}
impl Metric {
    fn push(&mut self, task: &str, pair: Option<(f64, f64)>) {
        if let Some((off, on)) = pair {
            self.tasks.entry(task.into()).or_default().push(on - off);
        }
    }
    fn report(&self, settings: &super::BootstrapSettings) -> PairedMetric {
        let means: Vec<_> = self
            .tasks
            .values()
            .map(|values| values.iter().sum::<f64>() / values.len() as f64)
            .collect();
        PairedMetric {
            confidence_interval: super::uncertainty::interval(&means, settings),
            pairs: self.tasks.values().map(Vec::len).sum(),
            tasks: means.len(),
            mean_delta: (!means.is_empty()).then(|| means.iter().sum::<f64>() / means.len() as f64),
            task_delta_range: (!means.is_empty()).then(|| {
                [
                    means.iter().copied().fold(f64::INFINITY, f64::min),
                    means.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                ]
            }),
        }
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}
fn nonnegative(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}
const fn matches_feature(feature: Feature, oracle: &Oracle) -> bool {
    matches!(
        (feature, oracle),
        (
            Feature::ToolResultTriage
                | Feature::SessionCategorization
                | Feature::EvalCategorization,
            Oracle::Classification { .. }
        ) | (
            Feature::ContextReranking | Feature::Recommendations,
            Oracle::Ranking { .. }
        ) | (Feature::ProgressMonitoring, Oracle::Progress { .. })
    )
}
pub(super) fn validate_plan(plan: &BenchmarkPlan) -> Result<(), String> {
    let gates = &plan.gates;
    if !super::uncertainty::valid(&gates.bootstrap) {
        return Err("invalid bootstrap settings".into());
    }
    let mut feature_ids = BTreeSet::new();
    for limits in &gates.feature_limits {
        let id = FEATURES
            .iter()
            .position(|feature| *feature == limits.feature)
            .ok_or("unknown feature limits")?;
        if !feature_ids.insert(id)
            || !nonnegative(limits.quality_gain)
            || limits.quality_gain == 0.0
            || limits.quality_gain > 1.0
            || !nonnegative(limits.quality_loss)
            || limits.quality_loss > 1.0
            || !nonnegative(limits.latency_increase_ms)
            || !nonnegative(limits.cost_increase_usd)
            || !nonnegative(limits.reliability_loss)
            || limits.reliability_loss > 1.0
        {
            return Err("invalid or duplicate feature critical limits".into());
        }
    }
    if plan.schema_version != 1 || !valid_id(&plan.oracle_id) || !valid_id(&plan.preregistration_id)
    {
        return Err("unsupported version or invalid plan provenance".into());
    }
    if gates.minimum_tasks == 0
        || !gates.minimum_completeness.is_finite()
        || gates.minimum_completeness <= 0.0
        || gates.minimum_completeness > 1.0
        || !nonnegative(gates.quality_gain)
        || gates.quality_gain == 0.0
        || gates.quality_gain > 1.0
        || !nonnegative(gates.quality_loss)
        || gates.quality_loss > 1.0
        || !nonnegative(gates.latency_increase_ms)
        || !nonnegative(gates.cost_increase_usd)
        || !nonnegative(gates.reliability_loss)
        || gates.reliability_loss > 1.0
    {
        return Err("invalid preregistered gates".into());
    }
    let mut ids = BTreeSet::new();
    for task in &plan.tasks {
        if !valid_id(&task.task_id)
            || !ids.insert(&task.task_id)
            || task.repetitions == 0
            || !matches_feature(task.feature, &task.oracle)
            || !validate_oracle(&task.oracle)
        {
            return Err("invalid, duplicate or feature-incompatible task oracle".into());
        }
    }
    Ok(())
}
fn live(row: &BenchmarkObservation, plan: &BenchmarkPlan) -> bool {
    let evidence = &row.evidence;
    row.schema_version == 1
        && row.preregistration_id == plan.preregistration_id
        && evidence.source == EvidenceSource::LiveConsumer
        && evidence.consumer_executed == super::EvidenceStatus::Confirmed
        && evidence.isolated_feature == super::EvidenceStatus::Confirmed
        && evidence.authority_preserved == super::EvidenceStatus::Confirmed
        && evidence.request_only == super::EvidenceStatus::Confirmed
        && valid_id(&evidence.operation)
        && valid_id(&evidence.capture_id)
        && valid_id(&evidence.pairing_id)
        && (row.arm != Arm::Off
            || (evidence.discovery_calls == 0
                && evidence.verification_calls == 0
                && evidence.inference_calls == 0))
}
fn known(value: Option<f64>) -> Option<f64> {
    value.filter(|value| nonnegative(*value))
}
fn success(row: &BenchmarkObservation) -> f64 {
    if row.outcome == TerminalOutcome::Succeeded {
        1.0
    } else {
        0.0
    }
}

pub(super) fn quality_verdict(report: &FeatureReport, gates: &Gates) -> Verdict {
    verdict(report, gates, true)
}

fn verdict(report: &FeatureReport, gates: &Gates, quality_only: bool) -> Verdict {
    let Some(limits) = gates
        .feature_limits
        .iter()
        .find(|limits| limits.feature == report.feature)
    else {
        return Verdict::Inconclusive;
    };
    // Complete reliability evidence can establish regression even when failures make
    // semantic quality unavailable. Missing attempts never masquerade as successes.
    if report.reliability.pairs == report.scheduled_pairs
        && report.reliability.tasks >= gates.minimum_tasks
        && report
            .reliability
            .mean_delta
            .is_some_and(|delta| delta < -limits.reliability_loss)
    {
        return Verdict::Regressed;
    }
    // Quality coverage is scheduled, not conditional on provider or consumer success.
    if report.quality.tasks < gates.minimum_tasks
        || report
            .completeness
            .is_none_or(|fraction| fraction < gates.minimum_completeness)
        || report.reliability.pairs != report.scheduled_pairs
    {
        return Verdict::Inconclusive;
    }
    let Some(quality) = report.quality.mean_delta else {
        return Verdict::Inconclusive;
    };
    let Some(reliability) = report.reliability.mean_delta else {
        return Verdict::Inconclusive;
    };
    if quality < -limits.quality_loss || reliability < -limits.reliability_loss {
        return Verdict::Regressed;
    }
    // Quality-only evidence never asserts efficiency or full readiness. Resource
    // measurements and their unknown coverage remain untouched in the nested report.
    if quality_only {
        return match (
            report.quality.confidence_interval,
            report.reliability.confidence_interval,
        ) {
            (Some(quality_ci), Some(reliability_ci))
                if quality_ci[0] >= limits.quality_gain
                    && reliability_ci[0] >= -limits.reliability_loss =>
            {
                Verdict::Improved
            }
            _ => Verdict::Inconclusive,
        };
    }
    // Unknown pricing/timing cannot be presented as an efficient improvement.
    if report.cost_usd.pairs != report.scheduled_pairs
        || report.latency_ms.pairs != report.scheduled_pairs
    {
        return Verdict::Inconclusive;
    }
    let Some(cost) = report.cost_usd.mean_delta else {
        return Verdict::Inconclusive;
    };
    let Some(latency) = report.latency_ms.mean_delta else {
        return Verdict::Inconclusive;
    };

    let Some(quality_ci) = report.quality.confidence_interval else {
        return Verdict::Inconclusive;
    };
    let Some(reliability_ci) = report.reliability.confidence_interval else {
        return Verdict::Inconclusive;
    };
    let Some(cost_ci) = report.cost_usd.confidence_interval else {
        return Verdict::Inconclusive;
    };
    let Some(latency_ci) = report.latency_ms.confidence_interval else {
        return Verdict::Inconclusive;
    };
    let resource_loss = cost > limits.cost_increase_usd || latency > limits.latency_increase_ms;
    if quality < -limits.quality_loss || reliability < -limits.reliability_loss {
        return Verdict::Regressed;
    }
    if quality_ci[0] >= limits.quality_gain && reliability_ci[0] >= -limits.reliability_loss {
        if resource_loss {
            Verdict::Tradeoff
        } else if cost_ci[1] <= limits.cost_increase_usd
            && latency_ci[1] <= limits.latency_increase_ms
        {
            Verdict::Improved
        } else {
            Verdict::Inconclusive
        }
    } else if resource_loss {
        Verdict::Regressed
    } else {
        Verdict::Inconclusive
    }
}

type TrialKey = (String, u32, Arm);
fn unique<'a>(
    inventory: &'a BTreeMap<TrialKey, Vec<&'a BenchmarkObservation>>,
    key: &TrialKey,
) -> Option<&'a BenchmarkObservation> {
    inventory
        .get(key)
        .filter(|rows| rows.len() == 1)
        .map(|rows| rows[0])
}

struct Metrics {
    quality: Metric,
    latency: Metric,
    cost: Metric,
    reliability: Metric,
    delay: Metric,
}
impl Metrics {
    fn new() -> Self {
        Self {
            quality: Metric::default(),
            latency: Metric::default(),
            cost: Metric::default(),
            reliability: Metric::default(),
            delay: Metric::default(),
        }
    }
    fn pair(
        &mut self,
        task: &BenchmarkTask,
        off: &BenchmarkObservation,
        on: &BenchmarkObservation,
        gaps: &mut Vec<String>,
        identity: &str,
    ) {
        if off.evidence.pairing_id != on.evidence.pairing_id
            || off.evidence.capture_id == on.evidence.capture_id
        {
            gaps.push(format!("{identity}: incompatible pair evidence"));
            return;
        }
        self.reliability
            .push(&task.task_id, Some((success(off), success(on))));
        self.latency.push(
            &task.task_id,
            known(off.latency_ms).zip(known(on.latency_ms)),
        );
        self.cost
            .push(&task.task_id, known(off.cost_usd).zip(known(on.cost_usd)));
        if known(off.latency_ms).zip(known(on.latency_ms)).is_none() {
            gaps.push(format!("{identity}: unknown latency"));
        }
        if known(off.cost_usd).zip(known(on.cost_usd)).is_none() {
            gaps.push(format!("{identity}: unknown total pricing"));
        }
        if off.outcome != TerminalOutcome::Succeeded || on.outcome != TerminalOutcome::Succeeded {
            gaps.push(format!(
                "{identity}: non-successful trial; quality unavailable"
            ));
            return;
        }
        let Some((off_output, on_output)) = off.output.as_ref().zip(on.output.as_ref()) else {
            gaps.push(format!("{identity}: missing consumer output"));
            return;
        };
        // On may legitimately abstain; it still needs observed inference for a live treatment.
        if on.evidence.inference_calls == 0 {
            gaps.push(format!("{identity}: on arm never inferred"));
            return;
        }
        let off_score = score_output(&task.oracle, off_output);
        let on_score = score_output(&task.oracle, on_output);
        self.quality
            .push(&task.task_id, Some((off_score.quality, on_score.quality)));
        self.delay.push(
            &task.task_id,
            off_score
                .detection_delay_ms
                .zip(on_score.detection_delay_ms)
                .map(|(off, on)| (off as f64, on as f64)),
        );
        if !off_score.constraints_passed || !on_score.constraints_passed {
            gaps.push(format!(
                "{identity}: output structure/admission constraint failure (scored zero)"
            ));
        }
    }
}

fn feature_report(
    feature: Feature,
    plan: &BenchmarkPlan,
    inventory: &BTreeMap<TrialKey, Vec<&BenchmarkObservation>>,
) -> FeatureReport {
    let tasks: Vec<_> = plan
        .tasks
        .iter()
        .filter(|task| task.feature == feature)
        .collect();
    let scheduled_pairs = tasks.iter().map(|task| task.repetitions as usize).sum();
    let mut report = FeatureReport {
        feature,
        scheduled_tasks: tasks.len(),
        scheduled_pairs,
        scheduled_arms: scheduled_pairs * 2,
        observed_arms: 0,
        live_arms: 0,
        successful_arms: 0,
        completeness: None,
        quality: Metric::default().report(&plan.gates.bootstrap),
        latency_ms: Metric::default().report(&plan.gates.bootstrap),
        cost_usd: Metric::default().report(&plan.gates.bootstrap),
        reliability: Metric::default().report(&plan.gates.bootstrap),
        detection_delay_ms: Metric::default().report(&plan.gates.bootstrap),
        diagnostics: vec![
            super::ArmDiagnostics::new(Arm::Off, scheduled_pairs),
            super::ArmDiagnostics::new(Arm::On, scheduled_pairs),
        ],
        verdict: Verdict::Inconclusive,
        gaps: Vec::new(),
    };
    let mut metrics = Metrics::new();
    for task in tasks {
        for diagnostics in &mut report.diagnostics {
            diagnostics.register_oracle(&task.oracle);
        }
        for repetition in 0..task.repetitions {
            let identity = format!("{}:{repetition}", task.task_id);
            let mut arms = Vec::new();
            for arm in [Arm::Off, Arm::On] {
                let key = (task.task_id.clone(), repetition, arm);
                let row = unique(inventory, &key);
                if let Some(row) = row {
                    report.observed_arms += 1;
                    if live(row, plan) {
                        report.diagnostics[usize::from(arm == Arm::On)].observe(task, row);
                        report.live_arms += 1;
                        report.successful_arms +=
                            usize::from(row.outcome == TerminalOutcome::Succeeded);
                        arms.push(row);
                    } else {
                        report.gaps.push(format!(
                            "{identity}:{arm:?}: not eligible live consumer evidence"
                        ));
                    }
                } else {
                    let reason = if inventory.contains_key(&key) {
                        "ambiguous duplicate"
                    } else {
                        "missing scheduled arm"
                    };
                    report.gaps.push(format!("{identity}:{arm:?}: {reason}"));
                }
            }
            if arms.len() == 2 {
                metrics.pair(task, arms[0], arms[1], &mut report.gaps, &identity);
            }
        }
    }
    for diagnostics in &mut report.diagnostics {
        diagnostics.finish();
    }
    report.quality = metrics.quality.report(&plan.gates.bootstrap);
    report.latency_ms = metrics.latency.report(&plan.gates.bootstrap);
    report.cost_usd = metrics.cost.report(&plan.gates.bootstrap);
    report.reliability = metrics.reliability.report(&plan.gates.bootstrap);
    report.detection_delay_ms = metrics.delay.report(&plan.gates.bootstrap);
    report.completeness =
        (scheduled_pairs > 0).then(|| report.quality.pairs as f64 / scheduled_pairs as f64);
    report.verdict = verdict(&report, &plan.gates, false);
    report
}

/// Compare scheduled off/on actual-consumer captures independently for each feature.
///
/// # Errors
/// Returns an error for unsupported versions, invalid preregistration/oracles, unknown tasks,
/// unscheduled repetitions or numeric overflow. Duplicate scheduled captures are reported as
/// ambiguous gaps, never selected by arrival order. Missing trials remain scheduled.
pub fn compare_benchmark(
    plan: &BenchmarkPlan,
    observations: &[BenchmarkObservation],
) -> Result<BenchmarkReport, String> {
    validate_plan(plan)?;
    let schedule: BTreeMap<_, _> = plan
        .tasks
        .iter()
        .map(|task| (task.task_id.as_str(), task))
        .collect();
    let mut inventory = BTreeMap::<TrialKey, Vec<&BenchmarkObservation>>::new();
    let mut total = 0_usize;
    for task in &plan.tasks {
        total = total
            .checked_add(task.repetitions as usize)
            .ok_or("schedule overflow")?;
    }
    total.checked_mul(2).ok_or("schedule overflow")?;
    for row in observations {
        let task = schedule
            .get(row.task_id.as_str())
            .ok_or("unknown observed task")?;
        if row.schema_version != 1 || row.repetition >= task.repetitions {
            return Err("unsupported observation or unscheduled repetition".into());
        }
        inventory
            .entry((row.task_id.clone(), row.repetition, row.arm))
            .or_default()
            .push(row);
    }
    Ok(BenchmarkReport {
        schema_version: 3,
        oracle_id: plan.oracle_id.clone(),
        preregistration_id: plan.preregistration_id.clone(),
        gates: plan.gates.clone(),
        tasks: plan.tasks.clone(),
        observations: observations.to_vec(),
        features: FEATURES
            .into_iter()
            .map(|feature| feature_report(feature, plan, &inventory))
            .collect(),
    })
}
