//! Bounded canonical membership inspection for the pre-dispatch diagnostic hook.

use super::super::ServerState;
use bcode_workflow_store::{WorkflowExecutionAuthority, WorkflowExecutionSessionLink};

/// Diagnostic failure must never change dispatch or persisted execution outcomes.
/// Canonical execution-session persistence and authority verification precede this call.
pub(super) async fn before_dispatch(
    state: &ServerState,
    authority: &WorkflowExecutionAuthority,
    link: &WorkflowExecutionSessionLink,
    _dedicated_session: bool,
) {
    let registry = &state.xray_execution_watches;
    if !registry.active() {
        return;
    }
    let Ok(session_id) = link.session_id.parse() else {
        return;
    };
    let mut ancestors = Vec::new();
    let mut next = link.run_id.clone();
    let mut complete = false;
    for _ in 0..registry.ancestry_budget() {
        match super::super::child::diagnostic_parent_context(state, &next, authority).await {
            Ok((root, parent)) => {
                // The canonical root grants arbitrary recursive depth without walking
                // every generation. Subtree grants use the bounded ancestor walk.
                if root != link.run_id && !ancestors.contains(&root) {
                    ancestors.push(root);
                }
                let Some(parent) = parent else {
                    complete = true;
                    break;
                };
                if parent == link.run_id || parent == next {
                    break;
                }
                next.clone_from(&parent);
                if !ancestors.contains(&parent) {
                    ancestors.push(parent);
                }
            }
            Err(_) => break,
        }
    }
    if !state
        .reauthorize_execution_dispatch(&link.run_id, &ancestors)
        .await
    {
        return;
    }
    // Reverify the exact authority after asynchronous ancestry and policy reads. This is a
    // diagnostic admission check, not acquisition or transfer of execution authority.
    if super::super::child::diagnostic_parent_context(state, &link.run_id, authority)
        .await
        .is_err()
    {
        let _ = registry.ownership_changed(&link.run_id);
        return;
    }
    registry.before_dispatch(
        &state.xray,
        &link.run_id,
        session_id,
        authority,
        &ancestors,
        complete,
    );
}
