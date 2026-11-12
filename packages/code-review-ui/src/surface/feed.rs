//! The live feed of AI cards on the surface.
//!
//! * Asking keeps the reviewer in the review: the cursor moves to the question's card, which
//!   follows the linked session live (see [`crate::rows`]); `o` still opens the session in Bcode.
//! * On a card whose session asks for a permission, the feed keys answer it (allow, always,
//!   decline, never); while its turn runs, the stop key cancels the turn. These keys are
//!   resolved in their own scope ([`FEED_SCOPE`]) before the review's, and only while they
//!   apply, so elsewhere the same keys keep their review meaning.
//! * Answers and stops are sent from the next poll; a failure shows the prompt again with a
//!   toast.
//! * Spinners advance on a timer while a session works, laying out only that session's cards.
//! * When a card needs the reviewer (a permission request, a finished answer, a failure) while
//!   it is off screen, a toast says so; the scrollbar already marks every card.

use super::ReviewSurface;
use crate::action::Action;
use crate::agent::{Attention, FeedKeys, FeedOutcome};
use crate::context::UiContext;
use crate::loader::Message;
use crate::rows::AgentAction;
use bcode_code_review::document::{ItemKey, ReviewItem};
use bmux_keyboard::KeyStroke;
use bmux_tui_components::status_bar::StatusSeverity;
use bmux_tui_components::toast_stack::ToastSeverity;
use bmux_tui_components::virtual_list::{AnchorPolicy, Edge};
use std::time::Instant;

/// Keymap scope of the keys of an AI card's live feed.
pub const FEED_SCOPE: &str = "code-review-feed";

/// A feed request waiting to be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum FeedRequest {
    /// Answer a permission request.
    Permission {
        session: String,
        permission_id: String,
        approved: bool,
        remember: bool,
    },
    /// Stop the running turn.
    Stop { session: String },
}

impl ReviewSurface {
    /// The cursor card's exchange and linked session.
    fn card_at_cursor(&self) -> Option<(String, String)> {
        let review = self.review.as_ref()?;
        let ReviewItem::AgentThread(agent) = review.doc.item(review.cursor_index)? else {
            return None;
        };
        let exchange_id = agent.exchange_id.clone()?;
        let session = self.notes.exchange(&exchange_id)?.session_id.clone()?;
        Some((exchange_id, session))
    }

    /// Footer hints of the cursor card while its feed keys apply: the permission answers, or
    /// stop; empty otherwise.
    pub(super) fn feed_hints(&self) -> Vec<(String, String)> {
        let Some(state) = self
            .card_at_cursor()
            .and_then(|(_, session)| self.agents.state(&session))
        else {
            return Vec::new();
        };
        let keys = self.agents.keys();
        let hints: Vec<(&str, &str)> = match &state.feed.permission {
            Some(permission) if !self.agents.is_answering(&permission.permission_id) => {
                let mut hints = vec![(keys.allow.as_str(), "allow")];
                if permission.can_remember {
                    hints.push((&keys.always, "always"));
                }
                hints.push((&keys.decline, "decline"));
                if permission.can_remember {
                    hints.push((&keys.never, "never"));
                }
                hints.push((&keys.open, "open in Bcode"));
                hints
            }
            _ if state.working && !state.cancelling => {
                vec![(keys.stop.as_str(), "stop"), (&keys.open, "open in Bcode")]
            }
            _ => Vec::new(),
        };
        hints
            .into_iter()
            .filter(|(key, _)| !key.is_empty())
            .map(|(key, label)| (key.to_owned(), label.to_owned()))
            .collect()
    }

    /// Read the feed keys from the keymap for the cards' buttons: the first declared default
    /// still bound, else the first bound key.
    pub(super) fn prepare_feed_keys(&mut self, host: &UiContext) {
        let declared = crate::keymap::actions();
        let first = |scope: &str, action: &str| {
            let id = format!("bcode.code_review.{action}");
            let bound = host.keymap_keys(scope, &id);
            declared
                .iter()
                .find(|declared| declared.id == id && declared.scope == scope)
                .and_then(|declared| {
                    declared
                        .defaults
                        .iter()
                        .find(|key| bound.contains(key))
                        .cloned()
                })
                .or_else(|| bound.into_iter().next())
                .unwrap_or_default()
        };
        self.agents.set_keys(FeedKeys {
            allow: first(FEED_SCOPE, "feed_allow"),
            always: first(FEED_SCOPE, "feed_always"),
            decline: first(FEED_SCOPE, "feed_decline"),
            never: first(FEED_SCOPE, "feed_never"),
            stop: first(FEED_SCOPE, "feed_stop"),
            open: first(super::SCOPE, "open_session"),
        });
    }

    /// A feed key on the cursor's card, when it applies there; `None` lets the review handle
    /// the key.
    pub(super) fn feed_key(&mut self, stroke: KeyStroke, host: &UiContext) -> Option<Action> {
        let action = host.keymap_action(FEED_SCOPE, stroke)?;
        let action = action.strip_prefix("bcode.code_review.").unwrap_or(&action);
        let (_, session) = self.card_at_cursor()?;
        let state = self.agents.state(&session)?;
        let pending = state
            .feed
            .permission
            .as_ref()
            .is_some_and(|permission| !self.agents.is_answering(&permission.permission_id));
        let action = match action {
            "feed_allow" if pending => AgentAction::Allow,
            "feed_always" if pending => AgentAction::Always,
            "feed_decline" if pending => AgentAction::Decline,
            "feed_never" if pending => AgentAction::Never,
            "feed_stop" if state.working && !state.cancelling => AgentAction::Stop,
            _ => return None,
        };
        self.feed_action(action);
        Some(Action::Redraw)
    }

    /// Answer the permission request of the cursor card's session, stop its turn, or open or
    /// close one of its feed items.
    pub(super) fn feed_action(&mut self, action: AgentAction) {
        let Some((exchange_id, session)) = self.card_at_cursor() else {
            self.set_status(
                "this question has no linked session yet",
                StatusSeverity::Warning,
            );
            return;
        };
        let (approved, remember) = match action {
            AgentAction::Allow => (true, false),
            AgentAction::Always => (true, true),
            AgentAction::Decline => (false, false),
            AgentAction::Never => (false, true),
            AgentAction::Stop => {
                self.feed_requests.push_back(FeedRequest::Stop { session });
                self.set_status("stopping the Bcode turn…", StatusSeverity::Info);
                return;
            }
            AgentAction::Toggle(index) => {
                self.toggle_feed_item(&exchange_id, &session, usize::from(index));
                return;
            }
            _ => return,
        };
        let Some(permission) = self
            .agents
            .state(&session)
            .and_then(|state| state.feed.permission.clone())
        else {
            self.set_status("no permission request is pending", StatusSeverity::Info);
            return;
        };
        if self.agents.is_answering(&permission.permission_id) {
            return;
        }
        self.agents
            .set_answering(&session, &permission.permission_id, true);
        self.feed_requests.push_back(FeedRequest::Permission {
            session,
            permission_id: permission.permission_id,
            approved,
            remember: remember && permission.can_remember,
        });
        let verb = match (approved, remember) {
            (true, false) => "allowed",
            (true, true) => "always allowed",
            (false, false) => "declined",
            (false, true) => "never allowed",
        };
        self.set_status(format!("{verb} {}", permission.tool), StatusSeverity::Info);
    }

    /// Open or close the reasoning or tool output at `index` of the card's turn.
    fn toggle_feed_item(&mut self, exchange_id: &str, session: &str, index: usize) {
        let Some(exchange) = self.notes.exchange(exchange_id) else {
            return;
        };
        let newest = crate::rows::is_newest(exchange, &self.notes);
        let Some(id) = self
            .agents
            .state(session)
            .and_then(|state| crate::rows::exchange_turn(exchange, state, newest))
            .and_then(|(turn, _)| turn.items.get(index))
            .and_then(crate::rows::item_id)
            .map(|id| crate::rows::open_key(exchange_id, id))
        else {
            return;
        };
        if !self.folds.feed_open.remove(&id) {
            self.folds.feed_open.insert(id);
        }
        self.folds.revision = self.folds.revision.wrapping_add(1);
    }

    /// Send queued feed requests.
    pub(super) fn pump_feed(&mut self, host: &UiContext) {
        while let Some(request) = self.feed_requests.pop_front() {
            let host_ref = std::sync::Arc::clone(host.host());
            let inbox = self.inbox.clone();
            host.spawn(Box::pin(async move {
                let outcome = match request {
                    FeedRequest::Permission {
                        session,
                        permission_id,
                        approved,
                        remember,
                    } => FeedOutcome {
                        error: host_ref
                            .resolve_permission(permission_id.clone(), approved, remember)
                            .await
                            .err()
                            .map(|error| error.message),
                        session,
                        permission_id: Some(permission_id),
                    },
                    FeedRequest::Stop { session } => FeedOutcome {
                        error: host_ref
                            .cancel_turn(session.clone())
                            .await
                            .err()
                            .map(|error| error.message),
                        session,
                        permission_id: None,
                    },
                };
                inbox.push(Message::Feed(outcome));
            }));
        }
    }

    /// A sent feed request finished; a failed permission answer shows its prompt again.
    pub(super) fn finish_feed(&mut self, outcome: FeedOutcome) {
        let Some(error) = outcome.error else {
            return;
        };
        let title = if let Some(permission_id) = &outcome.permission_id {
            self.agents
                .set_answering(&outcome.session, permission_id, false);
            "Could not answer the permission request"
        } else {
            "Could not stop the Bcode turn"
        };
        self.toasts.push(ToastSeverity::Error, title, Some(error));
    }

    /// Advance spinners and ask for a frame when the next one is due. Returns whether a card
    /// changed.
    pub(super) fn tick_feed(&mut self, host: &UiContext) -> bool {
        let before = self.agents.revision();
        let now = Instant::now();
        if let Some(delay) = self
            .agents
            .tick(now, bmux_tui_components::spinner::DEFAULT_INTERVAL)
        {
            let due = now + delay;
            if self.feed_wake.is_none_or(|wake| wake <= now || due < wake) {
                self.feed_wake = Some(due);
                host.host().request_redraw_after(delay);
            }
        }
        self.agents.revision() != before
    }

    /// Toast attention changes of sessions whose cards are all off screen.
    pub(super) fn feed_attention(&mut self) {
        for (session, attention) in self.agents.take_attention() {
            let cards: Vec<(String, String)> = self
                .notes
                .exchanges()
                .iter()
                .filter(|exchange| exchange.session_id.as_deref() == Some(session.as_str()))
                .map(|exchange| {
                    (
                        exchange.exchange_id.clone(),
                        super::annotate::location_label(&exchange.anchor),
                    )
                })
                .collect();
            let Some(review) = self.review.as_ref() else {
                continue;
            };
            let (first, last) = review.visible;
            let visible = cards.iter().any(|(id, _)| {
                review
                    .doc
                    .index_of(&ItemKey::AgentThread { id: id.clone() })
                    .is_some_and(|index| (first..=last).contains(&index))
            });
            let Some((_, location)) = cards.last().filter(|_| !visible) else {
                continue;
            };
            let (severity, title) = match &attention {
                Attention::Permission(tool) => (
                    ToastSeverity::Warning,
                    format!("Bcode needs your permission to use {tool}"),
                ),
                Attention::Answered => (ToastSeverity::Success, "Bcode answered".to_owned()),
                Attention::Failed(_) => (ToastSeverity::Error, "Bcode failed".to_owned()),
            };
            let body = match attention {
                Attention::Failed(message) => format!("{location}: {message}"),
                _ => location.clone(),
            };
            self.toasts.push(severity, title, Some(body));
        }
    }

    /// Put the cursor on the card of `exchange_id`, keeping the viewport when the card is
    /// already visible.
    pub(super) fn focus_card(&mut self, exchange_id: &str) {
        let Some(review) = self.review.as_mut() else {
            return;
        };
        let key = ItemKey::AgentThread {
            id: exchange_id.to_owned(),
        };
        if let Some(index) = review.doc.index_of(&key) {
            review.set_cursor(index, AnchorPolicy::EnsureVisibleFrom(Edge::Bottom));
        }
    }
}
