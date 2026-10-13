//! Exact public semantic scopes. Exhaustion is scoped, never full workflow acceptance.
use super::{EvalError, Path, write_json};
use bcode_workflow::{
    WORKFLOW_LIFECYCLE_HISTORY_VERSION, WORKFLOW_SEMANTIC_HISTORY_VERSION,
    WorkflowLifecycleHistoryPage, WorkflowLifecycleHistoryQuery, WorkflowLifecycleHistoryScope,
    WorkflowSemanticHistoryCoverage, WorkflowSemanticHistoryPage, WorkflowSemanticHistoryQuery,
    WorkflowSemanticHistoryScope,
};
use serde_json::Value;
use std::time::{Duration, Instant};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy)]
pub enum Scope {
    Attempts,
    Lifecycle,
}

impl Scope {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Attempts => "attempts_v1",
            Self::Lifecycle => "run_lifecycle_v1",
        }
    }

    const fn command(self) -> &'static str {
        match self {
            Self::Attempts => "semantic-history-run",
            Self::Lifecycle => "lifecycle-history-run",
        }
    }

    fn validate(
        self,
        run: &str,
        cursor: Option<Value>,
        value: Value,
    ) -> Result<Validated, EvalError> {
        let invalid =
            || EvalError::Validation("semantic capture page is unsupported or inconsistent".into());
        match self {
            Self::Attempts => {
                let query = WorkflowSemanticHistoryQuery {
                    version: WORKFLOW_SEMANTIC_HISTORY_VERSION,
                    run_id: run.into(),
                    scope: WorkflowSemanticHistoryScope::AttemptsV1,
                    cursor: cursor
                        .map(serde_json::from_value)
                        .transpose()
                        .map_err(|_| invalid())?,
                    limit: 100,
                };
                let page: WorkflowSemanticHistoryPage =
                    serde_json::from_value(value).map_err(|_| invalid())?;
                query.validate_page(&page).map_err(|_| invalid())?;
                Ok(Validated {
                    pin: serde_json::to_value(&page.cursor)?,
                    next: page.next_cursor.map(serde_json::to_value).transpose()?,
                    complete_coverage: page.cursor.coverage
                        == WorkflowSemanticHistoryCoverage::CompleteFromAdmission,
                    rows: page.events.len(),
                })
            }
            Self::Lifecycle => {
                let query = WorkflowLifecycleHistoryQuery {
                    version: WORKFLOW_LIFECYCLE_HISTORY_VERSION,
                    run_id: run.into(),
                    scope: WorkflowLifecycleHistoryScope::RunLifecycleV1,
                    cursor: cursor
                        .map(serde_json::from_value)
                        .transpose()
                        .map_err(|_| invalid())?,
                    limit: 100,
                };
                let page: WorkflowLifecycleHistoryPage =
                    serde_json::from_value(value).map_err(|_| invalid())?;
                query.validate_page(&page).map_err(|_| invalid())?;
                Ok(Validated {
                    pin: serde_json::to_value(&page.cursor)?,
                    next: page.next_cursor.map(serde_json::to_value).transpose()?,
                    complete_coverage: page.cursor.coverage
                        == WorkflowSemanticHistoryCoverage::CompleteFromAdmission,
                    rows: page.events.len(),
                })
            }
        }
    }
}

struct Validated {
    pin: Value,
    next: Option<Value>,
    complete_coverage: bool,
    rows: usize,
}

/// Retain each response before interpreting it, including incompatible/future responses.
/// One shared caller deadline bounds all scopes, not a fresh allowance per page.
pub fn capture(
    scope: Scope,
    run: &str,
    directory: &Path,
    deadline: Instant,
    invoke: &impl Fn(&[String], &str, Duration) -> Result<Value, EvalError>,
) -> (Value, Result<(), EvalError>) {
    let mut pages = 0_u64;
    let mut rows = 0_usize;
    let mut pin = None;
    let mut cursor = None;
    let result = (|| {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(EvalError::Validation(
                    "semantic capture deadline elapsed".into(),
                ));
            }
            let mut args = vec![
                "workflow".into(),
                scope.command().into(),
                "--run-id".into(),
                run.into(),
                "--limit".into(),
                "100".into(),
            ];
            if let Some(value) = &cursor {
                args.extend(["--cursor-json".into(), serde_json::to_string(value)?]);
            }
            let value = invoke(&args, scope.name(), remaining.min(Duration::from_secs(15)))?;
            write_json(
                directory.join(format!("{}-{pages:04}.json", scope.name())),
                &value,
            )?;
            pages += 1;
            if Instant::now() >= deadline {
                return Err(EvalError::Validation(
                    "semantic capture deadline elapsed".into(),
                ));
            }
            let page = scope.validate(run, cursor.take(), value)?;
            pin = Some(page.pin);
            rows += page.rows;
            if !page.complete_coverage {
                return Err(EvalError::Validation(
                    "semantic capture coverage is not complete from admission".into(),
                ));
            }
            let Some(next) = page.next else {
                return Ok(());
            };
            cursor = Some(next);
        }
    })();
    let boundary = serde_json::json!({
        "status": if result.is_ok() { "complete" } else { "partial_or_unavailable" },
        "scope": scope.name(), "pages_retained": pages, "validated_rows": rows,
        "last_validated_cursor": pin, "full_history_complete": false,
    });
    (boundary, result)
}
