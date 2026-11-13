//! What the review reaches while handling input, polling, and painting.
//!
//! [`UiContext`] pairs the [`ReviewHost`] with the state the app owns for the whole run: the
//! resolved keymap and the configured diff layout preference. Screens take it by reference; it
//! is cheap to clone into tasks.

use std::sync::Arc;

use bcode_keymap::KeymapRegistry;
use bcode_session_models::{ProjectionWindowRequest, SessionId};
use bcode_session_view_models::ReasoningPresentationPolicy;
use bmux_keyboard::KeyStroke;

use crate::appearance::AppearanceControl;
use crate::config::DiffLayout;
use crate::host::{BlockingTask, HostError, HostTask, ReviewHost, SessionFollow};

/// Host capabilities, keymap, and layout preference of a running review.
#[derive(Clone)]
pub struct UiContext {
    host: Arc<dyn ReviewHost>,
    keymap: Arc<KeymapRegistry>,
    diff_layout: Option<DiffLayout>,
    appearance: Option<Arc<AppearanceControl>>,
}

impl std::fmt::Debug for UiContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UiContext")
            .field("diff_layout", &self.diff_layout)
            .finish_non_exhaustive()
    }
}

impl UiContext {
    /// Context over `host` resolving keys with `keymap`, preferring `diff_layout` when the
    /// review's own configuration sets no layout.
    #[must_use]
    pub fn new(
        host: Arc<dyn ReviewHost>,
        keymap: Arc<KeymapRegistry>,
        diff_layout: Option<DiffLayout>,
    ) -> Self {
        Self {
            host,
            keymap,
            diff_layout,
            appearance: None,
        }
    }

    /// Let the review choose its appearance through `control` (the app owns its theme).
    #[must_use]
    pub fn with_appearance(mut self, control: Arc<AppearanceControl>) -> Self {
        self.appearance = Some(control);
        self
    }

    /// The appearance the app presents, when the app owns its theme (not when the host does).
    #[must_use]
    pub fn appearance(&self) -> Option<&AppearanceControl> {
        self.appearance.as_deref()
    }

    /// Ask the app to open its appearance picker. Returns `false` when the host owns the
    /// theme, so there is nothing to choose here.
    #[must_use]
    pub fn request_appearance_picker(&self) -> bool {
        self.appearance()
            .inspect(|control| control.request_open())
            .is_some()
    }

    /// The host.
    #[must_use]
    pub const fn host(&self) -> &Arc<dyn ReviewHost> {
        &self.host
    }

    /// The resolved keymap.
    #[must_use]
    pub fn keymap(&self) -> &KeymapRegistry {
        &self.keymap
    }

    /// Run `task` in the background; the host asks for a frame when it finishes.
    pub fn spawn(&self, task: HostTask) {
        self.host.spawn(task);
    }

    /// Run blocking `task` off the UI thread; the host asks for a frame when it finishes.
    pub fn spawn_blocking(&self, task: BlockingTask) {
        self.host.spawn_blocking(task);
    }

    /// Ask for a new frame.
    pub fn request_redraw(&self) {
        self.host.request_redraw();
    }

    /// Copy `text` to the clipboard.
    ///
    /// # Errors
    ///
    /// * [`HostError`] when no clipboard is available or copying fails.
    pub fn copy_text(&self, text: &str) -> Result<(), HostError> {
        self.host.copy_to_clipboard(text)
    }

    /// Action bound to `stroke` in `scope`.
    #[must_use]
    pub fn keymap_action(&self, scope: &str, stroke: KeyStroke) -> Option<String> {
        self.keymap.action_for_key(scope, stroke).map(str::to_owned)
    }

    /// Whether the user's configuration rebinds or unbinds `stroke` in `scope`.
    #[must_use]
    pub fn keymap_is_overridden(&self, scope: &str, stroke: KeyStroke) -> bool {
        self.keymap.is_overridden_key(scope, stroke)
    }

    /// Keys bound to `action` in `scope`, preferred first.
    #[must_use]
    pub fn keymap_keys(&self, scope: &str, action: &str) -> Vec<String> {
        self.keymap
            .keys_for_action(scope, action)
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// The configured diff layout preference, if any.
    #[must_use]
    pub const fn diff_layout(&self) -> Option<DiffLayout> {
        self.diff_layout
    }

    /// Follow a session's live view.
    ///
    /// # Errors
    ///
    /// * [`HostError`] when the host cannot follow sessions.
    pub fn follow_session(
        &self,
        session_id: SessionId,
        projection: ProjectionWindowRequest,
        reasoning_policy: ReasoningPresentationPolicy,
    ) -> Result<SessionFollow, HostError> {
        self.host
            .follow_session(session_id, projection, reasoning_policy)
    }
}
