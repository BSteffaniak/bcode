//! Bounded identity discovery across canonical representations in one read snapshot.
use super::{Database, DatabaseValue, Error, RUNS};
use std::collections::BTreeMap;
use switchy_database::query::{FilterableQuery, SortDirection};

/// Whether one representation family can be interpreted in this observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FamilyState {
    Available,
    Absent,
    Unsupported,
    Degraded,
}
/// Canonical membership, not authority to execute or mutate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogRepresentation {
    Legacy,
    Version2,
    Conflict,
}
/// Exact identity discovered without decoding its execution payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogIdentity {
    pub run_id: String,
    pub representation: CatalogRepresentation,
}
/// One merged keyset page. Degraded families mean discovery is incomplete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogPage {
    pub entries: Vec<CatalogIdentity>,
    pub next_after: Option<String>,
    pub legacy: FamilyState,
    pub version2: FamilyState,
}

async fn identities(
    db: &dyn Database,
    table: &str,
    after: Option<&str>,
    limit: usize,
) -> Result<Vec<String>, Error> {
    let mut query = db
        .select(table)
        .columns(&["run_id"])
        .sort("run_id", SortDirection::Asc)
        .limit(limit);
    if let Some(after) = after {
        query = query.where_gt("run_id", after);
    }
    query
        .execute(db)
        .await?
        .into_iter()
        .map(|row| match row.get("run_id") {
            Some(DatabaseValue::String(id)) if crate::validate_id("run_id", &id).is_ok() => Ok(id),
            _ => Err(Error::MaintenanceRequired),
        })
        .collect()
}
async fn family(
    db: &dyn Database,
    table: &str,
    after: Option<&str>,
    limit: usize,
) -> (FamilyState, Vec<String>) {
    match db.table_exists(table).await {
        Ok(false) => (FamilyState::Absent, vec![]),
        Ok(true) => identities(db, table, after, limit).await.map_or_else(
            |_| (FamilyState::Degraded, vec![]),
            |ids| (FamilyState::Available, ids),
        ),
        Err(_) => (FamilyState::Degraded, vec![]),
    }
}
async fn extension_state(db: &dyn Database) -> FamilyState {
    match db.table_exists(super::super::CONTRACT).await {
        Ok(true) => {}
        _ => return FamilyState::Degraded,
    }
    match super::supported(db).await {
        Ok(false) => FamilyState::Unsupported,
        Ok(true) => match super::super::verified(db).await {
            Ok(()) => FamilyState::Available,
            Err(_) => FamilyState::Degraded,
        },
        Err(_) => FamilyState::Degraded,
    }
}
/// Merge bounded canonical identity pages ordered by run ID, without repair or installation.
///
/// Unknown execution formats are identified, never decoded as legacy. A conflict entry
/// confers no canonical authority. Degraded families explicitly prevent completeness claims.
/// Cursors are per-request keysets, not durable snapshots; rescan for later arrivals behind them.
/// # Errors
/// Rejects invalid bounds/cursors and failure to open or commit the shared read snapshot.
pub async fn page(
    db: &dyn Database,
    after: Option<&str>,
    limit: u16,
) -> Result<CatalogPage, Error> {
    if !(1..=100).contains(&limit) {
        return Err(Error::InvalidAdmission);
    }
    if let Some(after) = after {
        crate::validate_id("catalog cursor", after).map_err(|_| Error::InvalidAdmission)?;
    }
    let tx = db.begin_transaction().await?;
    let count = usize::from(limit);
    let (legacy, old) = family(&*tx, "workflow_runs", after, count + 1).await;
    let (mut version2, new) = family(&*tx, RUNS, after, count + 1).await;
    if version2 == FamilyState::Available {
        version2 = extension_state(&*tx).await;
    } else if version2 == FamilyState::Absent
        && tx
            .table_exists(super::super::CONTRACT)
            .await
            .unwrap_or(true)
    {
        version2 = FamilyState::Degraded;
    }
    let mut merged = BTreeMap::new();
    for id in old {
        merged.insert(id, CatalogRepresentation::Legacy);
    }
    for id in new {
        merged
            .entry(id)
            .and_modify(|value| *value = CatalogRepresentation::Conflict)
            .or_insert(CatalogRepresentation::Version2);
    }
    let more = merged.len() > count;
    let entries = merged
        .into_iter()
        .take(count)
        .map(|(run_id, representation)| CatalogIdentity {
            run_id,
            representation,
        })
        .collect::<Vec<_>>();
    let next_after = more
        .then(|| {
            entries
                .last()
                .map(|entry| entry.run_id.clone())
                .ok_or(Error::MaintenanceRequired)
        })
        .transpose()?;
    tx.commit().await?;
    Ok(CatalogPage {
        entries,
        next_after,
        legacy,
        version2,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn alternating_families_page_without_loss_and_unknown_version_stays_local() {
        let db = super::super::super::tests::database().await;
        super::super::super::install(&*db).await.unwrap();
        let definition = bcode_workflow::WorkflowBuilder::new(
            "catalog",
            bcode_workflow::Step::task("work", |v: u32, _| async move { Ok(v) }),
        )
        .build()
        .unwrap()
        .definition()
        .clone();
        let identity =
            bcode_workflow::WorkflowDefinitionIdentity::for_definition("catalog", &definition)
                .unwrap();
        for i in 0..205 {
            let id = format!("run-{i:04}");
            if i % 2 == 0 {
                db.insert("workflow_runs")
                    .value("run_id", id)
                    .value("single_active", false)
                    .value("status", "running")
                    .value("concurrency_cap", 7_i64)
                    .execute(&*db)
                    .await
                    .unwrap();
            } else {
                let mut request = super::super::super::tests::run(&id, false);
                request.definition_id.clone_from(&identity.definition_id);
                request.definition_version = identity.definition_version;
                super::super::super::admit(&*db, &request, &definition)
                    .await
                    .unwrap();
            }
        }
        let mut after = None;
        let mut seen = Vec::new();
        loop {
            let page = page(&*db, after.as_deref(), 31).await.unwrap();
            assert_eq!(page.legacy, FamilyState::Available);
            assert_eq!(page.version2, FamilyState::Available);
            for entry in page.entries {
                seen.push(entry);
            }
            after = page.next_after;
            if after.is_none() {
                break;
            }
        }
        assert_eq!(seen.len(), 205);
        for (i, entry) in seen.iter().enumerate() {
            assert_eq!(entry.run_id, format!("run-{i:04}"));
            assert_eq!(
                entry.representation,
                if i % 2 == 0 {
                    CatalogRepresentation::Legacy
                } else {
                    CatalogRepresentation::Version2
                }
            );
        }
        db.update(super::super::super::CONTRACT)
            .value("version", i64::MAX)
            .where_eq("representation", 2_i64)
            .execute(&*db)
            .await
            .unwrap();
        let unknown = page(&*db, None, 100).await.unwrap();
        assert_eq!(unknown.legacy, FamilyState::Available);
        assert_eq!(unknown.version2, FamilyState::Unsupported);
        assert_eq!(unknown.entries.len(), 100);
        assert_eq!(
            unknown.entries[1].representation,
            CatalogRepresentation::Version2
        );
    }
    #[tokio::test]
    async fn conflict_on_page_boundary_is_never_selected_as_authoritative() {
        let db = super::super::super::tests::database().await;
        // A damaged identity-only new table: discovery must expose conflicts without installation.
        switchy_database::schema::create_table(RUNS)
            .column(super::super::super::column(
                "run_id",
                switchy_database::schema::DataType::Text,
                false,
            ))
            .primary_key("run_id")
            .execute(&*db)
            .await
            .unwrap();
        for id in ["a", "b", "c"] {
            db.insert("workflow_runs")
                .value("run_id", id)
                .value("single_active", false)
                .value("status", "running")
                .value("concurrency_cap", 7_i64)
                .execute(&*db)
                .await
                .unwrap();
        }
        db.insert(RUNS)
            .value("run_id", "b")
            .execute(&*db)
            .await
            .unwrap();
        let first = page(&*db, None, 1).await.unwrap();
        assert_eq!(first.next_after.as_deref(), Some("a"));
        let boundary = page(&*db, first.next_after.as_deref(), 1).await.unwrap();
        assert_eq!(
            boundary.entries,
            vec![CatalogIdentity {
                run_id: "b".into(),
                representation: CatalogRepresentation::Conflict
            }]
        );
        assert_eq!(boundary.version2, FamilyState::Degraded);
        assert_eq!(boundary.next_after.as_deref(), Some("b"));
        let last = page(&*db, boundary.next_after.as_deref(), 1).await.unwrap();
        assert_eq!(last.entries[0].run_id, "c");
        assert_eq!(last.next_after, None);
        assert_eq!(last.version2, FamilyState::Degraded);
    }

    #[tokio::test]
    async fn absent_extension_is_legacy_and_partial_extension_is_degraded() {
        let db = super::super::super::tests::database().await;
        let empty = page(&*db, None, 1).await.unwrap();
        assert_eq!(empty.version2, FamilyState::Absent);
        switchy_database::schema::create_table(super::super::super::CONTRACT)
            .column(super::super::super::column(
                "version",
                switchy_database::schema::DataType::BigInt,
                false,
            ))
            .execute(&*db)
            .await
            .unwrap();
        let partial = page(&*db, None, 1).await.unwrap();
        assert_eq!(partial.legacy, FamilyState::Available);
        assert_eq!(partial.version2, FamilyState::Degraded);
    }
}
