//! Analyze observed cache rounds against expectations.
//!
//! All cache-ratio arithmetic in Bcode lives here so eval telemetry, live verification, and CI
//! round-trip tests judge the same numbers the same way.

use bcode_prompt_cache_models::{
    CacheRoundObservation, PromptCacheExpectations, PromptCacheScenarioOutcome, measurement,
};
use std::collections::BTreeMap;

/// Measurements and verdicts derived from a sequence of cache rounds.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptCacheAnalysis {
    /// Measurements keyed by [`measurement`] constants.
    pub measurements: BTreeMap<String, f64>,
    /// Human-readable failures; empty when every applicable check passed.
    pub failures: Vec<String>,
}

impl PromptCacheAnalysis {
    /// Convert the analysis into a scenario outcome.
    #[must_use]
    pub fn outcome(&self) -> PromptCacheScenarioOutcome {
        if self.failures.is_empty() {
            PromptCacheScenarioOutcome::Passed
        } else {
            PromptCacheScenarioOutcome::Failed {
                reason: self.failures.join("; "),
            }
        }
    }

    fn measure(&mut self, key: &str, value: f64) {
        self.measurements.insert(key.to_string(), value);
    }

    fn fail(&mut self, message: impl Into<String>) {
        self.failures.push(message.into());
    }
}

/// Summarize a sequence of rounds without judging them.
///
/// Useful for telemetry where thresholds are applied later (for example eval baselines).
#[must_use]
pub fn measure_rounds(rounds: &[CacheRoundObservation]) -> BTreeMap<String, f64> {
    let mut analysis = PromptCacheAnalysis {
        measurements: BTreeMap::new(),
        failures: Vec::new(),
    };
    record_measurements(&mut analysis, rounds);
    analysis.measurements
}

/// Judge a sequence of rounds that all share one stable prefix.
///
/// Round zero is the cold request; every later round is eligible for a cache read. Checks:
///
/// * every round has a coherent input breakdown;
/// * no explicit cache points were dropped (when reported);
/// * the eligible hit ratio meets the threshold;
/// * cached tokens never shrink across eligible rounds (rolling breakpoints advance);
/// * the late uncached tail stays within its bound (for four or more eligible rounds);
/// * cache-write amplification stays within its bound when writes are reported.
#[must_use]
pub fn analyze_rounds(
    rounds: &[CacheRoundObservation],
    expectations: &PromptCacheExpectations,
) -> PromptCacheAnalysis {
    let mut analysis = PromptCacheAnalysis {
        measurements: BTreeMap::new(),
        failures: Vec::new(),
    };
    record_measurements(&mut analysis, rounds);
    if rounds.is_empty() {
        analysis.fail("no provider rounds were observed");
        return analysis;
    }
    for round in rounds {
        if !round.valid_input_breakdown {
            analysis.fail(format!(
                "round {} reported cache subsets that exceed its input tokens",
                round.round
            ));
        }
        if round
            .dropped_cache_points
            .is_some_and(|dropped| dropped > 0)
        {
            analysis.fail(format!(
                "round {} dropped {} explicit cache points",
                round.round,
                round.dropped_cache_points.unwrap_or_default()
            ));
        }
    }
    let eligible = &rounds[1..];
    if eligible.is_empty() {
        return analysis;
    }
    let thresholds = &expectations.thresholds;
    let hit_ratio = analysis
        .measurements
        .get(measurement::HIT_ROUND_RATIO)
        .copied()
        .unwrap_or_default();
    if hit_ratio < thresholds.min_hit_round_ratio {
        analysis.fail(format!(
            "cache hit ratio {hit_ratio:.2} across {} eligible rounds is below {:.2}",
            eligible.len(),
            thresholds.min_hit_round_ratio
        ));
    }
    let mut previous_cached = None;
    for round in eligible {
        let cached = round.cached_input_tokens.unwrap_or_default();
        if let Some(previous) = previous_cached
            && cached < previous
        {
            analysis.fail(format!(
                "cached input shrank from {previous} to {cached} at round {}",
                round.round
            ));
            break;
        }
        previous_cached = Some(cached);
    }
    if eligible.len() >= 4 {
        let late_ratio = analysis
            .measurements
            .get(measurement::LATE_UNCACHED_RATIO)
            .copied()
            .unwrap_or_default();
        if late_ratio > thresholds.max_late_uncached_ratio {
            analysis.fail(format!(
                "late uncached input ratio {late_ratio:.2} exceeds {:.2}",
                thresholds.max_late_uncached_ratio
            ));
        }
    }
    if rounds.iter().any(CacheRoundObservation::has_cache_write) {
        let amplification = analysis
            .measurements
            .get(measurement::WRITE_AMPLIFICATION)
            .copied()
            .unwrap_or_default();
        if amplification > thresholds.max_write_amplification {
            analysis.fail(format!(
                "cache write amplification {amplification:.2} exceeds {:.2}",
                thresholds.max_write_amplification
            ));
        }
    }
    analysis
}

/// Judge a cold request followed by one warm repeat of the identical prefix.
#[must_use]
pub fn analyze_warm_repeat(
    cold: &CacheRoundObservation,
    warm: &CacheRoundObservation,
    expectations: &PromptCacheExpectations,
) -> PromptCacheAnalysis {
    let rounds = [cold.clone(), warm.clone()];
    let mut analysis = analyze_rounds(&rounds, expectations);
    let warm_input = f64::from(warm.input_tokens.unwrap_or_default());
    let warm_cached = f64::from(warm.cached_input_tokens.unwrap_or_default());
    let warm_ratio = if warm_input > 0.0 {
        warm_cached / warm_input
    } else {
        0.0
    };
    analysis.measure(measurement::WARM_READ_RATIO, warm_ratio);
    if warm_ratio < expectations.thresholds.min_warm_read_ratio {
        analysis.fail(format!(
            "warm same-prefix repeat read {warm_ratio:.2} of its input from cache; expected at least {:.2}",
            expectations.thresholds.min_warm_read_ratio
        ));
    }
    if expectations.reports_cache_writes == Some(true) && !cold.has_cache_write() {
        analysis.fail("cold request reported no cache writes although the model reports them");
    }
    analysis
}

fn record_measurements(analysis: &mut PromptCacheAnalysis, rounds: &[CacheRoundObservation]) {
    let eligible = rounds.get(1..).unwrap_or_default();
    let hits = eligible
        .iter()
        .filter(|round| round.has_cache_read())
        .count();
    let increases = eligible
        .windows(2)
        .filter(|pair| {
            pair[1].cached_input_tokens.unwrap_or_default()
                > pair[0].cached_input_tokens.unwrap_or_default()
        })
        .count();
    let sum = |field: fn(&CacheRoundObservation) -> Option<u32>| -> f64 {
        rounds
            .iter()
            .map(|round| f64::from(field(round).unwrap_or_default()))
            .sum()
    };
    let input = sum(|round| round.input_tokens);
    let cached = sum(|round| round.cached_input_tokens);
    let writes = sum(|round| round.cache_write_input_tokens);
    let uncached = sum(|round| round.uncached_input_tokens);
    let dropped = rounds
        .iter()
        .map(|round| usize_f64(round.dropped_cache_points.unwrap_or_default()))
        .sum::<f64>();

    let late_start = eligible.len().saturating_sub(eligible.len() / 3);
    let late = &eligible[late_start.min(eligible.len())..];
    let late_input = late
        .iter()
        .map(|round| f64::from(round.input_tokens.unwrap_or_default()))
        .sum::<f64>();
    let late_uncached = late
        .iter()
        .map(|round| f64::from(round.uncached_input_tokens.unwrap_or_default()))
        .sum::<f64>();
    let late_ratio = if late_input > 0.0 {
        late_uncached / late_input
    } else {
        0.0
    };
    let final_input = rounds
        .last()
        .and_then(|round| round.input_tokens)
        .map_or(0.0, f64::from);
    let write_amplification = if final_input > 0.0 {
        writes / final_input
    } else {
        0.0
    };

    analysis.measure(measurement::ROUND_COUNT, usize_f64(rounds.len()));
    analysis.measure(measurement::ELIGIBLE_ROUND_COUNT, usize_f64(eligible.len()));
    analysis.measure(measurement::HIT_ROUND_COUNT, usize_f64(hits));
    analysis.measure(
        measurement::HIT_ROUND_RATIO,
        if eligible.is_empty() {
            0.0
        } else {
            usize_f64(hits) / usize_f64(eligible.len())
        },
    );
    analysis.measure(
        measurement::CACHED_INPUT_INCREASE_COUNT,
        usize_f64(increases),
    );
    analysis.measure(measurement::INPUT_TOKENS, input);
    analysis.measure(measurement::CACHED_INPUT_TOKENS, cached);
    analysis.measure(measurement::CACHE_WRITE_INPUT_TOKENS, writes);
    analysis.measure(measurement::UNCACHED_INPUT_TOKENS, uncached);
    analysis.measure(measurement::LATE_UNCACHED_RATIO, late_ratio);
    analysis.measure(measurement::WRITE_AMPLIFICATION, write_amplification);
    analysis.measure(measurement::DROPPED_CACHE_POINTS, dropped);
}

fn usize_f64(value: usize) -> f64 {
    u32::try_from(value).map_or(f64::MAX, f64::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_prompt_cache_models::{PromptCacheMechanism, PromptCacheThresholds};
    use std::collections::BTreeSet;

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    fn expectations() -> PromptCacheExpectations {
        PromptCacheExpectations {
            mechanism: PromptCacheMechanism::ExplicitPoints,
            reports_cache_writes: Some(true),
            supports_cache_key: true,
            ttl_seconds: BTreeSet::from([300]),
            min_prefix_tokens: 64,
            min_prefix_declared: true,
            max_cache_points: Some(4),
            thresholds: PromptCacheThresholds::default(),
        }
    }

    fn round(index: usize, input: u32, cached: u32, written: u32) -> CacheRoundObservation {
        CacheRoundObservation {
            round: index,
            input_tokens: Some(input),
            cached_input_tokens: Some(cached),
            cache_write_input_tokens: Some(written),
            uncached_input_tokens: Some(input.saturating_sub(cached).saturating_sub(written)),
            valid_input_breakdown: cached.saturating_add(written) <= input,
            dropped_cache_points: Some(0),
            ..CacheRoundObservation::default()
        }
    }

    #[test]
    fn healthy_tool_loop_passes() {
        let rounds = (0..8_u32)
            .map(|index| {
                let input = 1_000 + index * 200;
                let index = index as usize;
                if index == 0 {
                    round(0, input, 0, input)
                } else {
                    round(index, input, input - 200, 200)
                }
            })
            .collect::<Vec<_>>();
        let analysis = analyze_rounds(&rounds, &expectations());
        assert!(analysis.failures.is_empty(), "{:?}", analysis.failures);
        assert_close(analysis.measurements[measurement::HIT_ROUND_RATIO], 1.0);
        assert_close(
            analysis.measurements[measurement::CACHED_INPUT_INCREASE_COUNT],
            6.0,
        );
        assert!(analysis.measurements[measurement::LATE_UNCACHED_RATIO] < 0.01);
        // Each new segment is written exactly once, so total writes match the final request.
        assert_close(analysis.measurements[measurement::WRITE_AMPLIFICATION], 1.0);
    }

    #[test]
    fn missing_hits_fail() {
        let rounds = vec![
            round(0, 1_000, 0, 1_000),
            round(1, 1_200, 0, 0),
            round(2, 1_400, 0, 0),
        ];
        let analysis = analyze_rounds(&rounds, &expectations());
        assert!(
            analysis
                .failures
                .iter()
                .any(|failure| failure.contains("hit ratio 0.00"))
        );
    }

    #[test]
    fn shrinking_cache_and_dropped_points_fail() {
        let mut rounds = vec![
            round(0, 1_000, 0, 1_000),
            round(1, 1_200, 1_000, 200),
            round(2, 1_400, 800, 0),
        ];
        rounds[2].dropped_cache_points = Some(1);
        let analysis = analyze_rounds(&rounds, &expectations());
        assert!(
            analysis
                .failures
                .iter()
                .any(|failure| failure.contains("shrank"))
        );
        assert!(
            analysis
                .failures
                .iter()
                .any(|failure| failure.contains("dropped 1"))
        );
    }

    #[test]
    fn write_amplification_is_bounded() {
        let rounds = vec![
            round(0, 1_000, 0, 1_000),
            round(1, 1_000, 0, 1_000),
            round(2, 1_000, 0, 1_000),
            round(3, 1_000, 0, 1_000),
        ];
        let analysis = analyze_rounds(&rounds, &expectations());
        assert!(
            analysis
                .failures
                .iter()
                .any(|failure| failure.contains("write amplification"))
        );
    }

    #[test]
    fn warm_repeat_requires_reads_and_cold_writes() {
        let cold = round(0, 1_000, 0, 1_000);
        let warm = round(1, 1_000, 1_000, 0);
        let analysis = analyze_warm_repeat(&cold, &warm, &expectations());
        assert!(analysis.failures.is_empty(), "{:?}", analysis.failures);
        assert_close(analysis.measurements[measurement::WARM_READ_RATIO], 1.0);

        let cold_without_writes = round(0, 1_000, 0, 0);
        let analysis = analyze_warm_repeat(&cold_without_writes, &warm, &expectations());
        assert!(
            analysis
                .failures
                .iter()
                .any(|failure| failure.contains("no cache writes"))
        );

        let weak_warm = round(1, 1_000, 100, 0);
        let analysis = analyze_warm_repeat(&cold, &weak_warm, &expectations());
        assert!(
            analysis
                .failures
                .iter()
                .any(|failure| failure.contains("warm same-prefix"))
        );
    }

    #[test]
    fn measure_rounds_never_fails() {
        let measurements = measure_rounds(&[]);
        assert_close(measurements[measurement::ROUND_COUNT], 0.0);
        assert_close(measurements[measurement::HIT_ROUND_RATIO], 0.0);
    }
}
