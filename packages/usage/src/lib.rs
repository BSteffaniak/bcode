#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Checked reporting over normalized, generation-fenced accounting pages.

use bcode_session_models::{SessionCostEstimate, SessionTokenUsage};
use bcode_usage_models::{
    USAGE_VERSION, UsageBucket, UsageModel, UsageModelRow, UsageQuery, UsageReport,
    UsageRequestRow, UsageTotals,
};
pub mod index;

use std::collections::BTreeMap;

/// Exact captured grouping identity.
#[must_use]
pub fn model(usage: &SessionTokenUsage) -> UsageModel {
    UsageModel {
        provider: usage.catalog_provider_id.clone(),
        model: usage.catalog_entry_id.clone(),
    }
}

fn add(target: &mut u64, value: u64) -> Result<(), String> {
    *target = target
        .checked_add(value)
        .ok_or("usage aggregate overflow")?;
    Ok(())
}

/// Add one accepted contribution, preserving unknown pricing and token coverage.
/// # Errors
/// Rejects malformed valuations or arithmetic overflow without modifying the original totals.
pub fn contribute(totals: &mut UsageTotals, usage: &SessionTokenUsage) -> Result<(), String> {
    usage.validate()?;
    let mut next = totals.clone();
    add(&mut next.requests, 1)?;
    add(
        &mut next.input_tokens,
        u64::from(usage.input_tokens.unwrap_or_default()),
    )?;
    add(
        &mut next.output_tokens,
        u64::from(usage.output_tokens.unwrap_or_default()),
    )?;
    add(
        &mut next.cache_read_tokens,
        u64::from(usage.cached_input_tokens.unwrap_or_default()),
    )?;
    add(
        &mut next.cache_write_tokens,
        u64::from(usage.cache_write_input_tokens.unwrap_or_default()),
    )?;
    if usage.input_tokens.is_none() || usage.output_tokens.is_none() {
        add(&mut next.incomplete_token_requests, 1)?;
    }
    if let Some(SessionCostEstimate::Estimated {
        currency,
        total_micros,
        ..
    }) = &usage.cost
    {
        add(&mut next.priced_requests, 1)?;
        add(
            next.cost_micros.entry(currency.clone()).or_default(),
            *total_micros,
        )?;
    }
    *totals = next;
    Ok(())
}

/// Build one bounded report page. Totals explicitly describe the supplied page, not unseen history.
/// # Errors
/// Rejects invalid queries, oversized input pages, malformed accounting, or overflow.
pub fn report_page(
    query: &UsageQuery,
    rows: Vec<UsageRequestRow>,
    next_after: Option<u64>,
    coverage: String,
) -> Result<UsageReport, String> {
    query.validate()?;
    if rows.len() > query.limit as usize {
        return Err("oversized usage source page".into());
    }
    let mut totals = UsageTotals::default();
    let mut models = BTreeMap::<UsageModel, UsageTotals>::new();
    let mut buckets = BTreeMap::<u64, BTreeMap<UsageModel, UsageTotals>>::new();
    let mut requests = Vec::new();
    for row in rows {
        let identity = model(&row.entry.usage);
        if !query.range.contains(row.entry.first_observed_at_ms)
            || query.session_id.is_some_and(|id| row.session_id != id)
            || (!query.models.is_empty() && !query.models.contains(&identity))
            || (!query.providers.is_empty()
                && !identity
                    .provider
                    .as_ref()
                    .is_some_and(|provider| query.providers.contains(provider)))
        {
            continue;
        }
        contribute(&mut totals, &row.entry.usage)?;
        contribute(
            models.entry(identity.clone()).or_default(),
            &row.entry.usage,
        )?;
        let bucket = row.entry.first_observed_at_ms / query.bucket_ms * query.bucket_ms;
        contribute(
            buckets
                .entry(bucket)
                .or_default()
                .entry(identity)
                .or_default(),
            &row.entry.usage,
        )?;
        requests.push(row);
    }
    Ok(UsageReport {
        revision: query.revision.unwrap_or_default(),
        version: USAGE_VERSION,
        totals,
        models: model_rows(models),
        buckets: buckets
            .into_iter()
            .map(|(timestamp_ms, models)| UsageBucket {
                timestamp_ms,
                models: model_rows(models),
            })
            .collect(),
        requests,
        next_after,
        coverage,
    })
}

fn model_rows(rows: BTreeMap<UsageModel, UsageTotals>) -> Vec<UsageModelRow> {
    rows.into_iter()
        .map(|(model, totals)| UsageModelRow { model, totals })
        .collect()
}

/// Export the exact report, including coverage and currency identities.
/// # Errors
/// Returns an error if the report cannot be serialized.
pub fn export_json(report: &UsageReport) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(report)
}

fn csv_cell(value: &str) -> String {
    // Neutralize spreadsheet formulas in untrusted model/provider/request labels.
    let guarded = if value.starts_with(['=', '+', '-', '@', '\t', '\r', '\n']) {
        format!("'{value}")
    } else {
        value.to_owned()
    };
    format!("\"{}\"", guarded.replace('"', "\"\""))
}

/// Export normalized request estimates without mixing currencies or treating unknown cost as zero.
#[must_use]
pub fn export_csv(report: &UsageReport) -> String {
    use std::fmt::Write;
    let mut output = String::from(
        "session_id,request_key,timestamp_ms,provider,model,currency,cost_micros,coverage\r\n",
    );
    for row in &report.requests {
        let usage = &row.entry.usage;
        let (currency, cost) = match &usage.cost {
            Some(SessionCostEstimate::Estimated {
                currency,
                total_micros,
                ..
            }) => (currency.as_str(), total_micros.to_string()),
            _ => ("", String::new()),
        };
        let _ = writeln!(
            output,
            "{},{},{},{},{},{},{},{}\r",
            row.session_id,
            csv_cell(&row.entry.key),
            row.entry.first_observed_at_ms,
            csv_cell(usage.catalog_provider_id.as_deref().unwrap_or("")),
            csv_cell(usage.catalog_entry_id.as_deref().unwrap_or("")),
            csv_cell(currency),
            cost,
            csv_cell(&report.coverage)
        );
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_cost_is_not_zero_and_overflow_is_atomic() {
        let usage = SessionTokenUsage {
            input_tokens: Some(7),
            cached_input_tokens: Some(5),
            ..Default::default()
        };
        let mut totals = UsageTotals::default();
        contribute(&mut totals, &usage).unwrap();
        assert_eq!(totals.requests, 1);
        assert_eq!(totals.priced_requests, 0);
        assert!(totals.cost_micros.is_empty());
        assert_eq!(totals.input_tokens, 7);
        assert_eq!(totals.cache_read_tokens, 5);
        totals.requests = u64::MAX;
        let before = totals.clone();
        assert!(contribute(&mut totals, &usage).is_err());
        assert_eq!(totals, before);
    }
    #[test]
    fn csv_escapes_quotes_and_formulas() {
        assert_eq!(csv_cell("=1+1"), "\"'=1+1\"");
        assert_eq!(csv_cell("a,\"b\""), "\"a,\"\"b\"\"\"");
    }
}
