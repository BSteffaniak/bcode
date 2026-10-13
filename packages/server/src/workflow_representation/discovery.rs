//! Mixed-representation bounded discovery and explicit history availability.
use super::{
    RunLookup, ServerError, ServerState, lookup_at, retained_store, unavailable, unsupported,
};

pub async fn check_repair_identity(state: &ServerState, identity: &str) -> Result<(), ServerError> {
    let retained = retained_store(state).await?;
    let db = super::open(retained.path(), false)?;
    if super::representation::supports_admission(&db)
        .await
        .map_err(|_| unavailable())?
        && super::representation::inspection_attempts::run_for_dispatch(&db, identity)
            .await
            .map_err(|_| unavailable())?
            .is_some()
    {
        return Err(ServerError::WorkflowCapabilityUnavailable(
            "explicit attempt repair is not supported for this workflow representation".into(),
        ));
    }
    Ok(())
}

pub async fn list(
    state: &ServerState,
    limit: usize,
) -> Result<Vec<bcode_workflow::WorkflowRunSummary>, ServerError> {
    let limit = u16::try_from(limit)
        .ok()
        .filter(|limit| (1..=100).contains(limit))
        .ok_or_else(unavailable)?;
    let page = super::catalog::page(
        state,
        &bcode_workflow_view_models::WorkflowIdentityCatalogRequest {
            version: bcode_workflow_view_models::WORKFLOW_IDENTITY_CATALOG_VERSION,
            after: None,
            limit,
        },
    )
    .await?;
    let mut results = Vec::new();
    for identity in page.entries {
        results.push(
            super::status(state, &identity.run_id)
                .await?
                .ok_or_else(unavailable)?,
        );
    }
    Ok(results)
}

pub async fn attempts(
    state: &ServerState,
    run: &str,
    cursor: Option<&bcode_workflow::AttemptCursor>,
    limit: usize,
) -> Result<Vec<bcode_workflow::AttemptSummary>, ServerError> {
    let retained = retained_store(state).await?;
    match lookup_at(retained.path(), run).await? {
        RunLookup::Legacy => crate::workflow_operations::attempt_history(state, run, cursor, limit)
            .map_err(Into::into),
        RunLookup::Available(_) => {
            let limit = u16::try_from(limit)
                .ok()
                .filter(|limit| (1..=100).contains(limit))
                .ok_or_else(unavailable)?;
            let db = super::open(retained.path(), false)?;
            if let Some(cursor) = cursor {
                let prepared = super::representation::attempts::owner_boundary::prepared(
                    &db,
                    run,
                    &cursor.dispatch_identity,
                )
                .await
                .map_err(|_| unavailable())?
                .ok_or_else(unavailable)?;
                if prepared.prepared_at_ms != cursor.prepared_at_ms {
                    return Err(unavailable());
                }
            }
            super::representation::inspection_attempts::page(
                &db,
                run,
                cursor.map(|cursor| cursor.dispatch_identity.as_str()),
                limit,
            )
            .await
            .map_err(|_| unavailable())
        }
        RunLookup::UnsupportedRepresentation => Err(unsupported()),
        RunLookup::Missing | RunLookup::Degraded => Err(unavailable()),
    }
}

use super::representation::journal::{CoverageOrigin, ScalarFact};
use bcode_workflow::{
    WorkflowObservationFact, WorkflowObservationOrigin, WorkflowObservationPage,
    WorkflowObservationSource, WorkflowTransitionObservation,
};

pub async fn observations(
    state: &ServerState,
    run: &str,
    after: Option<u64>,
    through: Option<u64>,
    limit: usize,
) -> Result<bcode_workflow::WorkflowObservationPage, ServerError> {
    let retained = retained_store(state).await?;
    if !matches!(
        lookup_at(retained.path(), run).await?,
        RunLookup::Available(_)
    ) {
        return Err(unsupported());
    }
    let limit = u16::try_from(limit)
        .ok()
        .filter(|value| (1..=100).contains(value))
        .ok_or_else(unavailable)?;
    let db = super::open(retained.path(), false)?;
    let page = super::representation::journal::page(&db, run, after.unwrap_or(0), through, limit)
        .await
        .map_err(|_| unavailable())?;
    Ok(WorkflowObservationPage {
        version: page.version,
        run_id: page.run_id,
        origin: match page.origin {
            CoverageOrigin::Admission => WorkflowObservationOrigin::Admission,
            CoverageOrigin::NotEnabled => WorkflowObservationOrigin::NotEnabled,
        },
        sources: page
            .sources
            .into_iter()
            .map(|source| WorkflowObservationSource {
                table: source.table,
                columns: source.columns,
                operations: source.operations,
            })
            .collect(),
        limitations: page.limitations,
        through_sequence: page.through_sequence,
        next_after_sequence: page.next_after_sequence,
        observations: page
            .observations
            .into_iter()
            .map(|row| WorkflowTransitionObservation {
                sequence: row.sequence,
                source: row.source_table,
                operation: row.operation,
                facts: row
                    .facts
                    .into_iter()
                    .map(|(key, value)| {
                        (
                            key,
                            match value {
                                ScalarFact::Null => WorkflowObservationFact::Null,
                                ScalarFact::Integer(value) => {
                                    WorkflowObservationFact::Integer(value)
                                }
                                ScalarFact::Text(value) => WorkflowObservationFact::Text(value),
                                ScalarFact::Unavailable(reason) => WorkflowObservationFact::Unavailable(match reason {
                                    super::representation::journal::FactUnavailableReason::Oversized => bcode_workflow::WorkflowObservationUnavailableReason::Oversized,
                                    super::representation::journal::FactUnavailableReason::UnsupportedType => bcode_workflow::WorkflowObservationUnavailableReason::UnsupportedType,
                                }),
                            },
                        )
                    })
                    .collect(),
                recorded_at_ms: row.timestamp_ms,
                timestamp_source: row.timestamp_source,
            })
            .collect(),
    })
}

pub async fn lifecycle_history(
    state: &ServerState,
    query: &bcode_workflow::WorkflowLifecycleHistoryQuery,
) -> Result<bcode_workflow::WorkflowLifecycleHistoryPage, ServerError> {
    query.validate().map_err(|_| {
        ServerError::WorkflowCapabilityUnavailable("invalid lifecycle history query".into())
    })?;
    // This read must not start initialization, migration or maintenance.
    let retained = match state.workflow_store.try_lock() {
        Ok(store) => store.retained_handle()?,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner().retained_handle()?,
        Err(std::sync::TryLockError::WouldBlock) => return Err(unavailable()),
    };
    let page = match lookup_at(retained.path(), &query.run_id).await? {
        RunLookup::Legacy => unsupported_lifecycle_history(query)?,
        RunLookup::Available(_) => {
            let db = super::open(retained.path(), false)?;
            super::representation::semantic_history::lifecycle::page(
                &db,
                &query.run_id,
                query.cursor.as_ref(),
                u16::try_from(query.limit).map_err(|_| unavailable())?,
            )
            .await
            .map_err(|_| unavailable())?
        }
        RunLookup::UnsupportedRepresentation => return Err(unsupported()),
        RunLookup::Missing | RunLookup::Degraded => return Err(unavailable()),
    };
    query.validate_page(&page).map_err(|_| unavailable())?;
    Ok(page)
}

fn unsupported_lifecycle_history(
    query: &bcode_workflow::WorkflowLifecycleHistoryQuery,
) -> Result<bcode_workflow::WorkflowLifecycleHistoryPage, ServerError> {
    use bcode_workflow::{
        WorkflowLifecycleHistoryCursor, WorkflowLifecycleHistoryPage,
        WorkflowSemanticHistoryCoverage,
    };
    if query
        .cursor
        .as_ref()
        .is_some_and(|cursor| cursor.coverage != WorkflowSemanticHistoryCoverage::Unsupported)
    {
        return Err(unsupported());
    }
    let page = WorkflowLifecycleHistoryPage {
        cursor: query
            .cursor
            .clone()
            .unwrap_or_else(|| WorkflowLifecycleHistoryCursor {
                version: query.version,
                run_id: query.run_id.clone(),
                scope: query.scope,
                coverage: WorkflowSemanticHistoryCoverage::Unsupported,
                through_sequence: 0,
                after_sequence: 0,
            }),
        events: Vec::new(),
        next_cursor: None,
    };
    query.validate_page(&page).map_err(|_| unavailable())?;
    Ok(page)
}

/// Read one verified current inventory page without initialization or maintenance.
pub async fn inventory(
    state: &ServerState,
    query: &bcode_workflow::WorkflowInventoryQuery,
) -> Result<bcode_workflow::WorkflowInventoryPage, ServerError> {
    query.validate().map_err(|_| unavailable())?;
    let page = super::outputs::inventory_page(state, query).await?;
    query.validate_page(&page).map_err(|_| unavailable())?;
    Ok(page)
}

pub async fn semantic_history(
    state: &ServerState,
    query: &bcode_workflow::WorkflowSemanticHistoryQuery,
) -> Result<bcode_workflow::WorkflowSemanticHistoryPage, ServerError> {
    query.validate().map_err(|_| {
        ServerError::WorkflowCapabilityUnavailable("invalid semantic history query".into())
    })?;
    // This read must not start initialization, migration or maintenance.
    let retained = match state.workflow_store.try_lock() {
        Ok(store) => store.retained_handle()?,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner().retained_handle()?,
        Err(std::sync::TryLockError::WouldBlock) => return Err(unavailable()),
    };
    let page = match lookup_at(retained.path(), &query.run_id).await? {
        RunLookup::Legacy => unsupported_semantic_history(query)?,
        RunLookup::Available(_) => {
            let db = super::open(retained.path(), false)?;
            super::representation::semantic_history::page(
                &db,
                &query.run_id,
                query.cursor.as_ref(),
                u16::try_from(query.limit).map_err(|_| unavailable())?,
            )
            .await
            .map_err(|_| unavailable())?
        }
        RunLookup::UnsupportedRepresentation => return Err(unsupported()),
        RunLookup::Missing | RunLookup::Degraded => return Err(unavailable()),
    };
    query.validate_page(&page).map_err(|_| unavailable())?;
    Ok(page)
}

fn unsupported_semantic_history(
    query: &bcode_workflow::WorkflowSemanticHistoryQuery,
) -> Result<bcode_workflow::WorkflowSemanticHistoryPage, ServerError> {
    use bcode_workflow::{
        WorkflowSemanticHistoryCoverage, WorkflowSemanticHistoryCursor, WorkflowSemanticHistoryPage,
    };
    if query
        .cursor
        .as_ref()
        .is_some_and(|cursor| cursor.coverage != WorkflowSemanticHistoryCoverage::Unsupported)
    {
        return Err(unsupported());
    }
    let page = WorkflowSemanticHistoryPage {
        cursor: query
            .cursor
            .clone()
            .unwrap_or_else(|| WorkflowSemanticHistoryCursor {
                version: query.version,
                run_id: query.run_id.clone(),
                scope: query.scope,
                coverage: WorkflowSemanticHistoryCoverage::Unsupported,
                through_sequence: 0,
                after_sequence: 0,
            }),
        events: Vec::new(),
        next_cursor: None,
    };
    query.validate_page(&page).map_err(|_| unavailable())?;
    Ok(page)
}

/// Read typed bounded committed facts without initialization, replay or execution authority.
pub async fn committed_history(
    state: &ServerState,
    query: &bcode_workflow::WorkflowCommittedHistoryQuery,
) -> Result<bcode_workflow::WorkflowCommittedHistoryPage, ServerError> {
    use bcode_workflow::{
        WorkflowCommittedHistoryPage, WorkflowCommittedHistoryScope as Scope,
        WorkflowSemanticHistoryCoverage as Coverage,
    };
    query.validate().map_err(|_| unavailable())?;
    let retained = retained_store(state).await?;
    let scopes = vec![
        Scope::AttemptsV1,
        Scope::RunLifecycleV1,
        Scope::ControlSourcesV1,
    ];
    let page = match lookup_at(retained.path(), &query.run_id).await? {
        RunLookup::Available(_) => {
            let db = super::open(retained.path(), false)?;
            let limit = u16::try_from(query.limit).map_err(|_| unavailable())?;
            let page = super::representation::journal::public_events::page(
                &db,
                query.version,
                &query.run_id,
                query.after_sequence,
                query.through_sequence,
                limit,
            )
            .await
            .map_err(|_| unavailable())?;
            WorkflowCommittedHistoryPage {
                version: page.version,
                run_id: page.run_id,
                from_admission: page.from_admission,
                scopes,
                attempt_coverage: page.attempt_coverage,
                lifecycle_coverage: page.lifecycle_coverage,
                control_coverage: page.control_coverage,
                through_sequence: page.through_sequence,
                next_after_sequence: page.next_after_sequence,
                events: page.events,
            }
        }
        RunLookup::Legacy | RunLookup::UnsupportedRepresentation => WorkflowCommittedHistoryPage {
            version: query.version,
            run_id: query.run_id.clone(),
            from_admission: false,
            scopes,
            attempt_coverage: Coverage::Unsupported,
            lifecycle_coverage: Coverage::Unsupported,
            control_coverage: Coverage::Unsupported,
            through_sequence: 0,
            next_after_sequence: None,
            events: Vec::new(),
        },
        RunLookup::Missing | RunLookup::Degraded => return Err(unavailable()),
    };
    query.validate_page(&page).map_err(|_| unavailable())?;
    Ok(page)
}

pub async fn events(
    state: &ServerState,
    run: &str,
    after: Option<u64>,
    limit: usize,
) -> Result<Vec<bcode_workflow::WorkflowHistoryEvent>, ServerError> {
    let retained = retained_store(state).await?;
    match lookup_at(retained.path(), run).await? {
        RunLookup::Legacy => {
            crate::workflow_operations::event_history(state, run, after, limit).map_err(Into::into)
        }
        RunLookup::Available(_) | RunLookup::UnsupportedRepresentation => Err(unsupported()),
        RunLookup::Missing | RunLookup::Degraded => Err(unavailable()),
    }
}

#[cfg(test)]
mod semantic_history_tests {
    #[tokio::test]
    async fn committed_history_discovery_preserves_real_admission_and_wire_pin() {
        let root = tempfile::tempdir().unwrap();
        let store =
            bcode_workflow_store::WorkflowStore::create_coexistence_baseline(root.path()).unwrap();
        let path = store.path().to_path_buf();
        let mut state = crate::tests::test_server_state(crate::SessionManager::default());
        state.workflow_store = std::sync::Arc::new(std::sync::Mutex::new(store));
        let db = super::super::open(&path, true).unwrap();
        let definition = bcode_workflow::WorkflowBuilder::new(
            "history",
            bcode_workflow::Step::task("worker", |v: u32, _| async move { Ok(v) }),
        )
        .build()
        .unwrap()
        .definition()
        .clone();
        let identity =
            bcode_workflow::WorkflowDefinitionIdentity::for_definition("history", &definition)
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
        super::super::representation::install(&db).await.unwrap();
        super::super::representation::journal::install(&db)
            .await
            .unwrap();
        let run = bcode_workflow_store::NewWorkflowRun {
            run_id: "committed".into(),
            definition_id: identity.definition_id,
            definition_version: identity.definition_version,
            workspace_snapshot: "fixture".into(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!(1)),
            execution_authority: None,
            created_at_ms: 1,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "fixture".into(),
                profile_id: "build".into(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::ReadOnly,
            limits: bcode_workflow::WorkflowRunLimits::default(),
        };
        super::super::representation::admit_registered_observed(&db, &run, &definition, true)
            .await
            .unwrap();
        let query = bcode_workflow::WorkflowCommittedHistoryQuery {
            version: 1,
            run_id: run.run_id,
            after_sequence: 0,
            through_sequence: None,
            limit: 1,
        };
        let page = super::committed_history(&state, &query).await.unwrap();
        drop(state);
        assert!(page.from_admission);
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].created_at_ms, run.created_at_ms);
        let response = bcode_ipc::ResponsePayload::WorkflowCommittedHistory { page: page.clone() };
        let decoded = serde_json::from_slice::<bcode_ipc::ResponsePayload>(
            &serde_json::to_vec(&response).unwrap(),
        )
        .unwrap();
        let bcode_ipc::ResponsePayload::WorkflowCommittedHistory { page: wire } = decoded else {
            panic!("committed page");
        };
        query.validate_page(&wire).unwrap();
        assert_eq!(wire, page);
    }
    #[test]
    fn legacy_semantic_boundary_never_promotes_opaque_history_or_cursor_coverage() {
        use bcode_workflow::*;
        let mut query = WorkflowSemanticHistoryQuery {
            version: WORKFLOW_SEMANTIC_HISTORY_VERSION,
            run_id: "run".into(),
            scope: WorkflowSemanticHistoryScope::AttemptsV1,
            cursor: None,
            limit: 1,
        };
        let page = super::unsupported_semantic_history(&query).unwrap();
        assert_eq!(
            page.cursor.coverage,
            WorkflowSemanticHistoryCoverage::Unsupported
        );
        assert!(page.events.is_empty());
        query.cursor = Some(page.cursor);
        query.cursor.as_mut().unwrap().coverage =
            WorkflowSemanticHistoryCoverage::CompleteFromAdmission;
        assert!(super::unsupported_semantic_history(&query).is_err());
    }
}
