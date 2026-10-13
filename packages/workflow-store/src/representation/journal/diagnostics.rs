//! Producer-defined public diagnostics from committed transactional observations.
//! Never executable step events or full semantic history coverage.
use super::{CoverageOrigin, Database, Error, ScalarFact, SourceCoverage};
use serde::{Deserialize, Serialize};

/// One exact retained diagnostic or an explicit missing recorded-time boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DiagnosticRecord {
    /// Secret-safe row transition with the original recorded source time.
    Event {
        event: bcode_workflow::WorkflowHistoryEvent,
    },
    /// A transition exists, but cannot satisfy the public event time contract.
    TimestampUnavailable { sequence: u64 },
}

/// Bounded declared diagnostics, not full workflow history or a durable resume token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticPage {
    pub version: u32,
    pub run_id: String,
    pub origin: CoverageOrigin,
    pub sources: Vec<SourceCoverage>,
    pub limitations: Vec<String>,
    pub through_sequence: u64,
    pub next_after_sequence: Option<u64>,
    pub records: Vec<DiagnosticRecord>,
}

/// Project recorded row-transition facts without interpreting owner payloads.
/// # Errors
/// Rejects unsupported query versions, invalid/stale future pins, unavailable installation,
/// damaged origins or unsupported source observations. Missing time is explicit, never guessed.
pub async fn page(
    db: &dyn Database,
    version: u32,
    run: &str,
    after: u64,
    through: Option<u64>,
    limit: u16,
) -> Result<DiagnosticPage, Error> {
    if version != 1 {
        return Err(Error::UnsupportedLifecycle);
    }
    let page = super::page(db, run, after, through, limit).await?;
    let mut records = Vec::new();
    for row in page.observations {
        if !page.sources.iter().any(|source| {
            source.table == row.source_table && source.operations.contains(&row.operation)
        }) {
            return Err(Error::MaintenanceRequired);
        }
        let Some(created_at_ms) = row.timestamp_ms else {
            records.push(DiagnosticRecord::TimestampUnavailable {
                sequence: row.sequence,
            });
            continue;
        };
        // Selected source facts are already bounded by typed observers. No message,
        // intent, receipt, evidence, configuration or private JSON is selected here.
        let facts: std::collections::BTreeMap<_, _> = row
            .facts
            .into_iter()
            .filter(|(name, _)| super::observers::FACTS.contains(&name.as_str()))
            .collect::<std::collections::BTreeMap<String, ScalarFact>>();
        records.push(DiagnosticRecord::Event {
            event: bcode_workflow::WorkflowHistoryEvent {
                event_seq: row.sequence,
                run_id: row.run_id,
                event_type: "row_transition_v1".into(),
                created_at_ms,
                payload: serde_json::json!({"version":1,"source":row.source_table,
                "operation":row.operation,"facts":facts,"timestamp_source":row.timestamp_source}),
            },
        });
    }
    Ok(DiagnosticPage {
        version: 1,
        run_id: page.run_id,
        origin: page.origin,
        sources: page.sources,
        limitations: page.limitations,
        through_sequence: page.through_sequence,
        next_after_sequence: page.next_after_sequence,
        records,
    })
}

#[cfg(test)]
pub(super) mod tests;
