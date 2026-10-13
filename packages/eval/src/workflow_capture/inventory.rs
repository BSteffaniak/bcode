//! Exhaustive current-fact evidence, independent of historical capture completeness.
use super::super::{EvalError, Path, write_json};
use bcode_workflow::{
    WORKFLOW_INVENTORY_VERSION, WorkflowInventoryCoverage, WorkflowInventoryEntry,
    WorkflowInventoryPage, WorkflowInventoryQuery, WorkflowInventorySessionLink,
    WorkflowInventorySource,
};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Capture {
    pub sessions: BTreeSet<String>,
    pub runs: BTreeSet<String>,
    pub outputs: Vec<bcode_workflow::WorkflowOutputInspection>,
    pub attempts: Vec<bcode_workflow::AttemptSummary>,
}

fn invalid() -> EvalError {
    EvalError::Validation("canonical inventory evidence unavailable or inconsistent".into())
}

fn remaining(deadline: Instant) -> Result<Duration, EvalError> {
    let duration = deadline.saturating_duration_since(Instant::now());
    if duration.is_zero() {
        Err(invalid())
    } else {
        Ok(duration.min(Duration::from_secs(15)))
    }
}

/// Drain every source for every verified descendant and recheck source generations.
/// No inventory membership or EOF is promoted to semantic history completeness.
pub fn capture(
    root: &str,
    directory: &Path,
    deadline: Instant,
    invoke: impl Fn(&[String], &str, Duration) -> Result<serde_json::Value, EvalError>,
) -> Result<Capture, EvalError> {
    let mut result = Capture::default();
    let mut pending = BTreeSet::from([root.to_owned()]);
    let mut parents = BTreeMap::new();
    let mut pins = Vec::new();
    let mut revisions = BTreeMap::new();
    let mut serial = 0_usize;
    while let Some(run) = pending.pop_first() {
        if !result.runs.insert(run.clone()) {
            return Err(invalid());
        }
        let label = format!("inventory-terminal-{}", result.runs.len());
        let value = invoke(
            &[
                "workflow".into(),
                "inspect-run".into(),
                "--run-id".into(),
                run.clone(),
                "--limit".into(),
                "1".into(),
            ],
            &label,
            remaining(deadline)?,
        )?;
        write_json(directory.join(format!("{label}.json")), &value)?;
        remaining(deadline)?;
        if value
            .pointer("/run/run_id")
            .and_then(serde_json::Value::as_str)
            != Some(run.as_str())
            || !matches!(
                value
                    .pointer("/run/status")
                    .and_then(serde_json::Value::as_str),
                Some("completed" | "failed" | "cancelled")
            )
        {
            return Err(invalid());
        }
        for source in [
            WorkflowInventorySource::Outputs,
            WorkflowInventorySource::AttemptContributors,
            WorkflowInventorySource::ChildRuns,
        ] {
            let mut query = WorkflowInventoryQuery {
                version: WORKFLOW_INVENTORY_VERSION,
                run_id: run.clone(),
                source,
                cursor: None,
                limit: 100,
            };
            loop {
                let args = arguments(&query)?;
                let label = format!("inventory-{serial}");
                serial = serial.checked_add(1).ok_or_else(invalid)?;
                let value = invoke(&args, &label, remaining(deadline)?)?;
                write_json(directory.join(format!("{label}.json")), &value)?;
                remaining(deadline)?;
                let page: WorkflowInventoryPage = serde_json::from_value(value)?;
                query.validate_page(&page).map_err(|_| invalid())?;
                if page.coverage != WorkflowInventoryCoverage::CurrentInventoryComplete {
                    return Err(invalid());
                }
                if revisions
                    .insert(run.clone(), page.cursor.graph_revision)
                    .is_some_and(|revision| revision != page.cursor.graph_revision)
                {
                    return Err(invalid());
                }
                if query.cursor.is_none() {
                    pins.push(page.cursor.clone());
                }
                for entry in page.entries {
                    match entry {
                        WorkflowInventoryEntry::Output { output } => {
                            if bcode_workflow::workflow_canonical_value_sha256(&output.value)
                                .map_err(|_| invalid())?
                                != output.checksum_sha256
                            {
                                return Err(invalid());
                            }
                            result.outputs.push(output);
                        }
                        WorkflowInventoryEntry::OutputUnavailable { .. } => return Err(invalid()),
                        WorkflowInventoryEntry::AttemptContributor {
                            attempt, session, ..
                        } => {
                            if attempt.terminal_at_ms.is_none()
                                || (attempt.status == "succeeded" && !attempt.has_receipt)
                                || !matches!(
                                    attempt.status.as_str(),
                                    "succeeded" | "failed" | "cancelled"
                                )
                            {
                                return Err(invalid());
                            }
                            match session {
                                WorkflowInventorySessionLink::Verified { session_id, .. } => {
                                    result.sessions.insert(session_id);
                                }
                                WorkflowInventorySessionLink::NotApplicable {
                                    reason: bcode_workflow::WorkflowInventorySessionNotApplicableReason::CanonicalNonSessionNode,
                                } => {}
                                WorkflowInventorySessionLink::Unavailable { .. } => {
                                    return Err(invalid());
                                }
                            }
                            result.attempts.push(attempt);
                        }
                        WorkflowInventoryEntry::ChildRun {
                            child_run_id,
                            root_run_id,
                            parent,
                        } => {
                            if root_run_id != root
                                || parent.run_id != run
                                || child_run_id == root
                                || parents.insert(child_run_id.clone(), run.clone()).is_some()
                                || result.runs.contains(&child_run_id)
                            {
                                return Err(invalid());
                            }
                            pending.insert(child_run_id);
                        }
                    }
                }
                match page.next_cursor {
                    Some(cursor) => query.cursor = Some(cursor),
                    None => break,
                }
            }
        }
    }
    // Even a one-page source must still have the same generation after descendant drain.
    for (index, pin) in pins.into_iter().enumerate() {
        let query = WorkflowInventoryQuery {
            version: WORKFLOW_INVENTORY_VERSION,
            run_id: pin.run_id.clone(),
            source: pin.source,
            cursor: Some(pin),
            limit: 1,
        };
        let label = format!("inventory-recheck-{index}");
        let value = invoke(&arguments(&query)?, &label, remaining(deadline)?)?;
        write_json(directory.join(format!("{label}.json")), &value)?;
        remaining(deadline)?;
        let page: WorkflowInventoryPage = serde_json::from_value(value)?;
        query.validate_page(&page).map_err(|_| invalid())?;
        if page.coverage != WorkflowInventoryCoverage::CurrentInventoryComplete {
            return Err(invalid());
        }
    }
    Ok(result)
}

fn arguments(query: &WorkflowInventoryQuery) -> Result<Vec<String>, EvalError> {
    let source = match query.source {
        WorkflowInventorySource::Outputs => "outputs",
        WorkflowInventorySource::AttemptContributors => "attempt_contributors",
        WorkflowInventorySource::ChildRuns => "child_runs",
    };
    let mut args = vec![
        "workflow".into(),
        "inventory-run".into(),
        "--run-id".into(),
        query.run_id.clone(),
        "--source".into(),
        source.into(),
        "--limit".into(),
        query.limit.to_string(),
    ];
    if let Some(cursor) = &query.cursor {
        args.extend(["--cursor-json".into(), serde_json::to_string(cursor)?]);
    }
    Ok(args)
}

#[cfg(test)]
mod tests;
