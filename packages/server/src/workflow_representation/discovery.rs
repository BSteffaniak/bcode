//! Mixed-representation bounded discovery and explicit history availability.
use super::{
    RunLookup, ServerError, ServerState, lookup_at, retained_store, unavailable, unsupported,
};

pub async fn check_repair_identity(state: &ServerState, identity: &str) -> Result<(), ServerError> {
    let retained = retained_store(state)?;
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
    let retained = retained_store(state)?;
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

pub async fn events(
    state: &ServerState,
    run: &str,
    after: Option<u64>,
    limit: usize,
) -> Result<Vec<bcode_workflow::WorkflowHistoryEvent>, ServerError> {
    let retained = retained_store(state)?;
    match lookup_at(retained.path(), run).await? {
        RunLookup::Legacy => {
            crate::workflow_operations::event_history(state, run, after, limit).map_err(Into::into)
        }
        RunLookup::Available(_) | RunLookup::UnsupportedRepresentation => Err(unsupported()),
        RunLookup::Missing | RunLookup::Degraded => Err(unavailable()),
    }
}
