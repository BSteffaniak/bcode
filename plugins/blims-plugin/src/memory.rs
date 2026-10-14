//! Existing-state scoped memory retrieval. No ranker transfer or candidate emission.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;
use switchy_database::Database;
use switchy_database::query::{FilterableQuery as _, SortDirection, where_eq};

/// Independently versioned retrieval request; caller-supplied paths are never authorization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryRetrieveRequest {
    /// Must equal 1.
    pub version: u16,
    /// Must match the invocation-authorized canonical workspace.
    pub working_directory: PathBuf,
    /// Exact company identity in the selected existing database.
    pub company_id: String,
    /// Exact agent identity in that company database.
    pub agent_id: String,
    /// Whole records to return, 1 through 16.
    pub limit: u16,
}

/// Original persisted memory, never a synthesized history excerpt.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryHit {
    /// Persisted identity.
    pub id: String,
    /// Persisted agent identity.
    pub agent_id: String,
    /// Persisted category.
    pub kind: String,
    /// Whole persisted summary; never silently truncated.
    pub summary: String,
    /// Persisted importance.
    pub importance: i64,
}

/// Bounded serialized response, including metadata, at most 8192 bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryRetrieveResponse {
    /// Response schema version, currently 1.
    pub version: u16,
    /// Verified company identity.
    pub company_id: String,
    /// Verified agent identity.
    pub agent_id: String,
    /// Whole original records in deterministic importance/identity order.
    pub hits: Vec<MemoryHit>,
    /// False when count or byte limits omit observed records; not total database coverage.
    pub complete: bool,
}

/// Normalized failures contain no database paths, SQL, memory text or driver diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MemoryRetrieveError {
    /// Unknown version or invalid identifiers/count.
    #[error("invalid memory retrieval request")]
    InvalidRequest,
    /// Authorization or exact resource confinement cannot be verified.
    #[error("memory retrieval authorization unavailable")]
    AuthorizationUnavailable,
    /// Missing, incompatible or damaged domain state.
    #[error("memory state unavailable or maintenance required")]
    StateUnavailable,
    /// Requested company or agent does not exist in this verified state location.
    #[error("memory scope unavailable")]
    ScopeUnavailable,
}

fn validate(request: &MemoryRetrieveRequest) -> Result<(), MemoryRetrieveError> {
    let valid_id = |value: &str| {
        !value.trim().is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
    };
    if request.version != 1
        || !(1..=16).contains(&request.limit)
        || !valid_id(&request.company_id)
        || !valid_id(&request.agent_id)
    {
        return Err(MemoryRetrieveError::InvalidRequest);
    }
    Ok(())
}

/// Retrieve from an already-authorized, existing company database owned by the BLIMS caller.
///
/// The caller must verify the connection belongs to `working_directory` and obtain explicit
/// read authorization before opening it. This function never opens paths, initializes schema,
/// emits candidates or authorizes disclosure to a ranker. Tool arguments cannot supply a connection.
///
/// # Errors
/// Rejects unsupported requests/schema, missing company/agent scope and corrupt state with
/// content-free errors. The returned JSON including metadata is bounded to 8192 bytes.
pub async fn retrieve_in_database(
    request: &MemoryRetrieveRequest,
    database: &dyn Database,
) -> Result<MemoryRetrieveResponse, MemoryRetrieveError> {
    validate(request)?;
    let versions = database
        .select("blims_schema_version")
        .columns(&["version"])
        .limit(2)
        .execute(database)
        .await
        .map_err(|_| MemoryRetrieveError::StateUnavailable)?;
    if versions.len() != 1
        || super::required_i64(&versions[0], "version")
            .map_err(|_| MemoryRetrieveError::StateUnavailable)?
            != super::SCHEMA_VERSION
    {
        return Err(MemoryRetrieveError::StateUnavailable);
    }
    // Current BLIMS has exactly one company per database; agents/memories have no company column.
    let companies = database
        .select("companies")
        .columns(&["id"])
        .limit(2)
        .execute(database)
        .await
        .map_err(|_| MemoryRetrieveError::StateUnavailable)?;
    if companies.len() != 1
        || super::required_text(&companies[0], "id")
            .map_err(|_| MemoryRetrieveError::StateUnavailable)?
            != request.company_id
    {
        return Err(MemoryRetrieveError::ScopeUnavailable);
    }
    if database
        .select("agents")
        .columns(&["id"])
        .filter(Box::new(where_eq("id", request.agent_id.clone())))
        .limit(1)
        .execute_first(database)
        .await
        .map_err(|_| MemoryRetrieveError::StateUnavailable)?
        .is_none()
    {
        return Err(MemoryRetrieveError::ScopeUnavailable);
    }
    let rows = database
        .select("memories")
        .columns(&["id", "agent_id", "kind", "summary", "importance"])
        .filter(Box::new(where_eq("agent_id", request.agent_id.clone())))
        .sort("importance", SortDirection::Desc)
        .sort("id", SortDirection::Asc)
        .limit(17)
        .execute(database)
        .await
        .map_err(|_| MemoryRetrieveError::StateUnavailable)?;
    let mut response = MemoryRetrieveResponse {
        version: 1,
        company_id: request.company_id.clone(),
        agent_id: request.agent_id.clone(),
        hits: Vec::new(),
        complete: rows.len() <= usize::from(request.limit),
    };
    let mut ids = BTreeSet::new();
    for row in rows.iter().take(usize::from(request.limit)) {
        let record =
            super::agent_memory_record(row).map_err(|_| MemoryRetrieveError::StateUnavailable)?;
        if record.agent_id != request.agent_id
            || record.id.is_empty()
            || !ids.insert(record.id.clone())
        {
            return Err(MemoryRetrieveError::StateUnavailable);
        }
        response.hits.push(MemoryHit {
            id: record.id,
            agent_id: record.agent_id,
            kind: record.kind,
            summary: record.summary,
            importance: record.importance,
        });
        if serde_json::to_vec(&response)
            .map_err(|_| MemoryRetrieveError::StateUnavailable)?
            .len()
            > 8192
        {
            response.hits.pop();
            response.complete = false;
            break;
        }
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> MemoryRetrieveRequest {
        MemoryRetrieveRequest {
            version: 1,
            working_directory: PathBuf::new(),
            company_id: "default".into(),
            agent_id: "a".into(),
            limit: 16,
        }
    }

    #[test]
    fn memory_request_rejects_future_and_invalid_bounds() {
        let mut input = request();
        assert!(validate(&input).is_ok());
        input.version = 2;
        assert_eq!(validate(&input), Err(MemoryRetrieveError::InvalidRequest));
        input.version = 1;
        input.limit = 17;
        assert!(validate(&input).is_err());
        input.limit = 0;
        assert!(validate(&input).is_err());
    }

    #[test]
    fn memory_scoped_typed_database_bounds_and_rejects_foreign_scope() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let database = switchy_database_connection::init(None, None).await.unwrap();
            // Initialization is explicit fixture setup, never part of the retrieval operation.
            super::super::create_core_tables(database.as_ref())
                .await
                .unwrap();
            database
                .insert("companies")
                .value("id", "default")
                .value("name", "company")
                .value("mission", "mission")
                .value("culture", "culture")
                .value("lifecycle_status", "active")
                .execute(database.as_ref())
                .await
                .unwrap();
            database
                .insert("agents")
                .value("id", "a")
                .value("name", "agent")
                .value("role", "engineer")
                .value("department_id", "d")
                .value("team_id", "t")
                .value("status", "active")
                .value("room_id", "r")
                .execute(database.as_ref())
                .await
                .unwrap();
            database
                .insert("blims_schema_version")
                .value("version", 1_i64)
                .execute(database.as_ref())
                .await
                .unwrap();
            for index in 0..18 {
                database
                    .insert("memories")
                    .value("id", format!("a-{index:02}"))
                    .value("agent_id", "a")
                    .value("kind", "discovery")
                    .value("summary", format!("original-{index}"))
                    .value("importance", 1_i64)
                    .execute(database.as_ref())
                    .await
                    .unwrap();
            }
            database
                .insert("memories")
                .value("id", "foreign")
                .value("agent_id", "b")
                .value("kind", "discovery")
                .value("summary", "foreign-secret")
                .value("importance", 100_i64)
                .execute(database.as_ref())
                .await
                .unwrap();
            let mut input = request();
            let response = retrieve_in_database(&input, database.as_ref())
                .await
                .unwrap();
            assert_eq!(response.hits.len(), 16);
            assert!(!response.complete);
            assert_eq!(response.hits[0].id, "a-00");
            assert!(response.hits.iter().all(|hit| hit.agent_id == "a"));
            input.company_id = "foreign".into();
            assert!(matches!(
                retrieve_in_database(&input, database.as_ref()).await,
                Err(MemoryRetrieveError::ScopeUnavailable)
            ));
            input.company_id = "default".into();
            input.agent_id = "missing".into();
            assert!(matches!(
                retrieve_in_database(&input, database.as_ref()).await,
                Err(MemoryRetrieveError::ScopeUnavailable)
            ));
            input.agent_id = "a".into();
            database
                .update("memories")
                .filter(Box::new(where_eq("id", "a-00")))
                .value("summary", "x".repeat(9000))
                .execute(database.as_ref())
                .await
                .unwrap();
            let response = retrieve_in_database(&input, database.as_ref())
                .await
                .unwrap();
            assert!(response.hits.is_empty());
            assert!(!response.complete);
            assert!(serde_json::to_vec(&response).unwrap().len() <= 8192);
        });
    }
}
