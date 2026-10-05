//! Genuine schema42 file tests. Typed writer statements are not an old executable.
use super::{Database, RUNS, admit, install};
use std::sync::{Arc, Barrier};
use switchy_database::{DatabaseValue, TransactionMode, query::FilterableQuery};

fn definition() -> bcode_workflow::WorkflowDefinition {
    bcode_workflow::WorkflowBuilder::new(
        "test",
        bcode_workflow::Step::task("work", |v: u32, _| async move { Ok(v) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone()
}
async fn legacy_insert(
    db: &dyn Database,
    id: &str,
    single: bool,
) -> Result<(), switchy_database::DatabaseError> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    tx.insert("workflow_runs")
        .value("run_id", id)
        .value("definition_id", "test")
        .value("definition_version", 1_i64)
        .value("workspace_snapshot", "workspace")
        .value("owner_plugin_id", "test")
        .value("workflow_kind", "test")
        .value("scope_key", "scope")
        .value("single_active", single)
        .value(
            "authorization_profile_json",
            serde_json::to_string(&super::tests::run(id, single).authorization_profile).unwrap(),
        )
        .value("status", "running")
        .value("node_execution_cap", 100_i64)
        .value("concurrency_cap", 7_i64)
        .value("cycle_cap", 10_i64)
        .value("retry_cap", 1_i64)
        .value("created_at_ms", 1_i64)
        .value("updated_at_ms", 1_i64)
        .execute(&*tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn race(same_id: bool) {
    let temp = tempfile::tempdir().unwrap();
    let mut legacy = crate::WorkflowStore::create_coexistence_baseline(temp.path()).unwrap();
    legacy.persist_definition("test", 1, &definition()).unwrap();
    let path = legacy.path().to_path_buf();
    let observer = switchy_database_connection::init_sqlite_rusqlite(Some(&path)).unwrap();
    install(&*observer).await.unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles = [false, true].map(|modern| {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let db = switchy_database_connection::init_sqlite_rusqlite(Some(&path)).unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            barrier.wait();
            runtime.block_on(async {
                if modern {
                    let mut run =
                        super::tests::run(if same_id { "collision" } else { "modern" }, !same_id);
                    let definition = definition();
                    let identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
                        "test",
                        &definition,
                    )
                    .unwrap();
                    run.definition_id = identity.definition_id;
                    run.definition_version = identity.definition_version;
                    admit(&*db, &run, &definition).await.is_ok()
                } else {
                    legacy_insert(&*db, if same_id { "collision" } else { "legacy" }, !same_id)
                        .await
                        .is_ok()
                }
            })
        })
    });
    assert_eq!(
        handles
            .into_iter()
            .map(|h| usize::from(h.join().unwrap()))
            .sum::<usize>(),
        1
    );
    let old = observer
        .select("workflow_runs")
        .columns(&["run_id", "concurrency_cap"])
        .execute(&*observer)
        .await
        .unwrap();
    let new = observer
        .select(RUNS)
        .columns(&["run_id"])
        .execute(&*observer)
        .await
        .unwrap();
    if old.is_empty() {
        assert!(
            legacy_insert(
                &*observer,
                if same_id { "collision" } else { "legacy" },
                !same_id
            )
            .await
            .is_err()
        );
    } else {
        let definition = definition();
        let identity =
            bcode_workflow::WorkflowDefinitionIdentity::for_definition("test", &definition)
                .unwrap();
        let mut retry = super::tests::run(if same_id { "collision" } else { "modern" }, !same_id);
        retry.definition_id = identity.definition_id;
        retry.definition_version = identity.definition_version;
        assert!(admit(&*observer, &retry, &definition).await.is_err());
    }
    assert_eq!(old.len() + new.len(), 1);
    if let Some(row) = old.first() {
        assert_eq!(row.get("concurrency_cap"), Some(DatabaseValue::Int64(7)));
    }
    verify_independent_legacy(&mut legacy);
    let contract = observer
        .select("workflow_storage_compatibility")
        .columns(&["compatibility"])
        .where_eq("contract_id", 1_i64)
        .execute(&*observer)
        .await
        .unwrap();
    assert_eq!(
        contract[0].get("compatibility"),
        Some(DatabaseValue::Int64(2))
    );
}
fn verify_independent_legacy(legacy: &mut crate::WorkflowStore) {
    let mut independent = super::tests::run("independent", false);
    independent.binding = None;
    independent.limits.concurrency_cap = Some(7);
    legacy.create_run(&independent).unwrap();
    assert_eq!(
        legacy
            .run_limits("independent")
            .unwrap()
            .unwrap()
            .concurrency_cap,
        Some(7)
    );
    assert!(
        legacy
            .list_runs(10)
            .unwrap()
            .iter()
            .any(|r| r.run_id == "independent")
    );
}

#[tokio::test]
async fn retained_legacy_handle_allows_install_and_same_id_race_has_one_winner() {
    race(true).await;
}
#[tokio::test]
async fn retained_legacy_handle_allows_install_and_binding_race_has_one_winner() {
    race(false).await;
}
