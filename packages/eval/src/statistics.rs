//! Descriptive uncertainty estimates; never substitutes for per-pair quality gates.
use serde_json::{Value, json};

pub fn wilson(successes: usize, total: usize) -> Option<[f64; 2]> {
    if total == 0 {
        return None;
    }
    let n = total as f64;
    let p = successes as f64 / n;
    let z = 1.959_963_984_540_054;
    let denominator = 1.0 + z * z / n;
    let center = (p + z * z / (2.0 * n)) / denominator;
    let radius = z * ((p.mul_add(1.0 - p, z * z / (4.0 * n))) / n).sqrt() / denominator;
    Some([center - radius, center + radius])
}

pub fn describe(pairs: &[Value]) -> Value {
    let mut candidates = std::collections::BTreeMap::<String, Vec<&Value>>::new();
    for pair in pairs {
        if let Some(id) = pair["candidate"].as_str() {
            candidates.entry(id.into()).or_default().push(pair);
        }
    }
    let summaries:Vec<_>=candidates.into_iter().map(|(id,pairs)| {
        let success=pairs.iter().filter(|pair|pair["candidate_passed"]==true).count();
        let baseline_passes=pairs.iter().filter(|pair|pair["baseline_passed"]==true).count();
        let regressions=pairs.iter().filter(|pair|pair["quality_regression"]==true).count();
        let mut ratios:Vec<_>=pairs.iter().filter(|pair|pair["speed_eligible"]==true).filter_map(|pair|pair["latency_ratio"].as_f64()).filter(|value|value.is_finite()).collect();
        ratios.sort_by(f64::total_cmp);
        let quantile=|percent:usize| ratios.get(ratios.len().saturating_mul(percent).div_ceil(100).saturating_sub(1)).copied();
        json!({"candidate":id,"trials":pairs.len(),"successes":success,"success_rate_wilson_95":wilson(success,pairs.len()),"regressions":regressions,"baseline_pass_trials":baseline_passes,"regression_rate_wilson_95":wilson(regressions,baseline_passes),"successful_pair_latency_ratio_p50":quantile(50),"successful_pair_latency_ratio_p95":quantile(95),"speed_sample_count":ratios.len(),"noninferiority_established":false})
    }).collect();
    json!({"method":"Wilson 95% intervals; nearest-rank successful-pair latency quantiles; descriptive, not independent-task generalization","candidates":summaries})
}

#[cfg(test)]
mod tests {
    use super::wilson;
    #[test]
    fn intervals_do_not_claim_certainty_from_tiny_samples() {
        assert!(wilson(0, 0).is_none());
        let interval = wilson(2, 2).unwrap();
        assert!(interval[0] < 0.5);
        assert!(interval[1] <= 1.0 + f64::EPSILON);
        assert!(wilson(0, 2).unwrap()[1] > 0.5);
    }
}
