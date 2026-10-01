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
    /// V2 delivers these complete retained bytes, not a live workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_snapshot: Option<bcode_shell_models::DeliveredSnapshot>,
    /// V3 complete retained binary-safe repository export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_delivery: Option<bcode_shell_models::RepositoryDelivery>,
    #[schemars(length(min = 1, max = 64), inner(length(min = 1, max = 4096)))]
    pub integrated_targets: Vec<String>,
    /// Optional explicit scope for every named target. Claims only, never freshness evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 64))]
    pub content_scope: Option<Vec<DeliveredContent>>,
    /// V3 later integration judgments; historical canonical outputs remain unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64))]
    pub resolutions: Vec<ContributionResolution>,
    #[schemars(length(max = 64), inner(length(min = 1, max = 4096)))]
    pub contribution_output_ids: Vec<String>,
    #[schemars(length(min = 1, max = 64))]
    pub criteria: Vec<Criterion>,
    /// Exact original stop condition this report evaluates. Absence in historical V1
    /// reports means coverage is unknown, never that the original criteria were covered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 65536))]
    pub original_stop_condition: Option<String>,
    #[schemars(length(max = 32))]
    pub checks: Vec<Check>,
    #[schemars(length(max = 64), inner(length(min = 1, max = 4096)))]
    pub retained_workspaces: Vec<String>,
    #[schemars(length(max = 64), inner(length(min = 1, max = 2048)))]
    pub unresolved_work: Vec<String>,
}

impl DeliveryReport {
    pub(crate) fn snapshot_coverage(&self, stop_condition: &str) -> bool {
        (self.version == ReportVersion::V2 && self.repository_delivery.is_none()
            && self.delivered_snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.validate().is_ok()
                    && self.integrated_targets == snapshot.files.keys().cloned().collect::<Vec<_>>()
            }) || self.version == ReportVersion::V3 && self.delivered_snapshot.is_none()
                && self.repository_delivery.as_ref().is_some_and(|delivery| {
                    delivery.valid() && self.integrated_targets == vec![delivery.artifact.clone()]
                }))
            && self.identifies_original_criteria(stop_condition)
            && self.content_scope.is_none()
            // An explicit whole-objective judgment prevents a selected subset from
            // silently replacing the original stop condition. It remains a judgment.
            && self.criteria.iter().any(|criterion| criterion.description == stop_condition)
            && !self.checks.is_empty()
            && self.criteria.iter().all(|criterion| match criterion.basis {
                Some(CriterionBasis::ObservedCheck) => !criterion.check_indices.is_empty()
                    && criterion.check_indices.iter().all(|index| (*index as usize) < self.checks.len()),
                Some(CriterionBasis::Review) => criterion.check_indices.is_empty(),
                _ => false,
            })
    }
    pub(crate) fn identifies_original_criteria(&self, stop_condition: &str) -> bool {
        self.original_stop_condition.as_deref() == Some(stop_condition)
            && !stop_condition.trim().is_empty()
    }

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
            || self
                .content_scope
                .as_ref()
                .is_some_and(|scope| !self.covers_scope(scope))
            || self.resolutions.iter().any(|resolution| {
                self.version != ReportVersion::V3
                    || !self.contribution_output_ids.contains(&resolution.output_id)
            })
            || !self.unresolved_work.is_empty()
    }

    fn covers_scope(&self, scope: &[DeliveredContent]) -> bool {
        let targets: std::collections::BTreeSet<_> = self.integrated_targets.iter().collect();
        let declared: std::collections::BTreeSet<_> =
            scope.iter().map(|item| &item.target).collect();
        targets.len() == self.integrated_targets.len()
            && declared.len() == scope.len()
            && targets == declared
            && scope.iter().all(|item| {
                let roots: std::collections::BTreeSet<_> = item.roots.iter().collect();
                !roots.is_empty()
                    && roots.len() == item.roots.len()
                    && std::path::Path::new(&item.workspace).is_absolute()
                    && item.roots.iter().all(|root| {
                        let path = std::path::Path::new(root);
                        !root.is_empty()
                            && path.components().all(|component| {
                                matches!(component, std::path::Component::Normal(_))
                            })
                            && !root
                                .split('/')
                                .any(|part| part.is_empty() || part == "." || part == "..")
                            && !roots
                                .iter()
                                .any(|other| *other != root && path.starts_with(other))
                    })
                    && self.checks.iter().any(|check| {
                        check.workspace == item.workspace
                            && check.execution.as_ref().is_some_and(|execution| {
                                execution.content_roots.as_ref() == Some(&item.roots)
                            })
                    })
            })
    }
}

/// Explicit later review of all negative items in one immutable contribution.
/// This is a judgment backed by target checks, not a claim that checks prove prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContributionResolution {
    #[schemars(length(min = 1, max = 4096))]
    pub output_id: String,
    #[schemars(length(min = 64, max = 64))]
    pub checksum_sha256: String,
    /// Exact JSON pointers to every historical negative item being reviewed.
    #[schemars(length(min = 1, max = 128), inner(length(min = 1, max = 4096)))]
    pub item_paths: Vec<String>,
    #[schemars(length(min = 1, max = 4096))]
    pub evidence: String,
    #[schemars(length(min = 1, max = 32))]
    pub check_indices: Vec<u32>,
}

impl DeliveryReport {
    pub(crate) fn resolves(&self, output: &bcode_workflow::WorkflowOutputInspection) -> bool {
        let matching: Vec<_> = self
            .resolutions
            .iter()
            .filter(|resolution| resolution.output_id == output.output_id)
            .collect();
        let Some(items) = negative_items(&output.value) else {
            return false;
        };
        self.version == ReportVersion::V3
            && !items.is_empty()
            && self.repository_delivery.is_some()
            && output.schema_id == "bcode.delegated_task_result.v2"
            && output.schema_version == 1
            && matching.len() == 1
            && matching.iter().all(|resolution| {
                resolution.checksum_sha256 == output.checksum_sha256
                    && resolution.item_paths == items
                    && !resolution.evidence.trim().is_empty()
                    && !resolution.check_indices.is_empty()
                    && resolution.check_indices.iter().all(|index| {
                        self.checks.get(*index as usize).is_some_and(|check| {
                            check.outcome == Observation::Passed && check.execution.is_some()
                        })
                    })
            })
    }
}

fn negative_items(value: &serde_json::Value) -> Option<Vec<String>> {
    let mut items = Vec::new();
    for (index, blocker) in value.get("blockers")?.as_array()?.iter().enumerate() {
        blocker.as_str()?;
        items.push(format!("/blockers/{index}"));
    }
    if let Some(contributions) = value.get("contributions") {
        for (index, contribution) in contributions.as_array()?.iter().enumerate() {
            for (item, remaining) in contribution
                .get("remaining_work")?
                .as_array()?
                .iter()
                .enumerate()
            {
                remaining.as_str()?;
                items.push(format!("/contributions/{index}/remaining_work/{item}"));
            }
            if let Some(validation) = contribution.get("validation") {
                for (check, result) in validation.as_array()?.iter().enumerate() {
                    match result.get("outcome")?.as_str()? {
                        "passed" => {}
                        "failed" | "not_run" => {
                            items
                                .push(format!("/contributions/{index}/validation/{check}/outcome"));
                        }
                        _ => return None,
                    }
                }
            }
            match contribution.get("retention") {
                None => {} // Historical omission remains unknown, not retention proof.
                Some(retention) => match retention.as_str()? {
                    "retained" | "unknown" => {}
                    "removed" => items.push(format!("/contributions/{index}/retention")),
                    _ => return None,
                },
            }
        }
    }
    Some(items)
}

/// Explicit delivered target coverage, interpreted only by the loop domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveredContent {
    #[schemars(length(min = 1, max = 4096))]
    pub target: String,
    #[schemars(length(min = 1, max = 4096))]
    pub workspace: String,
    #[schemars(length(min = 1, max = 64), inner(length(min = 1, max = 4096)))]
    pub roots: Vec<String>,
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
        // Failed or unrun worker checks remain negative history until an explicit
        // target-bound later review resolves them. Unknown representations fail closed.
        contribution.get("validation").is_some_and(|validation| {
            validation.as_array().is_none_or(|checks| {
                checks.iter().any(|check| {
                    check.get("outcome").and_then(serde_json::Value::as_str) != Some("passed")
                })
            })
        }) ||
        // An explicit removal is negative evidence even when the worker omitted
        // remaining work. Absence/unknown retention remains unknown, not proof
        // that any contribution was retained or integrated.
        contribution.get("retention").is_some_and(|retention| {
            !matches!(retention.as_str(), Some("retained" | "unknown"))
        })
            || contribution
                .get("remaining_work")
                .and_then(serde_json::Value::as_array)
                .is_none_or(|remaining| !remaining.is_empty())
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ReportVersion {
    #[serde(rename = "1")]
    V1,
    #[serde(rename = "2")]
    V2,
    #[serde(rename = "3")]
    V3,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Criterion {
    #[schemars(length(min = 1, max = 65536))]
    #[serde(rename = "criterion")]
    pub description: String,
    pub status: Observation,
    /// How the evidence was obtained; omitted historical claims remain unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis: Option<CriterionBasis>,
    /// Indexes into this report's authenticated checks (required for observed basis).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32))]
    pub check_indices: Vec<u32>,
    #[schemars(length(min = 1, max = 4096))]
    pub evidence: String,
}

/// Review is an explicit judgment, not mechanically observed verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CriterionBasis {
    ObservedCheck,
    Review,
    Unknown,
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
    fn original_coverage_and_review_basis_remain_explicit_claims() {
        let value = json!({
            "version":"1", "integrated_targets":["result"],
            "contribution_output_ids":[],
            "criteria":[{"criterion":"behavior", "status":"passed", "evidence":"review"}],
            "checks":[], "retained_workspaces":[], "unresolved_work":[]
        });
        let mut report: DeliveryReport = serde_json::from_value(value).unwrap();
        assert!(!report.identifies_original_criteria("original"));
        assert_eq!(report.criteria[0].basis, None);
        report.original_stop_condition = Some("original".into());
        report.criteria[0].basis = Some(CriterionBasis::Review);
        assert!(report.identifies_original_criteria("original"));
        assert!(!report.identifies_original_criteria("narrowed"));
        let encoded = serde_json::to_value(&report).unwrap();
        assert_eq!(encoded["criteria"][0]["basis"], "review");
        assert_eq!(
            serde_json::from_value::<DeliveryReport>(encoded).unwrap(),
            report
        );
    }

    #[test]
    fn snapshot_coverage_preserves_whole_objective_and_evidence_basis() {
        let value = json!({
            "version":"2", "delivered_snapshot":{"version":1,"files":{"result.txt":"bytes"}},
            "integrated_targets":["result.txt"], "contribution_output_ids":[],
            "original_stop_condition":"whole objective",
            "criteria":[{"criterion":"whole objective","status":"passed","basis":"observed_check","check_indices":[0],"evidence":"check"}],
            "checks":[{"command":"test","workspace":"snapshot","outcome":"passed","evidence":"canonical"}],
            "retained_workspaces":[], "unresolved_work":[]
        });
        let report: DeliveryReport = serde_json::from_value(value.clone()).unwrap();
        assert!(report.snapshot_coverage("whole objective"));
        assert!(!report.snapshot_coverage("different original"));
        for (pointer, replacement) in [
            ("/version", json!("1")),
            ("/criteria/0/criterion", json!("narrowed")),
            ("/criteria/0/basis", json!("unknown")),
            ("/criteria/0/check_indices", json!([])),
            ("/criteria/0/check_indices", json!([1])),
            ("/integrated_targets", json!(["other"])),
            ("/delivered_snapshot/files", json!({})),
        ] {
            let mut changed = value.clone();
            *changed.pointer_mut(pointer).unwrap() = replacement;
            let report: DeliveryReport = serde_json::from_value(changed).unwrap();
            assert!(!report.snapshot_coverage("whole objective"), "{pointer}");
        }
    }

    #[test]
    fn explicit_scope_requires_complete_unambiguous_checked_targets() {
        let value = json!({
            "version":"1", "integrated_targets":["result"],
            "content_scope":[{"target":"result", "workspace":"/repo", "roots":["src"]}],
            "contribution_output_ids":[],
            "criteria":[{"criterion":"behavior", "status":"passed", "evidence":"review"}],
            "checks":[{"command":"test", "workspace":"/repo", "outcome":"passed", "evidence":"receipt",
                "execution":{"output_id":"check", "command_index":0, "argv":["test"], "content_roots":["src"]}}],
            "retained_workspaces":[], "unresolved_work":[]
        });
        let report: DeliveryReport = serde_json::from_value(value.clone()).unwrap();
        assert!(!report.precludes_completion()); // Coverage claim, not verification.
        for (pointer, replacement) in [
            ("/integrated_targets", json!(["result", "missing"])),
            ("/integrated_targets", json!(["result", "result"])),
            ("/content_scope/0/target", json!("other")),
            ("/content_scope/0/workspace", json!("repo")),
            ("/content_scope/0/roots", json!(["src", "src/file"])),
            ("/content_scope/0/roots", json!(["../src"])),
            ("/content_scope/0/roots", json!(["src/./file"])),
            ("/content_scope/0/roots", json!(["src", "src"])),
            ("/checks/0/execution/content_roots", json!(["other"])),
            ("/checks/0/workspace", json!("/other")),
            ("/checks/0/execution", json!(null)),
        ] {
            let mut invalid = value.clone();
            *invalid.pointer_mut(pointer).unwrap() = replacement;
            let report: DeliveryReport = serde_json::from_value(invalid).unwrap();
            assert!(report.precludes_completion(), "{pointer}");
        }
    }

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
        output.value["contributions"][0]["retention"] = json!("removed");
        assert!(contribution_precludes_completion(&output));
        output.value["contributions"][0]["retention"] = json!("unknown");
        assert!(!contribution_precludes_completion(&output)); // Unknown is not certification.
        output.value["contributions"][0]["retention"] = json!("retained");
        assert!(!contribution_precludes_completion(&output)); // A claim is not certification.
        for validation in [
            json!([{"outcome":"failed"}]),
            json!([{"outcome":"not_run"}]),
            json!([{"outcome":"future"}]),
            json!([{}]),
            json!(null),
        ] {
            output.value["contributions"][0]["validation"] = validation;
            assert!(contribution_precludes_completion(&output));
        }
        output.value["contributions"][0]["validation"] = json!([{"outcome":"passed"}]);
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
    fn repository_coverage_keeps_original_criteria_and_version_boundaries() {
        let mut value = json!({
            "version":"3", "repository_delivery":{"target":{"version":1,"commit":"a".repeat(40)},"artifact":"retained-export","sha256":"b".repeat(64)},
            "integrated_targets":["retained-export"], "contribution_output_ids":[],
            "original_stop_condition":"whole objective",
            "criteria":[{"criterion":"whole objective","status":"passed","basis":"observed_check","check_indices":[0],"evidence":"canonical check"}],
            "checks":[{"command":"test","workspace":"private target","outcome":"passed","evidence":"canonical"}],
            "retained_workspaces":[],"unresolved_work":[]
        });
        let report: DeliveryReport = serde_json::from_value(value.clone()).unwrap();
        assert!(report.snapshot_coverage("whole objective"));
        assert!(!report.snapshot_coverage("different objective"));
        for version in ["1", "2"] {
            value["version"] = json!(version);
            let report: DeliveryReport = serde_json::from_value(value.clone()).unwrap();
            assert!(!report.snapshot_coverage("whole objective"));
        }
    }

    #[test]
    fn later_resolution_requires_exact_history_items_and_checked_target() {
        let output: bcode_workflow::WorkflowOutputInspection = serde_json::from_value(json!({
            "version":1,"output_id":"worker","run_id":"run","node_id":"worker",
            "activation_id":"activation","schema_id":"bcode.delegated_task_result.v2",
            "schema_version":1,"checksum_sha256":"a".repeat(64),"created_at_ms":1,
            "value":{"blockers":["integrate"],"contributions":[{"remaining_work":["test"]}]}
        }))
        .unwrap();
        let value = json!({
            "version":"3", "repository_delivery":{"target":{"version":1,"commit":"a".repeat(40)},"artifact":"export","sha256":"b".repeat(64)},
            "integrated_targets":["export"],"contribution_output_ids":["worker"],
            "criteria":[],"checks":[{"command":"test","workspace":"target","outcome":"passed","evidence":"checked","execution":{"output_id":"check","command_index":0,"argv":["test"]}}],
            "retained_workspaces":[],"unresolved_work":[],
            "resolutions":[{"output_id":"worker","checksum_sha256":"a".repeat(64),"item_paths":["/blockers/0","/contributions/0/remaining_work/0"],"evidence":"Integrated and checked; review judgment","check_indices":[0]}]
        });
        let report: DeliveryReport = serde_json::from_value(value.clone()).unwrap();
        assert!(report.resolves(&output));
        assert!(contribution_precludes_completion(&output)); // History is never rewritten.
        for retention in [json!("future"), json!(null), json!(true), json!({})] {
            let mut history = output.clone();
            history.value["contributions"][0]["retention"] = retention;
            assert!(contribution_precludes_completion(&history));
            assert!(!report.resolves(&history));
        }
        let mut removed = output.clone();
        removed.value["contributions"][0]["retention"] = json!("removed");
        assert!(contribution_precludes_completion(&removed));
        assert!(!report.resolves(&removed));
        let mut resolved = value.clone();
        resolved["resolutions"][0]["item_paths"] = json!([
            "/blockers/0",
            "/contributions/0/remaining_work/0",
            "/contributions/0/retention"
        ]);
        let resolved: DeliveryReport = serde_json::from_value(resolved).unwrap();
        assert!(resolved.resolves(&removed));
        assert!(contribution_precludes_completion(&removed));
        for outcome in ["failed", "not_run"] {
            let mut history = output.clone();
            history.value["contributions"][0]["validation"] = json!([{"outcome":outcome}]);
            assert!(contribution_precludes_completion(&history));
            assert!(!report.resolves(&history));
            let mut resolved = value.clone();
            resolved["resolutions"][0]["item_paths"] = json!([
                "/blockers/0",
                "/contributions/0/remaining_work/0",
                "/contributions/0/validation/0/outcome"
            ]);
            let resolved: DeliveryReport = serde_json::from_value(resolved).unwrap();
            assert!(resolved.resolves(&history));
            assert!(contribution_precludes_completion(&history));
            history.value["contributions"][0]["validation"][0]["outcome"] = json!("future");
            assert!(!resolved.resolves(&history));
        }
        for (pointer, replacement) in [
            ("/version", json!("2")),
            ("/resolutions/0/checksum_sha256", json!("b".repeat(64))),
            ("/resolutions/0/item_paths", json!(["/blockers/0"])),
            ("/resolutions/0/check_indices", json!([1])),
            ("/checks/0/outcome", json!("failed")),
            ("/checks/0/execution", json!(null)),
            ("/resolutions/0/evidence", json!(" ")),
        ] {
            let mut invalid = value.clone();
            *invalid.pointer_mut(pointer).unwrap() = replacement;
            let report: DeliveryReport = serde_json::from_value(invalid).unwrap();
            assert!(!report.resolves(&output), "{pointer}");
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
        value["version"] = json!("4");
        assert!(!validator.is_valid(&value));
        assert!(serde_json::from_value::<DeliveryReport>(value.clone()).is_err());
        value["version"] = json!("1");
        value["contribution_output_ids"] = json!(vec!["output"; 65]);
        assert!(!validator.is_valid(&value));
    }
}
