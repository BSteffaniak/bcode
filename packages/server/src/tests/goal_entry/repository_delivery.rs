//! Scripted model choices through normal goal admission, semantic delegation and shell ownership.
use super::*;

#[derive(Clone, Copy, Debug)]
pub(super) enum Case {
    Complete,
    MissingChecksum,
    WrongChecksum,
    IncompleteItems,
    DuplicateItems,
    MissingCheck,
    WrongArgv,
    WrongTarget,
}

fn reference(index: usize, pointer: &str) -> serde_json::Value {
    serde_json::json!({"$fake_result":{"index":index,"pointer":pointer}})
}

pub(super) fn handoff(workspace: &Path, condition: &serde_json::Value, case: Case) -> String {
    let repository = workspace.join("delivery-repository");
    std::fs::create_dir(&repository).unwrap();
    let workspace = repository.as_path();
    // A real immutable fixture commit, not a mocked export or successful check.
    std::fs::write(
        workspace.join("integrated.sh"),
        format!("{LEFT_MODULE}{RIGHT_MODULE}"),
    )
    .unwrap();
    for args in [
        vec!["init"],
        vec!["add", "integrated.sh"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "combined fixture",
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(workspace)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    let oid = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace)
        .output()
        .unwrap();
    assert!(oid.status.success());
    let commit = String::from_utf8(oid.stdout).unwrap().trim().to_owned();
    let argv = serde_json::json!(["/bin/sh", "-c", COMBINED_CHECK]);
    let inspect = |index, node| serde_json::json!({"$fake_result":{"index":index,"pointer":"/outputs","where":{"node_id":node},"select":"/inspection_arguments"}});
    let mut report = serde_json::json!({
        "version":"3", "original_stop_condition":condition,
        "repository_delivery":reference(1,"/output/value/repository_verification/delivery"),
        "integrated_targets":[reference(1,"/output/value/repository_verification/delivery/artifact")],
        "contribution_output_ids":[reference(0,"/output/output_id")],
        "resolutions":[{"output_id":reference(0,"/output/output_id"),"checksum_sha256":reference(0,"/output/checksum_sha256"),"item_paths":["/blockers/0","/blockers/1"],"evidence":"Reviewed historical blockers against retained combined repository check","check_indices":[0]}],
        "criteria":[{"criterion":condition,"status":"passed","basis":"observed_check","check_indices":[0],"evidence":"Combined exported fixture passed"}],
        "checks":[{"command":COMBINED_CHECK,"workspace":workspace,"outcome":"passed","evidence":"Canonical direct shell check","execution":{"output_id":reference(1,"/output/output_id"),"command_index":0,"argv":argv}}],
        "unresolved_work":[],"retained_workspaces":[workspace]
    });
    match case {
        Case::Complete => {}
        Case::MissingChecksum => {
            report["resolutions"][0]
                .as_object_mut()
                .unwrap()
                .remove("checksum_sha256");
        }
        Case::WrongChecksum => {
            report["resolutions"][0]["checksum_sha256"] = serde_json::json!("a".repeat(64));
        }
        Case::IncompleteItems => {
            report["resolutions"][0]["item_paths"] = serde_json::json!(["/blockers/0"]);
        }
        Case::DuplicateItems => {
            report["resolutions"][0]["item_paths"] =
                serde_json::json!(["/blockers/0", "/blockers/0"]);
        }
        Case::MissingCheck => report["checks"] = serde_json::json!([]),
        Case::WrongArgv => report["checks"][0]["execution"]["argv"] = serde_json::json!(["true"]),
        Case::WrongTarget => report["integrated_targets"] = serde_json::json!(["unrelated-export"]),
    }
    let objective = format!(
        "tool-call workflow.execution_context {{\"outputs_only\":true,\"limit\":3,\"$fake_json_pages\":{{\"items\":\"/outputs\",\"next\":\"/next_page_arguments\"}}}}\ntool-call workflow.execution_context {}\ntool-call workflow.execution_context {}\nstructured-result {report}",
        inspect(0, "repository-regression.repository"),
        inspect(1, "left")
    );
    let stage = serde_json::json!({"run_id":reference(0,"/run_id"),"expected_revision":reference(0,"/graph/revision"),"bind_source_activation":reference(0,"/activation_id"),"mutation_id":"repository-regression","repository_target":{"version":1,"commit":commit},"cwd":"delivery-repository","commands":[argv],"report_objective":objective,"reconciliation":[]});
    format!(
        "\ntool-call workflow.execution_context {{\"compact\":true,\"limit\":1}}\ntool-call loop.stage_repository_verification {stage}\ntool-call workflow.publish_run_graph_edit {}",
        reference(0, "/publication_arguments")
    )
}

pub(super) fn install(request: &mut PluginWorkflowStartRequest, workspace: &Path, _: Case) {
    let evaluation = request.definition.nodes.get_mut("loop.evaluation").unwrap();
    let inspect = serde_json::json!({"$fake_result":{"index":0,"pointer":"/outputs","where":{"node_id":"repository-regression.report"},"select":"/inspection_arguments"}});
    let instructions = evaluation.configuration["system_prompt"]
        .as_str()
        .unwrap()
        .split("\ntool-call workflow.execution_context")
        .next()
        .unwrap()
        .to_owned();
    let read = serde_json::json!({"path":workspace.join("integrated.sh"),"offset":1,"limit":100});
    evaluation.configuration["system_prompt"] = serde_json::json!(format!(
        "{instructions}\ntool-call workflow.execution_context {{\"outputs_only\":true,\"limit\":3,\"$fake_json_pages\":{{\"items\":\"/outputs\",\"next\":\"/next_page_arguments\"}}}}\ntool-call workflow.execution_context {inspect}\ntool-call filesystem.read {read}\nloop-delivery {}",
        reference(1, "/output/value")
    ));
}

#[tokio::test]
async fn repository_handoff_resolves_authenticated_history() {
    snapshot_delivery::exercise(snapshot_delivery::Case::Repository(Case::Complete)).await;
}

#[tokio::test]
async fn repository_handoff_rejects_unauthenticated_or_incomplete_evidence() {
    for case in [
        Case::MissingChecksum,
        Case::WrongChecksum,
        Case::IncompleteItems,
        Case::DuplicateItems,
        Case::MissingCheck,
        Case::WrongArgv,
        Case::WrongTarget,
    ] {
        snapshot_delivery::exercise(snapshot_delivery::Case::Repository(case)).await;
    }
}
