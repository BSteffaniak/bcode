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
        || plan.get("cwd").and_then(Value::as_str) != Some(check.workspace.as_str())
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
    let Some(files) = before.get("files").and_then(Value::as_array) else {
        return false;
    };
    !paths.is_empty()
        && paths.len() <= 64
        && paths.len() == files.len()
        && before.get("version") == Some(&Value::from(1))
        && before.get("workspace").and_then(Value::as_str) == Some(workspace)
        && result.get("content_after") == Some(before)
        && paths.iter().zip(files).all(|(path, file)| {
            path.as_str().is_some_and(|path| !path.is_empty())
                && file.get("path") == Some(path)
                && file
                    .get("sha256")
                    .and_then(Value::as_str)
                    .is_some_and(|digest| {
                        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
        })
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
            "admitted_input":{"version":2,"cwd":"/workspace","observe_files":["result.rs"],
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
