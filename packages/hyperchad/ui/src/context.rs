//! Renderer-neutral presentation routing context.

use bcode_session_models::SessionId;

#[cfg(test)]
pub(crate) mod tests;

/// Semantic application operation rendered through a canonical `HyperChad` route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentationAction {
    /// Submit a user message.
    SubmitMessage,
    /// Cancel the active turn.
    CancelTurn,
    /// Persist the draft for a session.
    UpdateDraft { session_id: SessionId },
    /// Resolve one permission request.
    ResolvePermission,
    /// Resolve an entire permission batch.
    ResolvePermissionBatch,
    /// Move through bounded transcript history.
    MoveHistoryWindow,
    /// Deliver input to an interactive tool.
    ResolveInteraction,
}

/// Supplies opaque application destinations for portable presentation components.
///
/// Components select destinations by semantic operation and never receive browser credentials,
/// backend origins, bind configuration, or transport request state.
pub trait PresentationContext {
    /// Resolve a semantic action to a canonical `HyperChad` route target.
    fn action_target(&self, action: PresentationAction) -> String;

    /// Resolve navigation to a session.
    fn session_target(&self, session_id: SessionId) -> String;

    /// Resolve a guarded resource target for one canonical session artifact reference.
    ///
    /// Renderers that cannot expose artifact bytes should return `None`.
    fn artifact_target(
        &self,
        session_id: SessionId,
        artifact_id: &str,
        reference_key: &str,
    ) -> Option<String>;
}

/// Unguarded canonical application routes for static rendering and renderer-neutral tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct StaticPresentationContext;

impl PresentationContext for StaticPresentationContext {
    fn action_target(&self, action: PresentationAction) -> String {
        match action {
            PresentationAction::SubmitMessage => "/actions/submit-message".to_owned(),
            PresentationAction::CancelTurn => "/actions/cancel-turn".to_owned(),
            PresentationAction::UpdateDraft { session_id } => {
                format!("/actions/update-draft/{session_id}")
            }
            PresentationAction::ResolvePermission => "/actions/permission".to_owned(),
            PresentationAction::ResolvePermissionBatch => "/actions/permission-batch".to_owned(),
            PresentationAction::MoveHistoryWindow => "/actions/history-window".to_owned(),
            PresentationAction::ResolveInteraction => "/actions/interaction".to_owned(),
        }
    }

    fn session_target(&self, session_id: SessionId) -> String {
        format!("/session/{session_id}")
    }

    fn artifact_target(
        &self,
        _session_id: SessionId,
        _artifact_id: &str,
        _reference_key: &str,
    ) -> Option<String> {
        None
    }
}
