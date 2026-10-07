//! Explicit service-vocabulary preflight, separate from independent semantic scoring.
use super::{BenchmarkPlan, Oracle};
use bcode_model::judgement::Feature;

/// Check producer triage vocabulary before treating a semantic plan as a service oracle.
/// Independent editing truth may use other labels; it must not silently become service truth.
///
/// # Errors
/// Returns bounded task identities for unmapped labels. Callers must surface these as oracle
/// mapping gaps, not model failures, and must not run qualifying service comparisons with them.
pub fn validate_service_oracle_vocabulary(plan: &BenchmarkPlan) -> Result<(), String> {
    for task in &plan.tasks {
        if task.feature != Feature::ToolResultTriage {
            continue;
        }
        if let Oracle::Classification { labels, .. } = &task.oracle
            && labels.iter().any(|(label, credit)| {
                !(matches!(
                    label.as_str(),
                    "code" | "environment" | "auth" | "transient" | "partial" | "inconclusive"
                ) || (label == "__negative_control__" && *credit <= 0.0))
            })
        {
            return Err(format!(
                "{}: triage producer vocabulary mapping gap",
                task.task_id
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn independent_labels_require_explicit_mapping_before_service_comparison() {
        let mut plan = super::super::tests::plan();
        for label in ["test", "permission", "invented"] {
            plan.tasks[0].oracle = Oracle::Classification {
                labels: BTreeMap::from([(label.into(), 1.0)]),
                abstention_credit: 0.0,
            };
            assert!(
                validate_service_oracle_vocabulary(&plan)
                    .unwrap_err()
                    .contains("mapping gap")
            );
        }
        for label in [
            "code",
            "auth",
            "environment",
            "transient",
            "partial",
            "inconclusive",
        ] {
            plan.tasks[0].oracle = Oracle::Classification {
                labels: BTreeMap::from([(label.into(), 1.0)]),
                abstention_credit: 0.0,
            };
            assert!(validate_service_oracle_vocabulary(&plan).is_ok());
        }
    }
}
