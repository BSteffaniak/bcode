//! Reproducible interleaved experiment ordering.
use super::{
    EvalCaseRunResult, EvalError, EvalObservation, EvalSuite, EvalVariantRunResult, Path,
    aggregate_variant, append_observation, fs, pass_rate, run_repetition, unix_ms, write_json,
};

fn shuffle(values: &mut [usize], state: &mut u64) {
    for index in (1..values.len()).rev() {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let selected = usize::try_from(*state % u64::try_from(index + 1).unwrap()).unwrap();
        values.swap(index, selected);
    }
}

fn order(
    cases: usize,
    variants: usize,
    repetitions: u32,
    seed: u64,
    randomize_cases: bool,
) -> Vec<(usize, usize, u32)> {
    let mut tasks = Vec::new();
    let mut state = seed;
    for repetition in 1..=repetitions {
        let mut case_order: Vec<_> = (0..cases).collect();
        if randomize_cases {
            shuffle(&mut case_order, &mut state);
        }
        for case_index in case_order {
            let mut variant_order: Vec<_> = (0..variants).collect();
            shuffle(&mut variant_order, &mut state);
            tasks.extend(
                variant_order
                    .into_iter()
                    .map(|variant| (case_index, variant, repetition)),
            );
        }
    }
    tasks
}

pub fn run(
    suite: &EvalSuite,
    suite_dir: &Path,
    output: &Path,
    seed: u64,
) -> Result<Vec<EvalVariantRunResult>, EvalError> {
    let tasks = order(
        suite.cases.len(),
        suite.variants.len(),
        suite.run.repetitions,
        seed,
        suite.run.randomize_case_order,
    );
    write_json(
        output.join("schedule.json"),
        &serde_json::json!({"schema_version":1,"seed":seed,"tasks":tasks}),
    )?;
    let mut results = std::collections::BTreeMap::<(usize, usize), Vec<_>>::new();
    for (case_index, variant_index, repetition) in tasks {
        let case = &suite.cases[case_index];
        let variant = &suite.variants[variant_index];
        let directory = output
            .join("cases")
            .join(&case.id)
            .join("variants")
            .join(&variant.id)
            .join("repetitions")
            .join(format!("{repetition:04}"));
        fs::create_dir_all(&directory)?;
        let result = run_repetition(
            suite, suite_dir, output, &directory, case, variant, repetition,
        )?;
        write_json(directory.join("result.json"), &result)?;
        write_json(directory.join("metrics.json"), &result.measurements)?;
        append_observation(
            &directory,
            &EvalObservation {
                unix_ms: unix_ms(),
                source: "repetition_finished".into(),
                payload: serde_json::to_value(&result)?,
            },
        )?;
        let failed = !result.passed;
        results
            .entry((variant_index, case_index))
            .or_default()
            .push(result);
        if failed && suite.run.fail_fast {
            break;
        }
    }
    Ok(suite
        .variants
        .iter()
        .enumerate()
        .map(|(variant_index, variant)| {
            let cases = suite
                .cases
                .iter()
                .enumerate()
                .map(|(case_index, case)| {
                    let mut repetitions = results
                        .remove(&(variant_index, case_index))
                        .unwrap_or_default();
                    repetitions.sort_by_key(|result| result.repetition);
                    EvalCaseRunResult {
                        case_id: case.id.clone(),
                        variant_id: variant.id.clone(),
                        pass_rate: pass_rate(repetitions.iter().map(|result| result.passed)),
                        repetitions,
                    }
                })
                .collect();
            aggregate_variant(suite, variant, cases)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::order;
    #[test]
    fn seeds_reproduce_complete_interleaved_blocks() {
        let first = order(4, 3, 5, 42, true);
        assert_eq!(first, order(4, 3, 5, 42, true));
        assert_ne!(first, order(4, 3, 5, 43, true));
        assert_eq!(first.len(), 60);
        let identities: std::collections::BTreeSet<_> = first.iter().copied().collect();
        assert_eq!(identities.len(), 60);
        for block in first.chunks_exact(3) {
            assert!(
                block
                    .iter()
                    .all(|trial| trial.0 == block[0].0 && trial.2 == block[0].2)
            );
            assert_eq!(
                block
                    .iter()
                    .map(|trial| trial.1)
                    .collect::<std::collections::BTreeSet<_>>()
                    .len(),
                3
            );
        }
    }
}
