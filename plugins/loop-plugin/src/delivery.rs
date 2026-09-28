//! Loop-owned delivery evidence, retained in canonical evaluation outputs.
//!
//! These are evaluator claims with inspectable references, not execution receipts.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Optional versioned report. Older goal outputs omit it; unknown versions reject.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryReport {
    pub version: ReportVersion,
    #[schemars(length(min = 1, max = 64), inner(length(min = 1, max = 4096)))]
    pub integrated_targets: Vec<String>,
    #[schemars(length(max = 64), inner(length(min = 1, max = 4096)))]
    pub contribution_output_ids: Vec<String>,
    #[schemars(length(min = 1, max = 64))]
    pub criteria: Vec<Criterion>,
    #[schemars(length(max = 32))]
    pub checks: Vec<Check>,
    #[schemars(length(max = 64), inner(length(min = 1, max = 4096)))]
    pub retained_workspaces: Vec<String>,
    #[schemars(length(max = 64), inner(length(min = 1, max = 2048)))]
    pub unresolved_work: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ReportVersion {
    #[serde(rename = "1")]
    V1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Criterion {
    #[schemars(length(min = 1, max = 2048))]
    #[serde(rename = "criterion")]
    pub description: String,
    pub status: Observation,
    #[schemars(length(min = 1, max = 4096))]
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Check {
    #[schemars(length(min = 1, max = 2048))]
    pub command: String,
    #[schemars(length(min = 1, max = 4096))]
    pub workspace: String,
    pub outcome: Observation,
    #[schemars(length(min = 1, max = 4096))]
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Observation {
    Passed,
    Failed,
    Unverified,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn delivery_report_is_bounded_and_preserves_unverified_evidence() {
        let schema = serde_json::to_value(schemars::schema_for!(DeliveryReport)).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let mut value = json!({
            "version":"1", "integrated_targets":["workspace/integrated.sh"],
            "contribution_output_ids":["canonical-output-left", "canonical-output-right"],
            "criteria":[{"criterion":"total includes surcharge", "status":"unverified", "evidence":"combined check not run"}],
            "checks":[], "retained_workspaces":["worker-left"], "unresolved_work":["run combined validation"]
        });
        assert!(validator.is_valid(&value));
        let report: DeliveryReport = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(report.criteria[0].status, Observation::Unverified);
        value["version"] = json!("2");
        assert!(!validator.is_valid(&value));
        assert!(serde_json::from_value::<DeliveryReport>(value.clone()).is_err());
        value["version"] = json!("1");
        value["contribution_output_ids"] = json!(vec!["output"; 65]);
        assert!(!validator.is_valid(&value));
    }
}
