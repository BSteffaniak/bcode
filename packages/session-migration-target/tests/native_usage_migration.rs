//! Writer-26 native harness usage catches up through the exclusive writer-27 migration.
//!
//! Writer 26 persisted native harness usage observations without folding them into the session
//! usage projection. The 26-to-27 edge preserves canonical payloads and rebuilds every current
//! projection, so the usage projection then includes native usage. No provider, native process,
//! raw SQL, or alternate projector is used; the writer-26 state is reconstructed from a current
//! store by removing exactly the projection rows writer 26 never wrote.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessActivity, HarnessAttention, HarnessAuthBinding,
    HarnessAuthCustody, HarnessCapabilities, HarnessCapability, HarnessCompleteness,
    HarnessConnectivity, HarnessContinuation, HarnessExecutionStatus, HarnessInstance,
    HarnessObservation, HarnessObservationKind, HarnessObservationPage, HarnessOpenRequest,
    HarnessOperation, HarnessPreparedSession, HarnessRequest, HarnessRequestKind,
    HarnessServiceResponse, HarnessServiceResponseKind, HarnessSupportLevel,
};
use bcode_session::{
    SessionManager,
    db::SessionDb,
    lease::{
        SessionLeaseOwnerContext, acquire_maintenance_session_write_lock,
        acquire_session_maintenance_guard,
    },
};
use bcode_session_migration::{build_target_receipt, normalize_canonical_row_for_writer_epoch};
use bcode_session_migration_target::{CURRENT_WRITER_EPOCH, MigrationPolicyCallbacks};
use bcode_session_models::{ClientId, ExternalHarnessOwner, SessionId, SessionUsageSummary};
use std::{path::Path, sync::Arc};
use switchy::database::{DatabaseValue, query::FilterableQuery};

const SOURCE_WRITER: u32 = 26;

fn manager(root: &Path) -> SessionManager {
    SessionManager::persistent_with_metrics_and_lease_owner(
        root,
        bcode_metrics::MetricsRegistry::disabled(),
        SessionLeaseOwnerContext {
            build_fingerprint: Some("artifact".into()),
            daemon_instance_id: Some("daemon".into()),
            ..SessionLeaseOwnerContext::default()
        },
    )
    .unwrap()
}

fn instance() -> HarnessInstance {
    HarnessInstance {
        instance_id: "fixture".into(),
        plugin_id: "fixture".into(),
        executable: "/not-invoked".into(),
        config_namespace: "fixture".into(),
        auth: HarnessAuthBinding {
            provider_id: "fixture".into(),
            profile: "native".into(),
            credential_namespace: "fixture".into(),
            custody: HarnessAuthCustody::NativeManaged,
        },
    }
}

fn capabilities() -> HarnessCapabilities {
    let supported = HarnessCapability {
        support: HarnessSupportLevel::Supported,
        constraints: Vec::new(),
    };
    HarnessCapabilities {
        model_switch: HarnessCapability::unsupported(),
        permission_mode: HarnessCapability::unsupported(),
        resume: supported.clone(),
        steering: supported.clone(),
        approval: supported.clone(),
        questions: supported.clone(),
        assistance: supported.clone(),
        interrupt: supported.clone(),
        authoritative_settlement: supported.clone(),
        background_continuation: supported.clone(),
        authentication: supported,
    }
}

fn usage(session_id: SessionId, id: &str, total_cost_micros: Option<u64>) -> HarnessObservation {
    HarnessObservation {
        observation_id: id.into(),
        session_id: session_id.to_string(),
        generation: 1,
        native_correlations: vec!["turn".into()],
        completeness: HarnessCompleteness::Complete,
        kind: HarnessObservationKind::Usage {
            input_tokens: 40,
            output_tokens: 10,
            cache_read_input_tokens: 50,
            cache_creation_input_tokens: 0,
            total_cost_micros,
            duration_ms: None,
            model: None,
        },
    }
}

/// Commit a managed session whose page carries one priced and one unpriced native result.
async fn managed_session_with_native_usage(root: &Path) -> SessionId {
    let sessions = manager(root);
    let session = sessions.create_session(None, root.into()).await.unwrap();
    let owner = ExternalHarnessOwner {
        session_id: session.id,
        generation: 1,
        artifact_id: "artifact".into(),
        daemon_id: "daemon".into(),
    };
    sessions
        .configure_external_harness(&owner, instance(), capabilities())
        .await
        .unwrap();
    let request = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "open-request".into(),
        instance: instance(),
        kind: HarnessRequestKind::Open(HarnessOpenRequest {
            open_id: "open".into(),
            session_id: session.id.to_string(),
            generation: 1,
            working_directory: "/tmp".into(),
            resume: None,
            parent_assistance: None,
        }),
    };
    sessions
        .claim_external_harness_request(&owner, &request)
        .await
        .unwrap();
    let prepared = HarnessPreparedSession {
        open_id: "open".into(),
        session_id: session.id.to_string(),
        instance_id: "fixture".into(),
        generation: 1,
        native_version: "fixture-v1".into(),
        requested_native_session_id: None,
    };
    sessions
        .complete_external_harness_request(
            &owner,
            &request,
            &HarnessServiceResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: request.request_id.clone(),
                operation: HarnessOperation::Open,
                kind: HarnessServiceResponseKind::Prepared(prepared),
            },
        )
        .await
        .unwrap();
    let page = HarnessObservationPage {
        live: Vec::new(),
        retention: None,
        hydration_requests: Vec::new(),
        inventory_page: None,
        observations: vec![
            usage(session.id, "result-priced", Some(900)),
            usage(session.id, "result-subscription", None),
        ],
        next_after: Some("native-cursor".into()),
        completeness: HarnessCompleteness::Complete,
        status: HarnessExecutionStatus {
            connectivity: HarnessConnectivity::Connected,
            activity: HarnessActivity::Running,
            attention: HarnessAttention::None,
            continuation: HarnessContinuation::Available,
            completeness: HarnessCompleteness::Complete,
        },
    };
    sessions
        .commit_external_harness_page(&owner, None, &page)
        .await
        .unwrap();
    assert!(
        sessions
            .release_idle_session_resources(session.id)
            .await
            .unwrap()
    );
    drop(sessions);
    session.id
}

/// Reconstruct writer-26 derived state: canonical native usage without usage projection rows.
async fn reconstruct_writer_26(root: &Path, session: SessionId) {
    let db = SessionDb::open_turso_in_root(session, root).await.unwrap();
    for key in ["external:1:result-priced", "external:1:result-subscription"] {
        db.database()
            .delete("session_usage_requests")
            .where_eq("request_key", key)
            .execute(db.database())
            .await
            .unwrap();
    }
    db.database()
        .update("session_usage_projection")
        .value(
            "summary_json",
            serde_json::to_string(&SessionUsageSummary::default()).unwrap(),
        )
        .where_eq("projection_id", 1)
        .execute(db.database())
        .await
        .unwrap();
    db.database()
        .update("session_storage_contract")
        .value(
            "writer_epoch",
            DatabaseValue::Int64(i64::from(SOURCE_WRITER)),
        )
        .where_eq("contract_id", 1)
        .execute(db.database())
        .await
        .unwrap();
    let stale = db.session_usage_summary().await.unwrap();
    assert_eq!(stale.observed_usage_count, 0);
}

async fn migrate(root: &Path, session: SessionId) -> SessionDb {
    let maintenance = acquire_session_maintenance_guard(root, session).unwrap();
    let write = acquire_maintenance_session_write_lock(&maintenance, root, session).unwrap();
    SessionDb::migrate_turso_in_root_observed_with_operation(
        session,
        root,
        &maintenance,
        &write,
        bcode_metrics::MetricsRegistry::disabled(),
        None,
        None,
        MigrationPolicyCallbacks {
            normalize: Arc::new(|row| {
                normalize_canonical_row_for_writer_epoch(row, SOURCE_WRITER)
                    .map_err(|error| error.to_string())
            }),
            build_receipt: Arc::new(build_target_receipt),
        },
    )
    .await
    .unwrap()
}

fn assert_native_totals(summary: &SessionUsageSummary) {
    assert_eq!(summary.observed_usage_count, 2);
    assert_eq!(summary.cumulative_metered_tokens, 2 * (40 + 50 + 10));
    assert_eq!(summary.totals_micros.get("USD"), Some(&900));
    assert_eq!(summary.estimated_usage_count, 1);
    assert_eq!(summary.unavailable_usage_count, 0);
}

#[tokio::test]
async fn writer_26_native_usage_joins_the_usage_projection_through_migration() {
    let root = tempfile::tempdir().unwrap();
    let session = managed_session_with_native_usage(root.path()).await;
    reconstruct_writer_26(root.path(), session).await;

    let db = migrate(root.path(), session).await;
    assert_eq!(
        db.storage_writer_epoch().await.unwrap(),
        u64::from(CURRENT_WRITER_EPOCH)
    );
    assert_native_totals(&db.session_usage_summary().await.unwrap());
    drop(db);

    // A fresh session manager serves the caught-up totals without loading history.
    let reopened = manager(root.path());
    let attachment = reopened
        .attach_session_recent(session, ClientId::new(), 1)
        .await
        .unwrap();
    assert_native_totals(&attachment.usage_summary);
}
