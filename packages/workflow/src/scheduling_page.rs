//! Portable scheduling-page application contracts. Cursors are application-owned opaque tokens.
use serde::{Deserialize, Serialize};

/// Scheduling pagination protocol version.
pub const WORKFLOW_SCHEDULING_PAGE_VERSION: u32 = 1;

/// Request one bounded page. No cursor means start a fresh observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSchedulingPageRequest {
    pub version: u32,
    pub run_id: String,
    pub limit: usize,
    /// Exact opaque token returned by the application, never a displayed activation ID.
    pub cursor: Option<String>,
}

impl WorkflowSchedulingPageRequest {
    /// Validate public protocol and bounded input before decoding an application cursor.
    /// # Errors
    /// Rejects unknown versions, empty run IDs, invalid page bounds, and oversized/empty tokens.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != WORKFLOW_SCHEDULING_PAGE_VERSION {
            return Err("unsupported scheduling page version".into());
        }
        if self.run_id.is_empty() || self.run_id.len() > 1024 || !(1..=100).contains(&self.limit) {
            return Err("invalid scheduling page request bounds".into());
        }
        if self
            .cursor
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.len() > 16384)
        {
            return Err("invalid scheduling cursor bounds".into());
        }
        Ok(())
    }
}

/// Application response envelope; `T` is the portable scheduling view payload.
/// Pages pin graph revision and representation, not a durable historical execution snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSchedulingPage<T> {
    pub version: u32,
    pub run_id: String,
    pub graph_revision: u64,
    pub observation: T,
    /// Exact next-page token, bound by the application to run, graph revision and representation.
    pub next_cursor: Option<String>,
}

impl<T> WorkflowSchedulingPage<T> {
    /// Validate the envelope before consuming its payload. Payload compatibility is independent.
    /// # Errors
    /// Rejects unsupported versions, wrong run/revision and malformed token bounds.
    pub fn validate(&self, run_id: &str, revision: Option<u64>) -> Result<(), String> {
        if self.version != WORKFLOW_SCHEDULING_PAGE_VERSION
            || self.run_id != run_id
            || revision.is_some_and(|revision| revision != self.graph_revision)
        {
            return Err("unsupported or mismatched scheduling page; restart observation".into());
        }
        if self
            .next_cursor
            .as_ref()
            .is_some_and(|cursor| cursor.is_empty() || cursor.len() > 16384)
        {
            return Err("invalid scheduling continuation token".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cursor_round_trip_is_exact_and_unknown_versions_fail_closed() {
        let mut request = WorkflowSchedulingPageRequest {
            version: 1,
            run_id: "run".into(),
            limit: 100,
            cursor: Some("opaque:界/abc+==".into()),
        };
        assert!(request.validate().is_ok());
        assert_eq!(
            serde_json::from_str::<WorkflowSchedulingPageRequest>(
                &serde_json::to_string(&request).unwrap()
            )
            .unwrap(),
            request
        );
        request.version = 2;
        assert!(request.validate().is_err());
        let response = WorkflowSchedulingPage {
            version: 1,
            run_id: "run".into(),
            graph_revision: 3,
            observation: (),
            next_cursor: request.cursor,
        };
        assert!(response.validate("run", Some(3)).is_ok());
        assert!(response.validate("other", Some(3)).is_err());
        assert!(response.validate("run", Some(4)).is_err());
    }
}
