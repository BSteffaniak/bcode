//! Portable cross-session reporting over the selected state location's usage index.

use std::future::Future;

/// Secret-safe usage reporting failure. Unknown codes remain failures, not empty reports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UsageApplicationFailure {
    /// Extensible machine-readable category.
    pub code: String,
    /// Normalized public message.
    pub message: String,
}

/// Caller-bound access to bounded, revision-fenced accounting snapshots.
///
/// Reads do not collect usage, repair canonical sessions, or rebuild reporting indexes.
/// Continuations retain the query's revision and cursor; conflicts require restarting.
pub trait UsageApplication: Sync {
    /// Query one cross-session report page within the host's selected state location.
    ///
    /// # Errors
    /// Rejects stopped applications, unsupported or invalid queries, unavailable reporting
    /// state, incompatible session drill-downs, and changed snapshot revisions.
    fn usage_report(
        &self,
        query: crate::UsageQuery,
    ) -> impl Future<Output = Result<crate::UsageReport, UsageApplicationFailure>> + Send;
}
