//! Deterministic descriptive task-cluster bootstrap without external RNG dependencies.
use super::BootstrapSettings;

/// Fixed algorithm/seed boundary for reproducible version-two report uncertainty.
pub const BOOTSTRAP_SEED: u64 = 0x4a55_4447_4556_414c;

/// Resample already paired task means, never individual repetitions. The percentile interval
/// is conditional on the measured corpus and does not establish representative sampling,
/// independent oracle provenance, p-values, or joint six-feature confidence.
pub(super) fn interval(means: &[f64], settings: &BootstrapSettings) -> Option<[f64; 2]> {
    if means.len() < settings.minimum_tasks || !valid(settings) {
        return None;
    }
    if means
        .len()
        .checked_mul(settings.replicates)
        .is_none_or(|work| work > 10_000_000)
    {
        return None;
    }
    let bound = u64::try_from(means.len()).ok()?;
    let task_count = f64::from(u32::try_from(means.len()).ok()?);
    let mut state = BOOTSTRAP_SEED;
    let mut samples = Vec::with_capacity(settings.replicates);
    for _ in 0..settings.replicates {
        let mut sum = 0.0;
        for _ in means {
            // SplitMix64; rejection avoids modulo bias in the task selection.
            let threshold = bound.wrapping_neg() % bound;
            let index = loop {
                state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
                let mut value = state;
                value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                value ^= value >> 31;
                if value >= threshold {
                    break usize::try_from(value % bound).ok()?;
                }
            };
            sum += means[index] / task_count;
        }
        samples.push(sum);
    }
    samples.sort_by(f64::total_cmp);
    let tail = (1.0 - settings.confidence_level) / 2.0;
    Some([quantile(&samples, tail)?, quantile(&samples, 1.0 - tail)?])
}

// Select floor((n - 1) * fraction) using bounded integer comparisons, rather than
// a float-to-index cast. At most 10,000 comparisons, with no truncation/sign fallback.
fn quantile(samples: &[f64], fraction: f64) -> Option<f64> {
    if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) {
        return None;
    }
    let last = u32::try_from(samples.len().checked_sub(1)?).ok()?;
    let position = f64::from(last) * fraction;
    let index = (0..=last)
        .take_while(|index| f64::from(*index) <= position)
        .last()?;
    samples.get(usize::try_from(index).ok()?).copied()
}

pub(super) fn valid(settings: &BootstrapSettings) -> bool {
    (200..=10_000).contains(&settings.replicates)
        && settings.minimum_tasks >= 5
        && settings.confidence_level.is_finite()
        && (0.8..=0.99).contains(&settings.confidence_level)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quantile_indices_are_bounded_without_numeric_casts() {
        assert_eq!(quantile(&[], 0.5), None);
        assert_eq!(quantile(&[1.0, 2.0, 3.0], 0.0), Some(1.0));
        assert_eq!(quantile(&[1.0, 2.0, 3.0], 0.5), Some(2.0));
        assert_eq!(quantile(&[1.0, 2.0, 3.0], 1.0), Some(3.0));
        assert_eq!(quantile(&[1.0], -0.1), None);
        assert_eq!(quantile(&[1.0], f64::NAN), None);
    }
    #[test]
    fn deterministic_independent_task_uncertainty() {
        let settings = BootstrapSettings::default();
        assert_eq!(interval(&[1.0; 4], &settings), None);
        assert_eq!(interval(&[1.0; 5], &settings), Some([1.0, 1.0]));
        let means = [-1.0, -0.5, 0.0, 0.5, 1.0];
        let first = interval(&means, &settings).unwrap();
        assert_eq!(Some(first), interval(&means, &settings));
        assert!(first[0] < 0.0 && first[1] > 0.0);
    }
}
