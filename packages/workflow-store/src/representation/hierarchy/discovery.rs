//! Bounded, authority-neutral child lineage discovery.
use super::{Database, DatabaseValue, Error, FilterableQuery, LINEAGE};
/// Exact parent admission and lineage observed under pinned durable ownership.
#[derive(Debug, Clone)]
pub struct ParentContext {
    /// Immutable original admission; never rewritten with override policy.
    pub admission: crate::NewWorkflowRun,
    /// Current effective local limits, including authorized monotonic extensions.
    pub effective_limits: bcode_workflow::WorkflowRunLimits,
    /// Exact link for a child; None only for a verified root.
    pub parent_link: Option<crate::WorkflowRunLink>,
    /// Canonical root run identity.
    pub root_run_id: String,
    /// Canonical lineage depth.
    pub depth: u64,
    /// Current graph revision in the same snapshot.
    pub revision: u64,
}
/// Read exact parent context without inventing lineage or transferring authority.
/// # Errors
/// Rejects stale authority, missing/inconsistent lineage, unknown contracts or database errors.
pub async fn parent_context(
    db: &dyn Database,
    owner: &crate::WorkflowExecutionAuthority,
    run: &str,
) -> Result<ParentContext, Error> {
    let tx = db.begin_transaction().await?;
    super::super::verified(&*tx).await?;
    let root = super::exact(&*tx, super::RUNS, "run_id", run).await?;
    if super::text(&root, "authority_json")?
        != serde_json::to_string(owner).map_err(|_| Error::InvalidAdmission)?
    {
        return Err(Error::AuthorityConflict);
    }
    let admission: crate::NewWorkflowRun =
        serde_json::from_str(&super::text(&root, "admission_json")?)
            .map_err(|_| Error::MaintenanceRequired)?;
    if admission.run_id != run {
        return Err(Error::MaintenanceRequired);
    }
    let lineage = super::exact(&*tx, LINEAGE, "run_id", run).await?;
    let root_run_id = super::text(&lineage, "root_run_id")?;
    let depth = u64::try_from(super::integer(&lineage, "depth")?)
        .map_err(|_| Error::MaintenanceRequired)?;
    let parent_link = match lineage.get("link_json") {
        Some(DatabaseValue::Null)
            if depth == 1
                && root_run_id == run
                && lineage.get("parent_run_id") == Some(DatabaseValue::Null) =>
        {
            None
        }
        Some(DatabaseValue::String(json)) => {
            let link: crate::WorkflowRunLink =
                serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)?;
            if link.child_run_id != run
                || link.root_run_id != root_run_id
                || depth <= 1
                || lineage.get("parent_run_id")
                    != Some(DatabaseValue::String(link.parent_run_id.clone()))
            {
                return Err(Error::MaintenanceRequired);
            }
            Some(link)
        }
        _ => return Err(Error::MaintenanceRequired),
    };
    let effective_limits =
        super::super::allowance::effective_limits(&*tx, run, &admission.limits).await?;
    let revision = super::super::graph_pages::revision(&*tx, run).await?;
    tx.commit().await?;
    Ok(ParentContext {
        admission,
        effective_limits,
        parent_link,
        root_run_id,
        depth,
        revision,
    })
}

/// One bounded page of exact committed child links.
#[derive(Debug, Clone)]
pub struct ChildPage {
    /// Verified direct-child links, ordered by child run ID.
    pub children: Vec<crate::WorkflowRunLink>,
    /// Exclusive continuation cursor if another direct child exists.
    pub next_child_run_id: Option<String>,
}
/// Observe direct children without acquiring execution authority or recursively walking lineage.
/// # Errors
/// Rejects invalid bounds, malformed lineage, unsupported storage and database errors.
pub async fn children(
    db: &dyn Database,
    parent: &str,
    after: Option<&str>,
    limit: u16,
) -> Result<ChildPage, Error> {
    if !(1..=100).contains(&limit) {
        return Err(Error::InvalidAdmission);
    }
    let tx = db.begin_transaction().await?;
    super::super::verified(&*tx).await?;
    let mut query = tx
        .select(LINEAGE)
        .columns(&["run_id", "link_json"])
        .where_eq("parent_run_id", parent)
        .sort("run_id", switchy_database::query::SortDirection::Asc)
        .limit(usize::from(limit) + 1);
    if let Some(after) = after {
        query = query.where_gt("run_id", after);
    }
    let mut rows = query.execute(&*tx).await?;
    let more = rows.len() > usize::from(limit);
    rows.truncate(usize::from(limit));
    let mut children = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(DatabaseValue::String(id)) = row.get("run_id") else {
            return Err(Error::MaintenanceRequired);
        };
        let Some(DatabaseValue::String(json)) = row.get("link_json") else {
            return Err(Error::MaintenanceRequired);
        };
        let link: crate::WorkflowRunLink =
            serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)?;
        if link.parent_run_id != parent || link.child_run_id != id {
            return Err(Error::MaintenanceRequired);
        }
        children.push(link);
    }
    let next_child_run_id = if more {
        children.last().map(|link| link.child_run_id.clone())
    } else {
        None
    };
    tx.commit().await?;
    Ok(ChildPage {
        children,
        next_child_run_id,
    })
}
