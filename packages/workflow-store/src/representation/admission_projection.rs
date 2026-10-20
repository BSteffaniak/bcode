//! Allocation-bounded projection from the canonical immutable admission JSON.
//!
//! This is not persisted or cached policy: every call reads the canonical document.
use super::{DatabaseValue, RepresentationError as Error};
use switchy_database::query::FilterableQuery;

/// Decode the store-owned admission extension without changing the bare admission contract.
pub(super) fn admission(json: &str) -> Result<crate::RepresentedWorkflowAdmission, Error> {
    serde_json::from_str(json).map_err(|_| Error::MaintenanceRequired)
}

/// Recover package context from exact canonical root lineage, including older child admissions.
///
/// This performs a fixed number of exact reads, independent of recursive depth.
/// # Errors
/// Rejects malformed lineage, conflicting child bindings and unsupported execution versions.
pub async fn package_binding(
    db: &dyn super::Database,
    run_id: &str,
) -> Result<Option<crate::WorkflowRunPackageBinding>, Error> {
    let rows = db
        .select("workflow_lineage_v2")
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(db)
        .await?;
    let row = rows.first().ok_or(Error::MaintenanceRequired)?;
    let Some(DatabaseValue::String(root_id)) = row.get("root_run_id") else {
        return Err(Error::MaintenanceRequired);
    };
    let Some(DatabaseValue::Int64(depth)) = row.get("depth") else {
        return Err(Error::MaintenanceRequired);
    };
    match row.get("link_json") {
        Some(DatabaseValue::Null)
            if root_id == run_id
                && depth == 1
                && row.get("parent_run_id") == Some(DatabaseValue::Null) => {}
        Some(DatabaseValue::String(json)) if root_id != run_id && depth > 1 => {
            let link: crate::WorkflowRunLink =
                serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)?;
            if link.child_run_id != run_id
                || link.root_run_id != root_id
                || i64::from(link.depth) != depth
                || row.get("parent_node_id")
                    != Some(DatabaseValue::String(link.parent_node_id.clone()))
                || row.get("parent_activation_id")
                    != Some(DatabaseValue::String(link.parent_activation_id.clone()))
                || row.get("parent_attempt")
                    != Some(DatabaseValue::Int64(i64::from(link.parent_attempt)))
                || row.get("parent_run_id") != Some(DatabaseValue::String(link.parent_run_id))
            {
                return Err(Error::MaintenanceRequired);
            }
        }
        _ => return Err(Error::MaintenanceRequired),
    }
    let root_lineage = db
        .select("workflow_lineage_v2")
        .where_eq("run_id", root_id.as_str())
        .limit(1)
        .execute(db)
        .await?;
    let root_lineage = root_lineage.first().ok_or(Error::MaintenanceRequired)?;
    if root_lineage.get("root_run_id") != Some(DatabaseValue::String(root_id.clone()))
        || root_lineage.get("depth") != Some(DatabaseValue::Int64(1))
        || root_lineage.get("parent_run_id") != Some(DatabaseValue::Null)
        || root_lineage.get("link_json") != Some(DatabaseValue::Null)
    {
        return Err(Error::MaintenanceRequired);
    }
    let rows = db
        .select(super::RUNS)
        .columns(&["admission_json"])
        .where_eq("run_id", root_id.as_str())
        .limit(1)
        .execute(db)
        .await?;
    let Some(DatabaseValue::String(json)) = rows.first().and_then(|r| r.get("admission_json"))
    else {
        return Err(Error::MaintenanceRequired);
    };
    let root = admission(&json)?;
    if root.admission.run_id != root_id {
        return Err(Error::MaintenanceRequired);
    }
    let binding = root.package_binding;
    if binding.as_ref().is_some_and(|b| b.version != 1) {
        return Err(Error::UnsupportedLifecycle);
    }
    if root_id != run_id {
        let rows = db
            .select(super::RUNS)
            .columns(&["admission_json"])
            .where_eq("run_id", run_id)
            .limit(1)
            .execute(db)
            .await?;
        let Some(DatabaseValue::String(json)) = rows.first().and_then(|r| r.get("admission_json"))
        else {
            return Err(Error::MaintenanceRequired);
        };
        let child = admission(&json)?;
        if child.admission.run_id != run_id
            || child
                .package_binding
                .as_ref()
                .is_some_and(|b| Some(b) != binding.as_ref())
        {
            return Err(Error::MaintenanceRequired);
        }
    }
    Ok(binding)
}

pub(super) async fn publication(
    db: &dyn super::Database,
    binding: &crate::WorkflowRunPackageBinding,
) -> Result<bcode_workflow::WorkflowPackageLock, Error> {
    if binding.version != 1 {
        return Err(Error::UnsupportedLifecycle);
    }
    let rows = db
        .select("workflow_package_publications")
        .columns(&["lock_json"])
        .where_eq("package_id", binding.package_id.as_str())
        .where_eq("package_lock_digest_sha256", binding.lock_digest.as_str())
        .limit(1)
        .execute(db)
        .await?;
    let Some(DatabaseValue::String(json)) = rows.first().and_then(|r| r.get("lock_json")) else {
        return Err(Error::InvalidAdmission);
    };
    let lock: bcode_workflow::WorkflowPackageLock =
        serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)?;
    lock.validate().map_err(|_| Error::MaintenanceRequired)?;
    if lock.package_id != binding.package_id
        || lock
            .digest_sha256()
            .map_err(|_| Error::MaintenanceRequired)?
            != binding.lock_digest
    {
        return Err(Error::MaintenanceRequired);
    }
    Ok(lock)
}

pub(super) async fn verify_member(
    db: &dyn super::Database,
    binding: &crate::WorkflowRunPackageBinding,
    run: &crate::NewWorkflowRun,
    definition: &bcode_workflow::WorkflowDefinition,
) -> Result<(), Error> {
    let lock = publication(db, binding).await?;
    let member = lock
        .members
        .iter()
        .find(|m| {
            m.definition_identity.definition_id == run.definition_id
                && m.definition_identity.definition_version == run.definition_version
        })
        .ok_or(Error::InvalidAdmission)?;
    let identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        member.definition_identity.kind.clone(),
        definition,
    )
    .map_err(|_| Error::InvalidAdmission)?;
    if identity != member.definition_identity {
        return Err(Error::InvalidAdmission);
    }
    Ok(())
}
#[derive(serde::Deserialize)]
struct LimitsOnly {
    limits: bcode_workflow::WorkflowRunLimits,
    #[serde(default)]
    package_binding: Option<crate::WorkflowRunPackageBinding>,
}
/// Decode only owned policy fields; serde still parses/skips the entire JSON and rejects syntax errors.
pub(super) fn limits(
    row: &switchy_database::Row,
) -> Result<bcode_workflow::WorkflowRunLimits, Error> {
    let Some(DatabaseValue::String(json)) = row.get("admission_json") else {
        return Err(Error::MaintenanceRequired);
    };
    let projected =
        serde_json::from_str::<LimitsOnly>(&json).map_err(|_| Error::MaintenanceRequired)?;
    if projected
        .package_binding
        .as_ref()
        .is_some_and(|b| b.version != 1)
    {
        return Err(Error::UnsupportedLifecycle);
    }
    Ok(projected.limits)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_keeps_bare_readers_compatible_and_preserves_unknown_versions() {
        let run = super::super::tests::run("binding", false);
        let bare = serde_json::to_string(&run).unwrap();
        assert_eq!(admission(&bare).unwrap().package_binding, None);
        assert_eq!(
            serde_json::to_string(&crate::RepresentedWorkflowAdmission {
                admission: run.clone(),
                package_binding: None,
            })
            .unwrap(),
            bare
        );
        let extension = crate::RepresentedWorkflowAdmission {
            admission: run.clone(),
            package_binding: Some(crate::WorkflowRunPackageBinding {
                version: 999,
                package_id: "test/package".into(),
                lock_digest: "a".repeat(64),
            }),
        };
        let json = serde_json::to_string(&extension).unwrap();
        // The pre-extension concrete admission reader ignores the optional field. Its generic
        // cancellation/transfer contract remains valid; this is not package execution consent.
        assert_eq!(
            serde_json::from_str::<crate::NewWorkflowRun>(&json).unwrap(),
            run
        );
        assert_eq!(admission(&json).unwrap(), extension);
        assert_eq!(
            serde_json::to_string(&admission(&json).unwrap()).unwrap(),
            json
        );
    }

    #[tokio::test]
    async fn unknown_binding_blocks_scheduling_without_rewriting_generic_lifecycle() {
        let db = super::super::tests::database().await;
        super::super::install(&*db).await.unwrap();
        let definition = bcode_workflow::WorkflowBuilder::new(
            "unknown",
            bcode_workflow::Step::task("work", |v: u32, _| async move { Ok(v) }),
        )
        .build()
        .unwrap()
        .definition()
        .clone();
        let id = bcode_workflow::WorkflowDefinitionIdentity::for_definition("unknown", &definition)
            .unwrap();
        let mut run = super::super::tests::run("unknown", false);
        run.definition_id = id.definition_id;
        run.definition_version = id.definition_version;
        let owner = crate::WorkflowExecutionAuthority {
            target_artifact_id: "a".into(),
            daemon_instance_id: "d".into(),
            generation: 1,
            fencing_token: "f".into(),
        };
        run.parent_session_id = Some("parent-session".into());
        run.execution_authority = Some(owner.clone());
        let binding = crate::WorkflowRunPackageBinding {
            version: 999,
            package_id: "test/future".into(),
            lock_digest: "a".repeat(64),
        };
        // Fixture models a future writer, not an upgrade of an existing admission.
        let tx = db.begin_transaction().await.unwrap();
        super::super::admit_package_in(&*tx, &run, &definition, Some(&binding))
            .await
            .unwrap();
        super::super::hierarchy::register_root(&*tx, &run)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(matches!(
            package_binding(&*db, &run.run_id).await,
            Err(Error::UnsupportedLifecycle)
        ));
        assert!(
            super::super::scheduling::snapshot(&*db, &run.run_id, None, None, 10)
                .await
                .is_err()
        );
        super::super::safety::request_cancellation(&*db, &run.run_id, &owner, 2)
            .await
            .unwrap();
        let rows = db
            .select(super::super::RUNS)
            .columns(&["admission_json"])
            .where_eq("run_id", run.run_id.as_str())
            .limit(1)
            .execute(&*db)
            .await
            .unwrap();
        let Some(DatabaseValue::String(json)) = rows[0].get("admission_json") else {
            panic!("admission");
        };
        assert_eq!(admission(&json).unwrap().package_binding, Some(binding));
    }
    #[test]
    fn projection_preserves_limits_and_rejects_missing_or_corrupt_policy() {
        let mut run = super::super::tests::run("policy", false);
        run.input = Some(
            serde_json::json!({"large": (0..10_000).map(|n| serde_json::json!({"id":n,"text":"retained input data","values":[1,2,3]})).collect::<Vec<_>>() }),
        );
        let json = serde_json::to_string(&run).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..100 {
            let projected: LimitsOnly = serde_json::from_str(&json).unwrap();
            assert_eq!(projected.limits, run.limits);
        }
        eprintln!(
            "limits-only 100 x {} bytes: {:?}",
            json.len(),
            start.elapsed()
        );
        let start = std::time::Instant::now();
        for _ in 0..100 {
            let full: crate::NewWorkflowRun = serde_json::from_str(&json).unwrap();
            assert_eq!(full.limits, run.limits);
        }
        eprintln!("full admission 100: {:?}", start.elapsed());
        assert!(serde_json::from_str::<LimitsOnly>("{}").is_err());
        assert!(
            serde_json::from_str::<LimitsOnly>(r#"{"limits":{"concurrency_cap":"future"}}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<LimitsOnly>(&format!("{json} invalid")).is_err());
    }
}
