//! Test presentation contexts.

use bcode_session_models::SessionId;

use super::{PresentationAction, PresentationContext};

/// Deterministic guarded context used to verify that opaque destinations propagate unchanged.
#[derive(Debug, Clone, Copy)]
pub struct GuardedTestContext;

impl PresentationContext for GuardedTestContext {
    fn action_target(&self, action: PresentationAction) -> String {
        let path = match action {
            PresentationAction::SubmitMessage => "/actions/submit-message".to_owned(),
            PresentationAction::CancelTurn => "/actions/cancel-turn".to_owned(),
            PresentationAction::UpdateDraft { session_id } => {
                format!("/actions/update-draft/{session_id}")
            }
            PresentationAction::ResolvePermission => "/actions/permission".to_owned(),
            PresentationAction::ResolvePermissionBatch => "/actions/permission-batch".to_owned(),
            PresentationAction::MoveHistoryWindow => "/actions/history-window".to_owned(),
            PresentationAction::ResolveInteraction => "/actions/interaction".to_owned(),
        };
        format!("{path}?test-capability=opaque")
    }

    fn session_target(&self, session_id: SessionId) -> String {
        format!(
            "/session/{session_id}?test-capability=opaque&test-event-scope=session-{session_id}"
        )
    }

    fn artifact_target(
        &self,
        session_id: SessionId,
        artifact_id: &str,
        reference_key: &str,
    ) -> Option<String> {
        Some(format!(
            "/artifacts/{session_id}?test-capability=opaque&artifact_id={artifact_id}&reference_key={reference_key}"
        ))
    }
}
