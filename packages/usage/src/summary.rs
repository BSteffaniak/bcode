//! Explicit whole-range aggregation over revision-fenced reporting pages.
use bcode_usage_models::{
    USAGE_VERSION, UsageBucket, UsageModel, UsageModelRow, UsageReport, UsageTotals,
};
use std::collections::BTreeMap;

/// Bounded aggregate state for an explicit reporting traversal, not a history cache.
/// Request rows are discarded. More than 256 models or 366 buckets fails explicitly.
#[derive(Debug, Clone, Default)]
pub struct UsageSummaryAccumulator {
    revision: Option<u64>,
    after: Option<u64>,
    finished: bool,
    totals: UsageTotals,
    models: BTreeMap<UsageModel, UsageTotals>,
    buckets: BTreeMap<u64, BTreeMap<UsageModel, UsageTotals>>,
}

impl UsageSummaryAccumulator {
    /// Accept the next page from the same query and index revision.
    /// # Errors
    /// Rejects duplicates, nonadvancing cursors, changed revisions, overflow, and oversized summaries.
    /// Failures do not modify previously accepted totals.
    pub fn accept(&mut self, after: Option<u64>, page: &UsageReport) -> Result<(), String> {
        if self.finished
            || after != self.after
            || page.version != USAGE_VERSION
            || self
                .revision
                .is_some_and(|revision| revision != page.revision)
            || page
                .next_after
                .is_some_and(|next| next <= after.unwrap_or_default())
        {
            return Err("usage summary page changed or arrived out of order".into());
        }
        if page.models.len() > 256
            || page.buckets.len() > 366
            || page.buckets.iter().any(|bucket| bucket.models.len() > 256)
        {
            return Err("oversized usage summary page".into());
        }
        let mut next = self.clone();
        merge(&mut next.totals, &page.totals)?;
        for row in &page.models {
            merge(
                next.models.entry(row.model.clone()).or_default(),
                &row.totals,
            )?;
        }
        for bucket in &page.buckets {
            let models = next.buckets.entry(bucket.timestamp_ms).or_default();
            for row in &bucket.models {
                merge(models.entry(row.model.clone()).or_default(), &row.totals)?;
            }
        }
        if next.models.len() > 256
            || next.buckets.len() > 366
            || next.buckets.values().any(|models| models.len() > 256)
        {
            return Err("usage summary too large; narrow the model or time filters".into());
        }
        next.revision = Some(page.revision);
        next.after = page.next_after;
        next.finished = page.next_after.is_none();
        *self = next;
        Ok(())
    }

    /// Finish only after end-of-source. Completeness applies to indexed snapshots, not provider invoices.
    /// # Errors
    /// Rejects unfinished or empty traversals.
    pub fn finish(self) -> Result<UsageReport, String> {
        if !self.finished {
            return Err("usage summary traversal is incomplete".into());
        }
        Ok(UsageReport {
            revision: self.revision.ok_or("missing usage revision")?, version: USAGE_VERSION,
            totals: self.totals, models: rows(self.models),
            buckets: self.buckets.into_iter().map(|(timestamp_ms, models)| UsageBucket { timestamp_ms, models: rows(models) }).collect(),
            requests: Vec::new(), next_after: None,
            coverage: "Whole-range totals of indexed snapshots; source freshness and catalog completeness remain unverified. Request rows omitted from summary.".into(),
        })
    }
}
fn rows(models: BTreeMap<UsageModel, UsageTotals>) -> Vec<UsageModelRow> {
    models
        .into_iter()
        .map(|(model, totals)| UsageModelRow { model, totals })
        .collect()
}
pub(crate) fn merge(target: &mut UsageTotals, source: &UsageTotals) -> Result<(), String> {
    for (target, source) in [
        (&mut target.requests, source.requests),
        (&mut target.priced_requests, source.priced_requests),
        (&mut target.input_tokens, source.input_tokens),
        (&mut target.output_tokens, source.output_tokens),
        (&mut target.cache_read_tokens, source.cache_read_tokens),
        (&mut target.cache_write_tokens, source.cache_write_tokens),
        (
            &mut target.incomplete_token_requests,
            source.incomplete_token_requests,
        ),
    ] {
        *target = target.checked_add(source).ok_or("usage summary overflow")?;
    }
    for (currency, amount) in &source.cost_micros {
        let total = target.cost_micros.entry(currency.clone()).or_default();
        *total = total
            .checked_add(*amount)
            .ok_or("usage cost summary overflow")?;
    }
    if target.cost_micros.len() > 256 {
        return Err("too many usage currencies".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn page(next_after: Option<u64>) -> UsageReport {
        UsageReport {
            version: USAGE_VERSION,
            revision: 4,
            totals: UsageTotals {
                requests: 1,
                cost_micros: BTreeMap::from([("USD".into(), 5)]),
                ..UsageTotals::default()
            },
            models: Vec::new(),
            buckets: Vec::new(),
            requests: Vec::new(),
            next_after,
            coverage: "page".into(),
        }
    }
    #[test]
    fn whole_range_requires_end_and_rejects_changed_revision() {
        let mut summary = UsageSummaryAccumulator::default();
        summary.accept(None, &page(Some(256))).unwrap();
        assert!(summary.clone().finish().is_err());
        let mut changed = page(None);
        changed.revision = 5;
        assert!(summary.accept(Some(256), &changed).is_err());
        summary.accept(Some(256), &page(None)).unwrap();
        assert!(summary.accept(Some(256), &page(None)).is_err());
        let report = summary.finish().unwrap();
        assert_eq!(report.totals.requests, 2);
        assert_eq!(report.totals.cost_micros["USD"], 10);
    }
    #[test]
    fn empty_filtered_pages_still_require_continuation() {
        let mut summary = UsageSummaryAccumulator::default();
        let mut empty = page(Some(256));
        empty.totals = UsageTotals::default();
        summary.accept(None, &empty).unwrap();
        summary.accept(Some(256), &page(None)).unwrap();
        assert_eq!(summary.finish().unwrap().totals.requests, 1);
    }
}
