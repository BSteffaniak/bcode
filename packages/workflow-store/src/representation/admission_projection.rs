//! Allocation-bounded projection from the canonical immutable admission JSON.
//! This is not persisted or cached policy: every call reads the canonical document.
use super::{DatabaseValue, RepresentationError as Error};
#[derive(serde::Deserialize)]
struct LimitsOnly {
    limits: bcode_workflow::WorkflowRunLimits,
}
/// Decode only owned policy fields; serde still parses/skips the entire JSON and rejects syntax errors.
pub(super) fn limits(
    row: &switchy_database::Row,
) -> Result<bcode_workflow::WorkflowRunLimits, Error> {
    let Some(DatabaseValue::String(json)) = row.get("admission_json") else {
        return Err(Error::MaintenanceRequired);
    };
    Ok(serde_json::from_str::<LimitsOnly>(&json)
        .map_err(|_| Error::MaintenanceRequired)?
        .limits)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projection_preserves_limits_and_rejects_missing_or_corrupt_policy() {
        let mut run = super::super::tests::run("policy", false);
        run.input = Some(
            serde_json::json!({"large": (0..10_000).map(|n| serde_json::json!({"id":n,"text":"retained input data","values":[1,2,3]})).collect::<Vec<_>>() }),
        );
        let json = serde_json::to_string(&run).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..100 {
            let projected: LimitsOnly = serde_json::from_str(&json).unwrap();
            assert_eq!(projected.limits, run.limits);
        }
        eprintln!(
            "limits-only 100 x {} bytes: {:?}",
            json.len(),
            start.elapsed()
        );
        let start = std::time::Instant::now();
        for _ in 0..100 {
            let full: crate::NewWorkflowRun = serde_json::from_str(&json).unwrap();
            assert_eq!(full.limits, run.limits);
        }
        eprintln!("full admission 100: {:?}", start.elapsed());
        assert!(serde_json::from_str::<LimitsOnly>("{}").is_err());
        assert!(
            serde_json::from_str::<LimitsOnly>(r#"{"limits":{"concurrency_cap":"future"}}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<LimitsOnly>(&format!("{json} invalid")).is_err());
    }
}
