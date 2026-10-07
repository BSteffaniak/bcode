use super::{inspect, project};
use bcode_workflow::{WorkflowAdmissionCategory, WorkflowAdmissionPhase};
use bcode_workflow_store::representation::admission_observation::{
    self, Category, Observation, Phase,
};
use switchy_database::{Database, TransactionMode, query::FilterableQuery};

fn observation(number: u8) -> Observation {
    Observation {
        version: admission_observation::VERSION,
        run_id: "run".into(),
        node_id: "worker".into(),
        activation_id: "activation".into(),
        attempt: 1,
        dispatch_identity: format!("{number:064x}"),
        phase: Phase::OwnerDispatch,
        category: Category::OwnerError,
        observed_at_ms: 7,
    }
}

async fn database() -> Box<dyn Database> {
    let db = switchy_database_connection::init_sqlite_rusqlite(None).unwrap();
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    admission_observation::install_in(&*tx).await.unwrap();
    tx.commit().await.unwrap();
    db
}

async fn seed(db: &dyn Database, number: u8) {
    let value = observation(number);
    db.insert("workflow_admission_observations_v1")
        .value("identity", admission_observation::cursor(&value))
        .value("run_id", value.run_id.as_str())
        .value("observation_json", serde_json::to_string(&value).unwrap())
        .execute(db)
        .await
        .unwrap();
}

#[test]
fn portable_projection_keeps_only_normalized_facts() {
    let result = project(observation(1));
    assert_eq!(result.phase, WorkflowAdmissionPhase::OwnerDispatch);
    assert_eq!(result.category, WorkflowAdmissionCategory::OwnerError);
    assert_eq!(result.observed_at_ms, 7);
    let encoded = serde_json::to_value(result).unwrap();
    assert!(encoded.get("event_seq").is_none());
    assert!(encoded.get("owner_error").is_none());
}

#[tokio::test]
async fn exact_limit_lookahead_distinguishes_complete_and_truncated() {
    let db = database().await;
    seed(&*db, 1).await;
    let (values, complete) = inspect(&*db, "run", 1).await;
    assert_eq!(values.len(), 1);
    assert_eq!(complete, Some(true));
    seed(&*db, 2).await;
    let (values, complete) = inspect(&*db, "run", 1).await;
    assert_eq!(values.len(), 1);
    assert_eq!(complete, Some(false));
}

#[tokio::test]
async fn absent_and_future_optional_state_are_unavailable_not_empty_complete() {
    let absent = switchy_database_connection::init_sqlite_rusqlite(None).unwrap();
    let (values, complete) = inspect(&*absent, "run", 10).await;
    assert!(values.is_empty());
    assert_eq!(complete, None);
    let db = database().await;
    db.update("workflow_admission_observation_contract")
        .value("version", 2_i64)
        .where_eq("contract_id", 1_i64)
        .execute(&*db)
        .await
        .unwrap();
    let (values, complete) = inspect(&*db, "run", 10).await;
    assert!(values.is_empty());
    assert_eq!(complete, None);
}

#[tokio::test]
async fn corrupt_lookahead_does_not_certify_a_valid_prefix() {
    let db = database().await;
    seed(&*db, 1).await;
    db.insert("workflow_admission_observations_v1")
        .value("identity", "ffffffff:owner")
        .value("run_id", "run")
        .value("observation_json", "unsupported payload")
        .execute(&*db)
        .await
        .unwrap();
    let (values, complete) = inspect(&*db, "run", 1).await;
    assert!(values.is_empty());
    assert_eq!(complete, None);
}
