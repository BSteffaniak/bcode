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

impl DeliveryReport {
    /// Claims can rule out completion, but cannot establish verified delivery.
    pub(crate) fn precludes_completion(&self) -> bool {
        self.integrated_targets.is_empty()
            || self
                .integrated_targets
                .iter()
                .any(|target| target.trim().is_empty())
            || self
                .contribution_output_ids
                .iter()
                .any(|id| id.trim().is_empty())
            || self.criteria.is_empty()
            || self.criteria.iter().any(|criterion| {
                criterion.description.trim().is_empty()
                    || criterion.status != Observation::Passed
                    || criterion.evidence.trim().is_empty()
            })
            || self.checks.iter().any(|check| {
                check.command.trim().is_empty()
                    || check.workspace.trim().is_empty()
                    || check.outcome != Observation::Passed
                    || check.evidence.trim().is_empty()
            })
            || !self.unresolved_work.is_empty()
    }
}

/// Canonical worker claims can disprove delivery, never certify it. Unknown
/// schemas remain unknown; custom worker schemas are an authorable choice.
pub fn contribution_precludes_completion(
    output: &bcode_workflow::WorkflowOutputInspection,
) -> bool {
    if output.schema_id != "bcode.delegated_task_result.v2" {
        return false;
    }
    if output.schema_version != 1 {
        return true;
    }
    let Some(blockers) = output
        .value
        .get("blockers")
        .and_then(serde_json::Value::as_array)
    else {
        return true;
    };
    if !blockers.is_empty() {
        return true;
    }
    let Some(contributions) = output.value.get("contributions") else {
        return false;
    };
    let Some(contributions) = contributions.as_array() else {
        return true;
    };
    contributions.iter().any(|contribution| {
        contribution
            .get("remaining_work")
            .and_then(serde_json::Value::as_array)
            .is_none_or(|remaining| !remaining.is_empty())
    })
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
    /// Exact canonical shell output and argv; prose command text is display-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<CheckExecution>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckExecution {
    #[schemars(length(min = 1, max = 4096))]
    pub output_id: String,
    pub command_index: u32,
    #[schemars(length(min = 1, max = 128), inner(length(max = 65536)))]
    pub argv: Vec<String>,
    /// Explicit selected content roots, matched exactly against the admitted check plan.
    /// Absence preserves historical reports; neither presence nor a match proves freshness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 64), inner(length(min = 1, max = 4096)))]
    pub content_roots: Option<Vec<String>>,
    /// Optional later observation-only shell output matching the checked content.
    /// This authenticates re-observation, not delivery-time freshness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 4096))]
    pub observation_output_id: Option<String>,
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
    fn canonical_contribution_blockers_cannot_be_hidden_by_delivery_claims() {
        let mut output: bcode_workflow::WorkflowOutputInspection = serde_json::from_value(json!({
            "version":1, "output_id":"output", "run_id":"run", "node_id":"worker",
            "activation_id":"activation", "schema_id":"bcode.delegated_task_result.v2",
            "schema_version":1, "checksum_sha256":"a".repeat(64), "created_at_ms":1,
            "value":{"blockers":[], "contributions":[{"remaining_work":[]}]}
        }))
        .unwrap();
        assert!(!contribution_precludes_completion(&output));
        output.value["blockers"] = json!(["permission denied"]);
        assert!(contribution_precludes_completion(&output));
        output.value["blockers"] = json!([]);
        output.value["contributions"][0]["remaining_work"] = json!(["conflict unresolved"]);
        assert!(contribution_precludes_completion(&output));
        output.value["contributions"][0]["remaining_work"] = json!(null);
        assert!(contribution_precludes_completion(&output));
        output.value = json!({"blockers":[]});
        assert!(!contribution_precludes_completion(&output));
        output.value = json!({});
        assert!(contribution_precludes_completion(&output));
        output.schema_id = "custom.worker".into();
        assert!(!contribution_precludes_completion(&output));
    }

    #[test]
    fn negative_delivery_claims_preclude_completion() {
        let value = json!({
            "version":"1", "integrated_targets":["integrated.sh"],
            "contribution_output_ids":[],
            "criteria":[{"criterion":"combined behavior", "status":"passed", "evidence":"observed"}],
            "checks":[{"command":"test", "workspace":"repo", "outcome":"passed", "evidence":"exit 0"}],
            "retained_workspaces":[], "unresolved_work":[]
        });
        let report: DeliveryReport = serde_json::from_value(value.clone()).unwrap();
        assert!(!report.precludes_completion()); // Not proof of completion.
        for (pointer, replacement) in [
            ("/criteria/0/criterion", json!(" \t\n")),
            ("/criteria/0/evidence", json!("\u{2003}")),
            ("/checks/0/command", json!(" \t")),
            ("/checks/0/workspace", json!("\n")),
            ("/integrated_targets", json!(["valid", " \t"])),
            ("/contribution_output_ids", json!(["valid", "\u{2003}"])),
            ("/criteria/0/status", json!("failed")),
            ("/criteria/0/status", json!("unverified")),
            ("/checks/0/outcome", json!("failed")),
            ("/checks/0/outcome", json!("unverified")),
            ("/checks/0/evidence", json!(" ")),
            ("/unresolved_work", json!(["conflict"])),
            ("/criteria", json!([])),
            ("/integrated_targets", json!([])),
        ] {
            let mut invalid = value.clone();
            *invalid.pointer_mut(pointer).unwrap() = replacement;
            let report: DeliveryReport = serde_json::from_value(invalid).unwrap();
            assert!(report.precludes_completion(), "{pointer}");
        }
    }

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
