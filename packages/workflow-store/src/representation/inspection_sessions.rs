//! Canonical paged execution-session provenance observation.
use super::{Database, DatabaseValue, RepresentationError as Error};
use switchy_database::query::{FilterableQuery, SortDirection};
/// Read bounded session links, rejecting malformed or conflicting identities.
/// # Errors
/// Rejects invalid bounds, unsupported durable state and inconsistent links.
pub async fn links(
    db: &dyn Database,
    run: &str,
    after: Option<&str>,
    limit: u16,
) -> Result<Vec<crate::WorkflowExecutionSessionLink>, Error> {
    if !(1..=100).contains(&limit) {
        return Err(Error::InvalidAdmission);
    }
    super::verified(db).await?;
    let mut query = db
        .select("workflow_execution_sessions_v2")
        .where_eq("run_id", run)
        .sort("dispatch_identity", SortDirection::Asc)
        .limit(usize::from(limit));
    if let Some(after) = after {
        query = query.where_gt("dispatch_identity", after);
    }
    query
        .execute(db)
        .await?
        .into_iter()
        .map(|row| {
            let Some(DatabaseValue::String(json)) = row.get("link_json") else {
                return Err(Error::MaintenanceRequired);
            };
            if json.len() > super::MAX_ADMISSION_BYTES {
                return Err(Error::MaintenanceRequired);
            }
            let link: crate::WorkflowExecutionSessionLink =
                serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)?;
            crate::validate_execution_session_link(&link)
                .map_err(|_| Error::MaintenanceRequired)?;
            if link.run_id != run
                || row.get("dispatch_identity")
                    != Some(DatabaseValue::String(crate::dispatch_identity(
                        run,
                        &link.node_id,
                        &link.activation_id,
                        link.attempt,
                    )))
            {
                return Err(Error::MaintenanceRequired);
            }
            Ok(link)
        })
        .collect()
}
