//! Opt-in acceptance against the user-specified retained old executable.
use super::super::*;
use std::{path::Path, sync::Arc};
const OLD: &str = "/Users/braden/GitHub/bcode/target/aarch64-apple-darwin/release/bcode";
fn old(state: &Path, args: &[&str]) -> serde_json::Value {
    let output = std::process::Command::new(OLD)
        .args([
            "--state-root",
            state.to_str().unwrap(),
            "--no-credential-discovery",
            "--request-timeout-secs",
            "15",
        ])
        .args(args)
        .current_dir(state)
        .output()
        .expect("old executable");
    assert!(
        output.status.success(),
        "old command {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("old JSON")
}
fn old_fixture(state: &Path) {
    let source = state.join("gate.json");
    let input = state.join("input.json");
    std::fs::write(&input, "1").unwrap();
    std::fs::write(&source,serde_json::json!({"workflow_source_version":3,"workflow_id":"coexistence-gate","title":"Coexistence gate","run_limits":{"maximum_duration_ms":600_000,"node_execution_cap":10,"concurrency_cap":7,"cycle_cap":1,"retry_cap":1},"steps":[{"id":"wait","input":{"schema":{"type_name":"number","schema":{"type":"integer"}}}}]}).to_string()).unwrap();
    let session = old(
        state,
        &[
            "session",
            "create",
            "old-parent",
            "--cwd",
            state.to_str().unwrap(),
            "--json",
        ],
    );
    old(
        state,
        &[
            "workflow",
            "author",
            "create",
            "--draft-id",
            "draft",
            source.to_str().unwrap(),
        ],
    );
    old(
        state,
        &[
            "workflow",
            "author",
            "publish",
            "--workflow-id",
            "coexistence-gate",
            "--draft-id",
            "draft",
            "--expected-generation",
            "1",
        ],
    );
    old(
        state,
        &[
            "workflow",
            "start",
            "--parent-session-id",
            session["id"].as_str().unwrap(),
            "--run-id",
            "old-active",
            "--input",
            input.to_str().unwrap(),
            "revision",
            "--workflow-id",
            "coexistence-gate",
            "--revision",
            "1",
        ],
    );
}

async fn current_fixture(
    root: &Path,
) -> (
    tempfile::TempDir,
    Arc<ServerState>,
    bcode_session_models::SessionId,
) {
    let sessions_root = tempfile::tempdir().unwrap();
    let sessions = crate::SessionManager::persistent_lazy(sessions_root.path());
    let parent = sessions
        .create_session(Some("new-parent".into()), root.to_path_buf())
        .await
        .unwrap();
    let store = bcode_workflow_store::WorkflowStore::open_in_state_dir(root).unwrap();
    assert_eq!(
        store
            .run_limits("old-active")
            .unwrap()
            .unwrap()
            .concurrency_cap,
        Some(7)
    );
    let state = Arc::new({
        let mut state =
            crate::tests::test_server_state_with_workflow_authorization(sessions, store);
        state
            .selected_provider_context
            .settings
            .insert("fake_structured_output_json".into(), "9".into());
        state
    });
    (sessions_root, state, parent.id)
}

fn complete_old_gate(root: &Path) {
    let waits = old(root, &["workflow", "waits", "--run-id", "old-active"]);
    old(
        root,
        &[
            "workflow",
            "provide-input",
            "--run-id",
            "old-active",
            "--node-id",
            "wait",
            "--activation-id",
            waits[0]["activation_id"].as_str().unwrap(),
            "--value",
            "9",
        ],
    );
    assert_eq!(
        old(root, &["workflow", "run-status", "--run-id", "old-active"])["status"],
        "completed"
    );
}

#[tokio::test]
#[ignore = "starts retained user-specified old daemon in a fresh preserved sandbox"]
async fn old_daemon_and_current_provider_owner_progress_in_same_state() {
    let root = tempfile::Builder::new()
        .prefix("bcode-old-current-server-")
        .tempdir()
        .unwrap()
        .keep();
    println!("coexistence sandbox: {}", root.display());
    old_fixture(&root);
    let (_sessions_root, state, parent) = current_fixture(&root).await;
    let definition = super::parallel_provider_definition();
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("mixed-provider", &definition)
            .unwrap();
    state
        .workflow_store
        .lock()
        .unwrap()
        .persist_definition(
            &identity.definition_id,
            identity.definition_version,
            &definition,
        )
        .unwrap();
    let started = crate::workflow_operations::start_run(
        &state,
        bcode_workflow::WorkflowRunStartRequest {
            definition_id: identity.definition_id,
            definition_version: identity.definition_version,
            run_id: Some("new-provider".into()),
            workspace_snapshot: "fixture".into(),
            parent_session_id: parent,
            parent_session_generation: None,
            binding: None,
            input: Some(serde_json::json!(1)),
            limits: bcode_workflow::WorkflowRunLimits::default(),
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(started.run.run_id, "new-provider");
    assert_eq!(
        old(&root, &["workflow", "run-status", "--run-id", "old-active"])["status"],
        "running"
    );
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            crate::drive_workflow_run(&state, "new-provider")
                .await
                .unwrap();
            if status(&state, "new-provider")
                .await
                .unwrap()
                .unwrap()
                .status
                == bcode_workflow::RunStatus::Completed
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("new provider completion");
    complete_old_gate(&root);
    assert_eq!(
        state
            .workflow_store
            .lock()
            .unwrap()
            .run_limits("old-active")
            .unwrap()
            .unwrap()
            .concurrency_cap,
        Some(7)
    );
    assert!(
        state
            .workflow_store
            .lock()
            .unwrap()
            .run_summary("new-provider")
            .unwrap()
            .is_none()
    );
    // Drop current handles before checking that the old process independently retains ownership.
    let path = crate::workflow_store_path(&state);
    drop(state);
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.parent().unwrap().join("workflow.lock"))
        .unwrap();
    assert!(lock.try_lock().is_err());
}
