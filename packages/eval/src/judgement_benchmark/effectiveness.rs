//! Independent end-task effectiveness, separate from service-ranking calibration.
//!
//! These contracts execute nothing and cannot authorize spending or promotion.
//! Public observations are caller declarations, not authenticated producer evidence.
//! No authenticated conversion exists here: all reports remain unverified and
//! promotion is blocked regardless of caller-supplied counters or confirmed statuses.
use super::{Arm, EvidenceSource, EvidenceStatus, FEATURES, Verdict};
use bcode_model::judgement::Feature;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Frozen dataset partition. Development evidence cannot promote a feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatasetSplit {
    /// May be used for development, never substituted for held-out evidence.
    Development,
    /// Frozen before tuning; its oracle is evaluated independently of judgement.
    HeldOut,
}

/// Independently authored task and its complete binary command-test oracle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectivenessTask {
    /// Unique scheduled task, including any authored permutation identity.
    pub task_id: String,
    /// Correlation cluster; permutations and seeds share the original family.
    pub family_id: String,
    /// Exactly one feature may vary.
    pub feature: Feature,
    /// Frozen task/input/model/budget/settings identity, excluding feature toggle.
    pub pairing_id: String,
    /// Immutable independent test suite identity, not provider-produced labels.
    pub oracle_id: String,
    /// Exact independent test identifiers; all must pass for task correctness.
    pub test_ids: BTreeSet<String>,
    /// Scheduled repetitions, not independent samples.
    pub repetitions: u32,
    /// Requires verified actual workflow dispatch, not merely a graph definition.
    pub requires_workflow_dispatch: bool,
}

/// Explicit preregistration; unknown schema versions fail closed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectivenessPlan {
    /// Version 1 only, separate from legacy service benchmark schemas.
    pub schema_version: u32,
    /// Frozen registration identity shared by every observation.
    pub preregistration_id: String,
    /// Frozen corpus identity shared by every observation.
    pub dataset_id: String,
    /// Development versus held-out identity cannot be rewritten after capture.
    pub split: DatasetSplit,
    /// Complete preregistered schedule.
    pub tasks: Vec<EffectivenessTask>,
    /// Minimum independent families (must be at least five); no significance claim.
    pub minimum_families: usize,
    /// Practical family-balanced correctness improvement, in (0, 1].
    pub minimum_gain: f64,
    /// Maximum extra whole-task USD, checked on every pair, including retries.
    pub maximum_extra_cost_usd: f64,
    /// Maximum extra whole-task wall latency, checked on every pair.
    pub maximum_extra_latency_ms: f64,
}

/// Actual portable execution identity; no provider-private payloads or credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectivenessIdentity {
    /// Exact subject artifact digest, not a mutable executable path.
    pub artifact_id: String,
    /// Exact base provider/model resolution identity.
    pub base_model_id: String,
    /// Equal bounded execution allowances; immutable digest, not observed usage.
    pub budget_id: String,
    /// Equal input/tools/cache/retry/config settings excluding only this toggle.
    pub settings_id: String,
}

/// Caller-declared terminal test measurement plus application/cost telemetry.
///
/// Nullable counters are unknown, not zero. None of these fields authenticate capture
/// provenance: even `Confirmed` is a declaration until a trusted producer verifies it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectivenessObservation {
    /// Version 1 only.
    pub schema_version: u32,
    /// Exact registration identity.
    pub preregistration_id: String,
    /// Exact frozen dataset identity.
    pub dataset_id: String,
    /// Exact partition identity.
    pub split: DatasetSplit,
    /// Scheduled task.
    pub task_id: String,
    /// Scheduled repetition.
    pub repetition: u32,
    /// Off or only-feature-on.
    pub arm: Arm,
    /// Unique bounded capture identity.
    pub capture_id: String,
    /// Frozen task/input/settings identity.
    pub pairing_id: String,
    /// Actual resolved subject/model/allowances.
    pub identity: EffectivenessIdentity,
    /// Live and offline consumer evidence remain distinct from calibration.
    pub source: EvidenceSource,
    /// Exact independent oracle identity; mismatch cannot score as benefit.
    pub oracle_id: String,
    /// Exact tests with independently observed pass/fail; None is unavailable.
    pub tests: BTreeMap<String, Option<bool>>,
    /// Independent terminal command-test evaluation, never advisory judge labels.
    pub independent_tests: EvidenceStatus,
    /// Canonical terminal measurement settled, including failures/cancellation.
    pub terminal_observed: EvidenceStatus,
    /// Exactly one feature differs; no all-on aggregate attribution.
    pub isolated_feature: EvidenceStatus,
    /// Permissions, canonical outcomes and request-only persistence preserved.
    pub safety_preserved: EvidenceStatus,
    /// Actual ordinary consumer trigger count, including policy-off hooks.
    pub trigger_count: Option<u64>,
    /// Actual advisory installation count; off must install nothing.
    pub application_count: Option<u64>,
    /// Actual optional discovery calls.
    pub discovery_calls: Option<u64>,
    /// Actual optional verification calls.
    pub verification_calls: Option<u64>,
    /// Actual optional inference calls.
    pub inference_calls: Option<u64>,
    /// Actual ordinary context admission (required for context reranking).
    pub ordinary_admission: EvidenceStatus,
    /// Admitted context supplied to dispatched base model (not service-only output).
    pub base_context_consumed: EvidenceStatus,
    /// Actual workflow dispatch proof when the task requires workflows.
    pub workflow_dispatched: EvidenceStatus,
    /// All calls, retries, discovery and verification billed with complete ledger.
    pub billing_complete: EvidenceStatus,
    /// Whole-task USD; None does not imply free execution.
    pub total_cost_usd: Option<f64>,
    /// Whole-task wall time; corpus observation time is not wall time.
    pub latency_ms: Option<f64>,
}

/// Descriptive paired direction counts, never probability or significance estimates.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectivenessCounts {
    /// Strictly greater correctness in on arm.
    pub wins: usize,
    /// Strictly lower correctness in on arm.
    pub losses: usize,
    /// Equal correctness (including two independently observed failures).
    pub ties: usize,
    /// Missing, contradictory or unverified comparison.
    pub inconclusive: usize,
}
impl EffectivenessCounts {
    fn push(&mut self, delta: Option<f64>) {
        match delta {
            Some(value) if value > 0.0 => self.wins += 1,
            Some(value) if value < 0.0 => self.losses += 1,
            Some(_) => self.ties += 1,
            None => self.inconclusive += 1,
        }
    }
}

/// One feature's independent end-task report. No cross-feature benefit averaging.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectivenessFeatureReport {
    /// Feature under test.
    pub feature: Feature,
    /// All scheduled pairs, including unavailable arms.
    pub pairs: EffectivenessCounts,
    /// Original family clusters; seeds/permutations cannot inflate this denominator.
    pub families: EffectivenessCounts,
    /// Mean of per-family correctness deltas; None unless all pairs are verified.
    pub family_mean_delta: Option<f64>,
    /// Descriptive verdict, including offline evidence; not release authorization.
    pub verdict: Verdict,
    /// Always false for this declaration-only API. Requires a future authenticated
    /// producer adapter verifying immutable execution/test/billing evidence, not
    /// caller-supplied identities, digests, booleans or aggregate statuses.
    pub promotion_eligible: bool,
    /// Explicit evidence and gate gaps; unknown facts never silently pass.
    pub gaps: Vec<String>,
}

/// Evidence trust boundary of the comparator input. No caller can assert authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectivenessEvidenceScope {
    /// Descriptive consistency of unverified input; not actual-consumer proof.
    CallerDeclarationsOnly,
}

/// Separately versioned independent-task report; legacy ranking reports are unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectivenessReport {
    /// Version 2 adds explicit evidence scope and blocks declaration-only promotion.
    pub schema_version: u32,
    /// Trust scope is set by the comparator, never copied from caller declarations.
    pub evidence_scope: EffectivenessEvidenceScope,
    /// Full frozen schedule and gates for auditability.
    pub plan: EffectivenessPlan,
    /// Feature-local results, including unscheduled features.
    pub features: Vec<EffectivenessFeatureReport>,
}

fn id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}
fn nonnegative(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}
fn confirmed(value: EvidenceStatus) -> bool {
    value == EvidenceStatus::Confirmed
}
fn validate_plan(plan: &EffectivenessPlan) -> Result<(), &'static str> {
    if plan.schema_version != 1 || !id(&plan.preregistration_id) || !id(&plan.dataset_id) {
        return Err("unsupported plan version or missing frozen identity");
    }
    if !(plan.minimum_families >= 5
        && plan.minimum_gain.is_finite()
        && 0.0 < plan.minimum_gain
        && plan.minimum_gain <= 1.0
        && nonnegative(plan.maximum_extra_cost_usd)
        && nonnegative(plan.maximum_extra_latency_ms))
    {
        return Err("invalid preregistered effectiveness gates");
    }
    let mut ids = BTreeSet::new();
    let mut families = BTreeMap::new();
    for task in &plan.tasks {
        if !id(&task.task_id)
            || !id(&task.family_id)
            || !id(&task.pairing_id)
            || !id(&task.oracle_id)
            || task.repetitions == 0
            || task.test_ids.is_empty()
            || task.test_ids.iter().any(|test| !id(test))
            || !ids.insert(&task.task_id)
        {
            return Err("invalid frozen task schedule or oracle");
        }
        if families
            .insert(&task.family_id, task.feature)
            .is_some_and(|old| old != task.feature)
        {
            return Err("family cannot cross isolated features");
        }
    }
    Ok(())
}

fn valid_measurement(
    plan: &EffectivenessPlan,
    task: &EffectivenessTask,
    row: &EffectivenessObservation,
) -> bool {
    let identity = &row.identity;
    row.preregistration_id == plan.preregistration_id
        && row.dataset_id == plan.dataset_id
        && row.split == plan.split
        && row.pairing_id == task.pairing_id
        && row.oracle_id == task.oracle_id
        && id(&row.capture_id)
        && id(&identity.artifact_id)
        && id(&identity.base_model_id)
        && id(&identity.budget_id)
        && id(&identity.settings_id)
        && row.source != EvidenceSource::ScorerCalibration
        && confirmed(row.independent_tests)
        && confirmed(row.terminal_observed)
        && confirmed(row.isolated_feature)
        && confirmed(row.safety_preserved)
        && row.tests.keys().eq(task.test_ids.iter())
        && row.tests.values().all(Option::is_some)
        && row.trigger_count.is_some_and(|count| count > 0)
        && row.discovery_calls.is_some()
        && row.verification_calls.is_some()
        && row.inference_calls.is_some()
        && match row.arm {
            Arm::Off => {
                row.application_count == Some(0)
                    && row.discovery_calls == Some(0)
                    && row.verification_calls == Some(0)
                    && row.inference_calls == Some(0)
            }
            Arm::On => {
                row.application_count.is_some_and(|count| count > 0)
                    && row.inference_calls.is_some_and(|count| count > 0)
                    && row
                        .application_count
                        .zip(row.trigger_count)
                        .is_some_and(|(applied, triggered)| applied <= triggered)
            }
        }
        && (task.feature != Feature::ContextReranking
            || (confirmed(row.ordinary_admission) && confirmed(row.base_context_consumed)))
        && (!task.requires_workflow_dispatch || confirmed(row.workflow_dispatched))
}
fn correctness(row: &EffectivenessObservation) -> f64 {
    if row.tests.values().all(|pass| *pass == Some(true)) {
        1.0
    } else {
        0.0
    }
}

type Inventory<'a> = BTreeMap<(&'a str, u32, Arm), Vec<&'a EffectivenessObservation>>;
fn unique<'a>(
    inventory: &Inventory<'a>,
    task: &str,
    repetition: u32,
    arm: Arm,
) -> Option<&'a EffectivenessObservation> {
    inventory
        .get(&(task, repetition, arm))
        .filter(|rows| rows.len() == 1)
        .map(|rows| rows[0])
}

fn feature_report(
    feature: Feature,
    plan: &EffectivenessPlan,
    inventory: &Inventory<'_>,
) -> EffectivenessFeatureReport {
    let mut report = EffectivenessFeatureReport {
        feature,
        pairs: EffectivenessCounts::default(),
        families: EffectivenessCounts::default(),
        family_mean_delta: None,
        verdict: Verdict::Inconclusive,
        promotion_eligible: false,
        gaps: vec!["unauthenticated caller declarations; immutable consumer/test/billing evidence requires authenticated producer conversion".into()],
    };
    let mut families: BTreeMap<&str, Vec<Option<f64>>> = BTreeMap::new();
    for task in plan.tasks.iter().filter(|task| task.feature == feature) {
        for repetition in 0..task.repetitions {
            let pair = unique(inventory, &task.task_id, repetition, Arm::Off).zip(unique(
                inventory,
                &task.task_id,
                repetition,
                Arm::On,
            ));
            let delta = pair
                .filter(|(off, on)| {
                    valid_measurement(plan, task, off)
                        && valid_measurement(plan, task, on)
                        && off.identity == on.identity
                        && off.capture_id != on.capture_id
                        && off.source == on.source
                })
                .map(|(off, on)| {
                    if off.source != EvidenceSource::LiveConsumer {
                        report.gaps.push("offline evidence cannot promote".into());
                    }
                    if !confirmed(off.billing_complete) || !confirmed(on.billing_complete) {
                        report
                            .gaps
                            .push("unknown or incomplete billing ledger".into());
                    }
                    match off
                        .total_cost_usd
                        .filter(|value| nonnegative(*value))
                        .zip(on.total_cost_usd.filter(|value| nonnegative(*value)))
                    {
                        Some((off, on)) if on - off <= plan.maximum_extra_cost_usd => {}
                        Some(_) => report.gaps.push("pair exceeds cost overhead gate".into()),
                        None => report.gaps.push("unknown whole-task cost".into()),
                    }
                    match off
                        .latency_ms
                        .filter(|value| nonnegative(*value))
                        .zip(on.latency_ms.filter(|value| nonnegative(*value)))
                    {
                        Some((off, on)) if on - off <= plan.maximum_extra_latency_ms => {}
                        Some(_) => report
                            .gaps
                            .push("pair exceeds latency overhead gate".into()),
                        None => report.gaps.push("unknown whole-task latency".into()),
                    }
                    correctness(on) - correctness(off)
                });
            if delta.is_none() {
                report.gaps.push(format!(
                    "{}:{repetition}: missing, duplicate or incompatible task/application evidence",
                    task.task_id
                ));
            }
            report.pairs.push(delta);
            families.entry(&task.family_id).or_default().push(delta);
        }
    }
    let mut means = Vec::new();
    for values in families.values() {
        let delta = values
            .iter()
            .copied()
            .collect::<Option<Vec<_>>>()
            .map(|values| values.iter().sum::<f64>() / values.len() as f64);
        report.families.push(delta);
        if let Some(delta) = delta {
            means.push(delta);
        }
    }
    if report.families.inconclusive == 0 && !means.is_empty() {
        report.family_mean_delta = Some(means.iter().sum::<f64>() / means.len() as f64);
    }
    if families.len() < plan.minimum_families {
        report.gaps.push("insufficient independent families".into());
    }
    if report.pairs.losses > 0 {
        report.verdict = if report.pairs.wins > 0 {
            Verdict::Tradeoff
        } else {
            Verdict::Regressed
        };
        report
            .gaps
            .push("observed task regression; average cannot hide losses".into());
    } else if report
        .family_mean_delta
        .is_some_and(|mean| mean >= plan.minimum_gain)
        && families.len() >= plan.minimum_families
    {
        report.verdict = Verdict::Improved;
    }
    if plan.split != DatasetSplit::HeldOut {
        report
            .gaps
            .push("development evidence cannot promote".into());
    }
    report.gaps.sort();
    report.gaps.dedup();
    // Neither matching declarations nor claimed live provenance authenticate an
    // actual capture. There is deliberately no public boolean escape hatch.
    report.promotion_eligible = false;
    report
}

/// Compare consistency of declared independent task results, without execution or mutation.
///
/// All input is unauthenticated: `Improved` describes supplied measurements only, never
/// consumer-observed benefit. Promotion is always blocked, even for claimed live held-out
/// data. An authenticated producer conversion verifying immutable evidence is not yet
/// implemented; callers cannot manufacture that authority by setting aggregate statuses.
/// Families cluster repeated tasks/permutations without significance claims.
///
/// # Errors
/// Rejects unknown versions, invalid gates/identities/oracles, and unscheduled observations.
/// Evidence gaps (including changed oracle identity) remain inconclusive, not scored zero.
pub fn compare_effectiveness(
    plan: &EffectivenessPlan,
    observations: &[EffectivenessObservation],
) -> Result<EffectivenessReport, &'static str> {
    validate_plan(plan)?;
    let schedule: BTreeMap<_, _> = plan
        .tasks
        .iter()
        .map(|task| (task.task_id.as_str(), task))
        .collect();
    let mut inventory = Inventory::new();
    let mut captures = BTreeSet::new();
    for row in observations {
        let task = schedule
            .get(row.task_id.as_str())
            .ok_or("unscheduled observed task")?;
        if row.schema_version != 1 || row.repetition >= task.repetitions {
            return Err("unsupported observation version or unscheduled repetition");
        }
        if !captures.insert(&row.capture_id) {
            return Err("capture identity reused across observations");
        }
        inventory
            .entry((row.task_id.as_str(), row.repetition, row.arm))
            .or_default()
            .push(row);
    }
    Ok(EffectivenessReport {
        schema_version: 2,
        evidence_scope: EffectivenessEvidenceScope::CallerDeclarationsOnly,
        plan: plan.clone(),
        features: FEATURES
            .into_iter()
            .map(|feature| feature_report(feature, plan, &inventory))
            .collect(),
    })
}
