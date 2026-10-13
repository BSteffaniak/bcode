//! Bounded canonical descendant observation in the aggregate's existing read snapshot.
use super::{ServerError, representation, stage};
use bcode_workflow::{WorkflowDescendantRunSummary, WorkflowRunLink};
use std::collections::{BTreeSet, VecDeque};
#[cfg(test)]
mod tests;

pub(super) async fn inspect(
    tx: &dyn switchy_database::DatabaseTransaction,
    root: &str,
    children: &[WorkflowRunLink],
    children_complete: bool,
    bound: u16,
) -> Result<(Vec<WorkflowDescendantRunSummary>, bool), ServerError> {
    let mut queue = VecDeque::from(children.to_vec());
    let mut seen = BTreeSet::from([root.to_owned()]);
    let mut result = Vec::new();
    let mut complete = children_complete;
    while let Some(link) = queue.pop_front() {
        if result.len() == usize::from(bound) {
            return Ok((result, false));
        }
        if !seen.insert(link.child_run_id.clone()) {
            return Err(stage("descendant_cycle"));
        }
        let run = representation::runs::summary_in(tx, &link.child_run_id)
            .await
            .map_err(|_| stage("descendant_run"))?
            .ok_or_else(|| stage("descendant_missing"))?;
        let page = representation::hierarchy::children_in(tx, &link.child_run_id, None, bound)
            .await
            .map_err(|_| stage("descendant_children"))?;
        complete &= page.next_child_run_id.is_none();
        // Bound retained breadth independently of hierarchy lifetime and depth.
        let remaining = usize::from(bound).saturating_sub(result.len() + queue.len() + 1);
        complete &= page.children.len() <= remaining;
        queue.extend(page.children.into_iter().take(remaining));
        result.push(WorkflowDescendantRunSummary { link, run });
    }
    Ok((result, complete))
}
