//! Loop interpretation of authenticated shell evidence, not a freshness certificate.
use super::delivery::{Check, CheckExecution};
use bcode_workflow::{
    NodeKind, WORKFLOW_OUTPUT_INSPECTION_VERSION, WorkflowBlockDefinition,
    WorkflowNodeDataflowPolicy, WorkflowOutputExecutionEvidence, WorkflowOutputProvenance,
};
use serde_json::Value;

/// Only direct advanced command plans are supported. Scripts and adapted inputs fail closed.
/// This establishes selected-file observations at execution time, not complete input coverage
/// or current filesystem freshness. The positive-delivery safeguard must remain in force.
pub fn observed_check(
    evidence: &WorkflowOutputExecutionEvidence,
    check: &Check,
    reference: &CheckExecution,
) -> bool {
    let provenance = &evidence.provenance;
    let output = &provenance.output;
    let producer = &provenance.producer;
    let Ok(block) =
        serde_json::from_value::<WorkflowBlockDefinition>(producer.configuration.clone())
    else {
        return false;
    };
    if evidence.version != WorkflowOutputExecutionEvidence::VERSION
        || provenance.version != WorkflowOutputProvenance::VERSION
        || provenance.producer_revision == 0
        || output.version != WORKFLOW_OUTPUT_INSPECTION_VERSION
        || output.output_id != reference.output_id
        || output.node_id != producer.id
        || output.checksum_sha256.len() != 64
        || !output
            .checksum_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || producer.kind != NodeKind::PluginBlock
        || producer.dataflow != WorkflowNodeDataflowPolicy::Direct
        || block.plugin_id != "bcode.shell"
        || block.block_id != "exec"
        || block.block_version != 1
        || block.operation != "exec"
        || output.schema_id != "bcode.shell.exec-result/v1"
        || output.schema_version != 1
    {
        return false;
    }
    let plan = &evidence.admitted_input;
    let result = &output.value;
    if plan.get("version") != Some(&Value::from(2))
        || result.get("version") != Some(&Value::from(2))
        || result.get("passed") != Some(&Value::Bool(true))
        // The shell owner admits a relative cwd and records its resolved absolute
        // identity in content_before/after. Comparing the relative plan path to
        // the reported workspace would reject every production observation.
        || !plan.get("cwd").and_then(Value::as_str).is_some_and(|cwd| {
            let path = std::path::Path::new(cwd);
            !path.is_absolute()
                && !path.components().any(|component| {
                    matches!(component, std::path::Component::ParentDir
                        | std::path::Component::RootDir | std::path::Component::Prefix(_))
                })
        })
    {
        return false;
    }
    let Some(commands) = plan.get("commands").and_then(Value::as_array) else {
        return false;
    };
    let Some(results) = result.get("commands").and_then(Value::as_array) else {
        return false;
    };
    if commands.is_empty() || commands.len() != results.len() || commands.len() > 64 {
        return false;
    }
    let index = reference.command_index as usize;
    let Some(command) = commands.get(index) else {
        return false;
    };
    if reference.argv.is_empty()
        || command.get("argv") != Some(&serde_json::json!(reference.argv))
        || !commands
            .iter()
            .zip(results)
            .enumerate()
            .all(|(index, (command, result))| {
                result.get("index").and_then(Value::as_u64) == Some(index as u64)
                    && result.get("status").and_then(Value::as_str) == Some("exited")
                    && result.get("exit_code") == Some(&Value::from(0))
                    && result.get("signal") == Some(&Value::Null)
                    && result.get("exit_accepted") == Some(&Value::Bool(true))
                    && command.get("accepted_exit_codes") == Some(&serde_json::json!([0]))
                    && result.get("accepted_exit_codes") == Some(&serde_json::json!([0]))
            })
    {
        return false;
    }
    if reference.content_roots.as_ref().is_some_and(|roots| {
        roots.is_empty()
            || roots.len() > 64
            || plan.get("observe_files") != Some(&serde_json::json!(roots))
    }) {
        return false;
    }
    unchanged_observation(plan, result, &check.workspace)
}

fn unchanged_observation(plan: &Value, result: &Value, workspace: &str) -> bool {
    let Some(before) = result
        .get("content_before")
        .filter(|value| value.is_object())
    else {
        return false;
    };
    let Some(paths) = plan.get("observe_files").and_then(Value::as_array) else {
        return false;
    };
    observation_matches_paths(before, paths)
        && before.get("workspace").and_then(Value::as_str) == Some(workspace)
        && result.get("content_after") == Some(before)
}

/// Interpret the shell owner's bounded scope manifest, not evaluator-supplied coverage.
fn observation_matches_paths(observation: &Value, paths: &[Value]) -> bool {
    let Some(files) = observation.get("files").and_then(Value::as_array) else {
        return false;
    };
    let Some(roots) = paths
        .iter()
        .map(normalized_path)
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    if roots.is_empty() || roots.len() > 64 {
        return false;
    }
    let mut entries = std::collections::BTreeSet::new();
    if !files.iter().all(|file| {
        file.get("path")
            .and_then(normalized_path)
            .is_some_and(|path| entries.insert(path))
            && file
                .get("sha256")
                .and_then(Value::as_str)
                .is_some_and(|digest| {
                    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
    }) {
        return false;
    }
    match observation.get("version").and_then(Value::as_u64) {
        Some(1) => {
            observation
                .get("directories")
                .is_none_or(|value| value.as_array().is_some_and(Vec::is_empty))
                && paths.len() == files.len()
                && paths
                    .iter()
                    .zip(files)
                    .all(|(path, file)| file.get("path") == Some(path))
        }
        Some(2) => {
            let Some(directories) = observation.get("directories").and_then(Value::as_array) else {
                return false;
            };
            let Some(directories) = directories
                .iter()
                .map(normalized_path)
                .collect::<Option<std::collections::BTreeSet<_>>>()
            else {
                return false;
            };
            if directories.is_empty()
                || directories.len() != observation["directories"].as_array().map_or(0, Vec::len)
                || !directories.iter().all(|path| entries.insert(*path))
                || entries.len() > 64
            {
                return false;
            }
            // Every selected root must exist; every observed descendant must have one
            // root and an observed directory parent. This rejects overlaps and gaps.
            roots.iter().all(|root| entries.contains(root))
                && entries.iter().all(|entry| {
                    roots.iter().filter(|root| entry.starts_with(root)).count() == 1
                        && (roots.contains(entry)
                            || entry
                                .parent()
                                .is_some_and(|parent| directories.contains(parent)))
                })
        }
        _ => false,
    }
}

fn normalized_path(value: &Value) -> Option<&std::path::Path> {
    let text = value.as_str()?;
    let path = std::path::Path::new(text);
    (!text.is_empty()
        && path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && !text.contains("//")
        && !text.ends_with('/')
        && !text
            .split('/')
            .any(|component| component == "." || component == ".."))
    .then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (WorkflowOutputExecutionEvidence, Check) {
        let observation = json!({"version":1,"workspace":"/workspace",
            "files":[{"path":"result.rs","sha256":"a".repeat(64)}]});
        let evidence = serde_json::from_value(json!({
            "version":1,
            "provenance": {
                "version":1,"producer_revision":1,
                "producer": {"id":"check","name":"Check","kind":"plugin_block",
                    "input":{"type_name":"input","schema":{}},
                    "output":{"type_name":"output","schema":{}},
                    "configuration": {
                        "block_id":"exec","block_version":1,"plugin_id":"bcode.shell",
                        "operation":"exec","input":{"type_name":"input","schema":{}},
                        "output":{"type_name":"output","schema":{}},"effect":"mutating",
                        "authorization":{"capability":"mutating","explicit_grant_required":true},
                        "timeout_ms":1000,"cancellation_supported":true,"reconciliation":"repair_required"
                    }},
                "output":{"version":1,"output_id":"check-output","run_id":"run","node_id":"check",
                    "activation_id":"activation","schema_id":"bcode.shell.exec-result/v1",
                    "schema_version":1,"checksum_sha256":"a".repeat(64),"created_at_ms":1,
                    "value":{"version":2,"passed":true,"content_before":observation,
                        "content_after":observation,"commands":[{"index":0,"status":"exited",
                            "exit_code":0,"signal":null,"accepted_exit_codes":[0],"exit_accepted":true}]}}
            },
            "admitted_input":{"version":2,"cwd":".","observe_files":["result.rs"],
                "commands":[{"argv":["cargo","test"],"accepted_exit_codes":[0]}]}
        })).unwrap();
        let check =
            serde_json::from_value(json!({"command":"display only","workspace":"/workspace",
            "outcome":"passed","evidence":"not trusted","execution":{
                "output_id":"check-output","command_index":0,"argv":["cargo","test"]}}))
            .unwrap();
        (evidence, check)
    }

    #[test]
    fn relative_execution_directory_uses_shell_resolved_workspace_identity() {
        let (mut evidence, mut check) = fixture();
        for cwd in [".", "subdir", ""] {
            evidence.admitted_input["cwd"] = json!(cwd);
            assert!(observed_check(
                &evidence,
                &check,
                check.execution.as_ref().unwrap()
            ));
        }
        for cwd in [
            json!("/workspace"),
            json!("../escape"),
            json!("sub/../../escape"),
            json!(null),
        ] {
            evidence.admitted_input["cwd"] = cwd;
            assert!(!observed_check(
                &evidence,
                &check,
                check.execution.as_ref().unwrap()
            ));
        }
        evidence.admitted_input["cwd"] = json!(".");
        check.workspace = "/other-workspace".into();
        assert!(!observed_check(
            &evidence,
            &check,
            check.execution.as_ref().unwrap()
        ));
        check.workspace = ".".into();
        assert!(!observed_check(
            &evidence,
            &check,
            check.execution.as_ref().unwrap()
        ));
    }

    #[test]
    fn explicit_content_roots_must_match_the_admitted_observation_scope() {
        let (evidence, mut check) = fixture();
        for roots in [
            vec![],
            vec!["other.rs".into()],
            vec!["result.rs".into(), "other.rs".into()],
            vec!["result.rs".into(), "result.rs".into()],
            vec!["./result.rs".into()],
        ] {
            check.execution.as_mut().unwrap().content_roots = Some(roots);
            assert!(!observed_check(
                &evidence,
                &check,
                check.execution.as_ref().unwrap()
            ));
        }
        check.execution.as_mut().unwrap().content_roots = Some(vec!["result.rs".into()]);
        assert!(observed_check(
            &evidence,
            &check,
            check.execution.as_ref().unwrap()
        ));
        check.execution.as_mut().unwrap().content_roots = None;
        assert!(observed_check(
            &evidence,
            &check,
            check.execution.as_ref().unwrap()
        ));
    }

    #[test]
    fn directory_observations_bind_scope_membership_and_content() {
        let (mut evidence, check) = fixture();
        evidence.admitted_input["observe_files"] = json!(["src", "empty"]);
        let observation = json!({"version":2,"workspace":"/workspace",
            "directories":["empty", "src", "src/nested"],
            "files":[{"path":"src/nested/result.rs","sha256":"a".repeat(64)}]});
        evidence.provenance.output.value["content_before"] = observation.clone();
        evidence.provenance.output.value["content_after"] = observation.clone();
        let reference = check.execution.as_ref().unwrap();
        assert!(observed_check(&evidence, &check, reference));
        for (field, value) in [
            ("version", json!(3)),
            ("directories", json!(["empty", "src"])),
            ("directories", json!(["empty", "src", "src", "src/nested"])),
            ("directories", json!(["src", "src/nested"])),
            (
                "directories",
                json!(["empty", "src", "src/nested", "other"]),
            ),
            (
                "files",
                json!([{"path":"src/../result.rs","sha256":"a".repeat(64)}]),
            ),
            (
                "files",
                json!([{"path":"src/nested/result.rs","sha256":"invalid"}]),
            ),
        ] {
            let mut changed = evidence.clone();
            changed.provenance.output.value["content_before"][field] = value.clone();
            changed.provenance.output.value["content_after"][field] = value;
            assert!(!observed_check(&changed, &check, reference), "{field}");
        }
        for paths in [
            json!(["src"]),
            json!(["src", "src/nested", "empty"]),
            json!(["src", "src", "empty"]),
        ] {
            let mut changed = evidence.clone();
            changed.admitted_input["observe_files"] = paths;
            assert!(!observed_check(&changed, &check, reference));
        }
        evidence.provenance.output.value["content_after"]["files"][0]["sha256"] =
            json!("b".repeat(64));
        assert!(!observed_check(&evidence, &check, reference));
        evidence.provenance.output.value["content_after"] = observation;
        evidence.provenance.output.value["content_after"]["directories"] =
            json!(["src", "src/nested"]);
        assert!(!observed_check(&evidence, &check, reference));
    }

    #[test]
    fn selected_file_observation_requires_exact_owner_plan_and_success() {
        let (evidence, check) = fixture();
        let reference = check.execution.as_ref().unwrap();
        assert!(observed_check(&evidence, &check, reference));
        for (pointer, replacement) in [
            ("/version", json!(2)),
            ("/provenance/producer/kind", json!("agent")),
            (
                "/provenance/producer/configuration/plugin_id",
                json!("other"),
            ),
            ("/provenance/producer/dataflow", json!("state_envelope_v1")),
            ("/provenance/output/value/passed", json!(false)),
            ("/provenance/output/value/commands/0/exit_code", json!(1)),
            ("/provenance/output/value/content_after", Value::Null),
            ("/provenance/output/value/content_before/version", json!(2)),
            ("/admitted_input/commands/0/argv", json!(["cargo", "check"])),
            ("/admitted_input/observe_files", json!([])),
            ("/admitted_input/cwd", json!("/other")),
        ] {
            let mut value = serde_json::to_value(&evidence).unwrap();
            // Dataflow defaults are omitted in serialization; insert that field explicitly.
            if pointer.ends_with("/dataflow") {
                value["provenance"]["producer"]["dataflow"] = replacement;
            } else {
                *value.pointer_mut(pointer).unwrap() = replacement;
            }
            let changed = serde_json::from_value(value).unwrap();
            assert!(!observed_check(&changed, &check, reference), "{pointer}");
        }
    }
}
