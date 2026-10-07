//! Mixed canonical identity discovery without pretending legacy rich queries cover new runs.
use super::{ServerError, ServerState, open, retained_store, unavailable};
use bcode_workflow_store::representation::runs::catalog as store;
use bcode_workflow_view_models::{
    WORKFLOW_IDENTITY_CATALOG_VERSION, WorkflowCatalogFamilyHealth as Health,
    WorkflowCatalogIdentity, WorkflowCatalogMembership as Membership, WorkflowIdentityCatalogPage,
    WorkflowIdentityCatalogRequest,
};

#[cfg(test)]
mod tests;

/// Read a bounded mixed-family identity page, ordered solely by run ID.
/// # Errors
/// Rejects unsupported protocol, invalid bounds/cursor, unavailable canonical store or query failure.
pub async fn page(
    state: &ServerState,
    request: &WorkflowIdentityCatalogRequest,
) -> Result<WorkflowIdentityCatalogPage, ServerError> {
    if request.version != WORKFLOW_IDENTITY_CATALOG_VERSION
        || !(1..=100).contains(&request.limit)
        || request
            .after
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 1024)
    {
        return Err(unavailable());
    }
    let retained = retained_store(state).await?;
    let db = open(retained.path(), false)?;
    let page = store::page(&db, request.after.as_deref(), request.limit)
        .await
        .map_err(|_| unavailable())?;
    Ok(project(page))
}

fn project(page: store::CatalogPage) -> WorkflowIdentityCatalogPage {
    WorkflowIdentityCatalogPage {
        version: WORKFLOW_IDENTITY_CATALOG_VERSION,
        entries: page
            .entries
            .into_iter()
            .map(|entry| WorkflowCatalogIdentity {
                run_id: entry.run_id,
                membership: match entry.representation {
                    store::CatalogRepresentation::Legacy => Membership::Legacy,
                    store::CatalogRepresentation::Version2 => Membership::Version2,
                    store::CatalogRepresentation::Conflict => Membership::Conflict,
                },
            })
            .collect(),
        next_after: page.next_after,
        legacy: health(page.legacy),
        version2: health(page.version2),
    }
}
const fn health(state: store::FamilyState) -> Health {
    match state {
        store::FamilyState::Available => Health::Available,
        store::FamilyState::Absent => Health::Absent,
        store::FamilyState::Unsupported => Health::Unsupported,
        store::FamilyState::Degraded => Health::Degraded,
    }
}

/// Return basic mixed rows through the existing catalog boundary, or None for legacy-only storage.
/// Rich query fields remain echoed for request correlation; `basic_discovery` explicitly marks them unapplied.
/// # Errors
/// Rejects invalid limits/cursors and unavailable storage without falling back to a partial legacy catalog.
pub async fn basic_view(
    state: &ServerState,
    request: &bcode_workflow_view_models::WorkflowCatalogRequest,
) -> Result<
    Option<bcode_workflow_view_models::WorkflowCatalogView>,
    bcode_workflow_store::WorkflowStoreError,
> {
    use bcode_workflow_view_models::{WorkflowBasicCatalogAvailability, WorkflowCatalogCursor};
    let invalid = || {
        bcode_workflow_store::WorkflowStoreError::InvalidData(
            "invalid mixed catalog request or unavailable storage".into(),
        )
    };
    let first = page(
        state,
        &WorkflowIdentityCatalogRequest {
            version: WORKFLOW_IDENTITY_CATALOG_VERSION,
            after: None,
            limit: 1,
        },
    )
    .await
    .map_err(|_| invalid())?;
    if first.version2 == Health::Absent {
        return Ok(None);
    }
    let limit = u16::try_from(request.limit)
        .ok()
        .filter(|limit| (1..=100).contains(limit))
        .ok_or_else(invalid)?;
    // Reserved cursor discriminator: never reinterpret an old timestamp cursor as an ID cursor.
    if request
        .cursor
        .as_ref()
        .is_some_and(|cursor| cursor.status_rank != u8::MAX || cursor.timestamp_ms != 0)
    {
        return Err(invalid());
    }
    let identities = page(
        state,
        &WorkflowIdentityCatalogRequest {
            version: WORKFLOW_IDENTITY_CATALOG_VERSION,
            after: request.cursor.as_ref().map(|cursor| cursor.run_id.clone()),
            limit,
        },
    )
    .await
    .map_err(|_| invalid())?;
    let mut runs = Vec::new();
    let mut unavailable_run_ids = Vec::new();
    for entry in &identities.entries {
        if entry.membership == Membership::Conflict {
            unavailable_run_ids.push(entry.run_id.clone());
            continue;
        }
        match super::status(state, &entry.run_id).await {
            Ok(Some(summary)) => runs.push(basic_row(summary)),
            _ => unavailable_run_ids.push(entry.run_id.clone()),
        }
    }
    let next_cursor = identities.next_after.map(|run_id| WorkflowCatalogCursor {
        sort: request.sort,
        timestamp_ms: 0,
        status_rank: u8::MAX,
        run_id,
    });
    let mut view = bcode_workflow_view::project_catalog(runs, request, next_cursor.is_some());
    view.next_cursor = next_cursor;
    view.basic_discovery = Some(WorkflowBasicCatalogAvailability {
        legacy: identities.legacy,
        version2: identities.version2,
        requested_query_applied: false,
        progress_available: false,
        unavailable_run_ids,
    });
    Ok(Some(view))
}

pub(super) fn basic_row(
    run: bcode_workflow::WorkflowRunSummary,
) -> bcode_workflow_view_models::WorkflowRunListItem {
    use bcode_workflow_view_models::{
        WorkflowDefinitionDisposition, WorkflowRunListItem, WorkflowRunStatus as Status,
    };
    let binding_label = run
        .binding
        .as_ref()
        .and_then(|binding| binding.display_label.clone());
    WorkflowRunListItem {
        display_title: binding_label.clone().unwrap_or_else(|| run.run_id.clone()),
        binding_label,
        run_id: run.run_id,
        definition_id: run.definition_id,
        definition_version: run.definition_version,
        authored_source: None,
        definition_disposition: WorkflowDefinitionDisposition::CompiledOnly,
        parent_run_id: None,
        descendant_count: 0,
        progress: bcode_workflow_view_models::WorkflowRunProgress::default(),
        attention: bcode_workflow_view_models::WorkflowAttentionSummary::default(),
        status: match run.status {
            bcode_workflow::RunStatus::Running => Status::Running,
            bcode_workflow::RunStatus::Paused => Status::Paused,
            bcode_workflow::RunStatus::Completed => Status::Completed,
            bcode_workflow::RunStatus::Failed => Status::Failed,
            bcode_workflow::RunStatus::Cancelled => Status::Cancelled,
            bcode_workflow::RunStatus::RepairRequired => Status::RepairRequired,
        },
        created_at_ms: run.created_at_ms,
        updated_at_ms: run.updated_at_ms,
    }
}
