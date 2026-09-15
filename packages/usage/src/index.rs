//! Disposable in-memory request index with atomic per-session publication.

use bcode_session_models::{SessionId, SessionUsageGeneration, SessionUsagePage};
use bcode_usage_models::{UsageQuery, UsageReport, UsageRequestRow};
use std::collections::BTreeMap;

#[derive(Debug, Default)]
struct SessionIndex {
    generation: Option<SessionUsageGeneration>,
    next_after: Option<String>,
    rows: BTreeMap<String, UsageRequestRow>,
}

/// Disposable reporting index. Collection is explicit; normal queries never repair sources.
/// A daemon restart discards this cache and must report missing coverage rather than zero spend.
#[derive(Debug, Default)]
pub struct UsageIndex {
    sessions: BTreeMap<SessionId, SessionIndex>,
    staging: BTreeMap<SessionId, SessionIndex>,
    next_ordinal: u64,
    rows: BTreeMap<u64, UsageRequestRow>,
    revision: u64,
}

impl UsageIndex {
    /// Accept one bounded source page into an explicit collection operation.
    /// Publication replaces the session atomically only after its complete generation is collected.
    /// # Errors
    /// Rejects missing/duplicate/out-of-order pages, changed generations, and invalid source bounds.
    pub fn collect(
        &mut self,
        session_id: SessionId,
        after: Option<&str>,
        page: SessionUsagePage,
    ) -> Result<bool, String> {
        if page.scanned > 256 || page.entries.len() > page.scanned as usize {
            return Err("oversized usage collection page".into());
        }
        if after.is_none() {
            self.staging.insert(
                session_id,
                SessionIndex {
                    generation: Some(page.generation),
                    ..Default::default()
                },
            );
        }
        let staged = self
            .staging
            .get_mut(&session_id)
            .ok_or("usage collection has not started")?;
        if staged.generation != Some(page.generation) || staged.next_after.as_deref() != after {
            return Err("usage collection changed; restart this session".into());
        }
        let mut previous = after.unwrap_or_default();
        for entry in &page.entries {
            if entry.key.as_str() <= previous {
                return Err("unordered usage contribution page".into());
            }
            entry.usage.validate()?;
            previous = &entry.key;
        }
        if page
            .next_after
            .as_deref()
            .is_some_and(|next| next <= after.unwrap_or_default() || next < previous)
        {
            return Err("invalid usage collection continuation".into());
        }
        let count = u64::try_from(page.entries.len()).map_err(|_| "usage index overflow")?;
        let end_ordinal = self
            .next_ordinal
            .checked_add(count)
            .ok_or("usage index overflow")?;
        let revision = self
            .revision
            .checked_add(1)
            .ok_or("usage revision overflow")?;
        for entry in page.entries {
            self.next_ordinal += 1;
            staged.rows.insert(
                entry.key.clone(),
                UsageRequestRow {
                    ordinal: self.next_ordinal,
                    session_id,
                    entry,
                },
            );
        }
        debug_assert_eq!(self.next_ordinal, end_ordinal);
        staged.next_after = page.next_after;
        if staged.next_after.is_some() {
            return Ok(false);
        }
        let completed = self
            .staging
            .remove(&session_id)
            .ok_or("missing usage staging")?;
        if let Some(previous) = self.sessions.remove(&session_id) {
            for row in previous.rows.into_values() {
                self.rows.remove(&row.ordinal);
            }
        }
        for row in completed.rows.values() {
            self.rows.insert(row.ordinal, row.clone());
        }
        self.sessions.insert(session_id, completed);
        self.revision = revision;
        Ok(true)
    }

    /// Query at most one page of indexed contributions; empty filtered pages may continue.
    /// # Errors
    /// Rejects invalid requests or invalid normalized accounting.
    pub fn query(&self, query: &UsageQuery) -> Result<UsageReport, String> {
        use std::ops::Bound::{Excluded, Unbounded};
        query.validate()?;
        if query
            .revision
            .is_some_and(|revision| revision != self.revision)
            || (query.after.is_some() && query.revision.is_none())
        {
            return Err("usage index changed; restart query".into());
        }
        let mut query = query.clone();
        query.revision = Some(self.revision);
        let rows: Vec<_> = self
            .rows
            .range((Excluded(query.after.unwrap_or_default()), Unbounded))
            .take(query.limit as usize)
            .map(|(_, row)| row.clone())
            .collect();
        let next_after = if rows.len() == query.limit as usize {
            rows.last().map(|row| row.ordinal)
        } else {
            None
        };
        crate::report_page(
            &query,
            rows,
            next_after,
            format!(
                "Page subtotal only; {} explicitly collected sessions; {} collecting; snapshot revision {}. Sources may have changed; recollect to refresh. Cache is not durable.",
                self.sessions.len(),
                self.staging.len(),
                self.revision
            ),
        )
    }

    /// Invalidate a removed or unverifiable session without selecting another storage location.
    /// # Errors
    /// Rejects revision overflow without changing the index.
    pub fn invalidate(&mut self, session_id: SessionId) -> Result<(), String> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or("usage revision overflow")?;
        self.staging.remove(&session_id);
        if let Some(previous) = self.sessions.remove(&session_id) {
            for row in previous.rows.into_values() {
                self.rows.remove(&row.ordinal);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{SessionCostRange, SessionTokenUsage, SessionUsageEntry};
    use bcode_usage_models::USAGE_VERSION;
    #[test]
    fn publication_is_atomic_across_pages_and_recollection_replaces() {
        let id = SessionId::new();
        let generation = SessionUsageGeneration {
            through_sequence: Some(3),
            cost_revision: 0,
        };
        let page = |key: &str, next: Option<&str>| SessionUsagePage {
            generation,
            scanned: 1,
            entries: vec![SessionUsageEntry {
                key: key.into(),
                first_observed_at_ms: 5,
                usage: SessionTokenUsage::default(),
            }],
            next_after: next.map(str::to_owned),
        };
        let query = UsageQuery {
            version: USAGE_VERSION,
            range: SessionCostRange {
                from_timestamp_ms: 0,
                to_timestamp_ms: 10,
            },
            models: std::collections::BTreeSet::new(),
            providers: std::collections::BTreeSet::new(),
            session_id: None,
            bucket_ms: 10,
            after: None,
            revision: None,
            limit: 256,
        };
        let mut index = UsageIndex::default();
        assert!(!index.collect(id, None, page("a", Some("a"))).unwrap());
        assert_eq!(index.query(&query).unwrap().totals.requests, 0);
        assert!(index.collect(id, Some("a"), page("b", None)).unwrap());
        assert_eq!(index.query(&query).unwrap().totals.requests, 2);
        assert!(index.collect(id, None, page("a", None)).unwrap());
        assert_eq!(index.query(&query).unwrap().totals.requests, 1);
        index.invalidate(id).unwrap();
        assert_eq!(index.query(&query).unwrap().totals.requests, 0);
    }
}
