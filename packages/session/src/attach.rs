//! Session client attachment and detachment behavior.

use crate::{
    AttachMode, SessionAttachment, SessionError, SessionManager, SessionProjectionWindowAttachment,
    usize_to_u64,
};
use bcode_session_models::{ClientId, ProjectionWindowRequest, SessionId};

impl SessionManager {
    /// Attach a client to an existing session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist.
    pub async fn attach_session(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<SessionAttachment, SessionError> {
        let total_timer = self.metrics.timer();
        let handle_timer = self.metrics.timer();
        let handle = self.session_handle(session_id).await?;
        self.metrics.record_histogram(
            "session.manager.attach_full.handle_duration_ms",
            handle_timer.elapsed_ms(),
        );
        let attach_timer = self.metrics.timer();
        let result = handle.attach(client_id, AttachMode::Full).await;
        self.metrics.record_histogram(
            "session.manager.attach_full.actor_attach_duration_ms",
            attach_timer.elapsed_ms(),
        );
        self.metrics.record_histogram(
            "session.manager.attach_full.total_duration_ms",
            total_timer.elapsed_ms(),
        );
        result
    }

    /// Attach a client and return only the most recent replayable history events.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist.
    pub async fn attach_session_recent(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        limit: usize,
    ) -> Result<SessionAttachment, SessionError> {
        let total_timer = self.metrics.timer();
        self.metrics
            .record_histogram("session.manager.attach_recent.limit", usize_to_u64(limit));
        let handle_timer = self.metrics.timer();
        let handle = self.session_handle(session_id).await?;
        self.metrics.record_histogram(
            "session.manager.attach_recent.handle_duration_ms",
            handle_timer.elapsed_ms(),
        );
        let attach_timer = self.metrics.timer();
        let result = handle.attach(client_id, AttachMode::Recent { limit }).await;
        self.metrics.record_histogram(
            "session.manager.attach_recent.actor_attach_duration_ms",
            attach_timer.elapsed_ms(),
        );
        if let Ok(attachment) = &result {
            self.metrics.record_histogram(
                "session.manager.attach_recent.history_event_count",
                usize_to_u64(attachment.history.len()),
            );
            self.metrics.record_histogram(
                "session.manager.attach_recent.input_history_entry_count",
                usize_to_u64(attachment.input_history.len()),
            );
        }
        self.metrics.record_histogram(
            "session.manager.attach_recent.total_duration_ms",
            total_timer.elapsed_ms(),
        );
        result
    }

    /// Attach a client and return replayable history covering a projection window.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * the session does not exist
    /// * the projection request is not supported
    pub async fn attach_session_projection_window(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        request: ProjectionWindowRequest,
    ) -> Result<SessionProjectionWindowAttachment, SessionError> {
        let total_timer = self.metrics.timer();
        let handle_timer = self.metrics.timer();
        let handle = self.session_handle(session_id).await?;
        self.metrics.record_histogram(
            "session.manager.attach_projection_window.handle_duration_ms",
            handle_timer.elapsed_ms(),
        );
        let projection_timer = self.metrics.timer();
        let projection_window = match handle.projection_window(request.clone()).await {
            Ok(window) => {
                self.metrics
                    .increment_counter("session.manager.attach_projection_window.fast_path_total");
                window
            }
            Err(SessionError::UnsupportedProjectionWindow) => {
                self.metrics
                    .increment_counter("session.manager.attach_projection_window.fallback_total");
                self.projection_window_from_recent_history(session_id, request)
                    .await?
            }
            Err(error) => return Err(error),
        };
        self.metrics.record_histogram(
            "session.manager.attach_projection_window.projection_query_duration_ms",
            projection_timer.elapsed_ms(),
        );
        let history = if let Some(range) = projection_window.source_range {
            handle
                .events_range(
                    range.start_sequence,
                    range.end_sequence,
                    usize::try_from(range.end_sequence - range.start_sequence + 1)
                        .unwrap_or(usize::MAX),
                )
                .await?
        } else {
            Vec::new()
        };
        let attach_timer = self.metrics.timer();
        let mut attachment = handle
            .attach(client_id, AttachMode::ProjectionWindow { history })
            .await?;
        self.metrics.record_histogram(
            "session.manager.attach_projection_window.actor_attach_duration_ms",
            attach_timer.elapsed_ms(),
        );
        self.metrics.record_histogram(
            "session.manager.attach_projection_window.history_event_count",
            usize_to_u64(attachment.history.len()),
        );
        self.metrics.record_histogram(
            "session.manager.attach_projection_window.input_history_entry_count",
            usize_to_u64(attachment.input_history.len()),
        );
        self.metrics.record_histogram(
            "session.manager.attach_projection_window.total_duration_ms",
            total_timer.elapsed_ms(),
        );
        attachment.history.shrink_to_fit();
        Ok(SessionProjectionWindowAttachment {
            attachment,
            projection_window,
        })
    }

    /// Detach a client from a session if it is currently attached.
    ///
    /// # Errors
    ///
    /// Returns an error when the detach command cannot be delivered.
    pub async fn detach_session(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<bool, SessionError> {
        let Ok(handle) = self.session_handle(session_id).await else {
            return Ok(false);
        };
        handle.detach(client_id).await
    }
}
