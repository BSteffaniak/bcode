//! HTML/Actix presentation routing context.

use crate::RenderSubscriptionScope;
use bcode_hyperchad_ui::context::{PresentationAction, PresentationContext};
use bcode_session_models::SessionId;

/// Selected-backend capability context for guarded browser routes and event scopes.
#[derive(Clone)]
pub struct HtmlActixPresentationContext {
    access_token: std::sync::Arc<str>,
}

impl std::fmt::Debug for HtmlActixPresentationContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HtmlActixPresentationContext")
            .field("access_token", &"[REDACTED]")
            .finish()
    }
}

impl HtmlActixPresentationContext {
    #[must_use]
    pub const fn new(access_token: std::sync::Arc<str>) -> Self {
        Self { access_token }
    }

    /// Construct the opaque renderer scope for one session.
    #[must_use]
    pub fn render_scope(&self, session_id: SessionId) -> RenderSubscriptionScope {
        RenderSubscriptionScope(format!("{}:{session_id}", self.access_token))
    }

    fn guarded(&self, path: &str) -> String {
        format!("{path}?token={}", self.access_token)
    }
}

impl PresentationContext for HtmlActixPresentationContext {
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
        self.guarded(&path)
    }

    fn session_target(&self, session_id: SessionId) -> String {
        format!(
            "/session/{session_id}?token={}&hyperchad-event-scope={}:{session_id}",
            self.access_token, self.access_token
        )
    }

    fn artifact_target(
        &self,
        session_id: SessionId,
        artifact_id: &str,
        reference_key: &str,
    ) -> Option<String> {
        if artifact_id.is_empty() || reference_key.is_empty() {
            return None;
        }
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("token", &self.access_token)
            .append_pair("artifact_id", artifact_id)
            .append_pair("reference_key", reference_key)
            .finish();
        Some(format!("/artifacts/{session_id}?{query}"))
    }
}
