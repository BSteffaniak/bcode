//! Generic, revision-fenced composer interaction plumbing. Contributors own activation and order.

use std::path::PathBuf;

use bcode_plugin_sdk::composer::{
    ComposerActivation, ComposerContinuation, ComposerDocument, ComposerEdit,
    ComposerSearchRequest, ComposerSnapshot, ComposerSnapshotState, ComposerTextRange,
    MAX_COMPOSER_CANDIDATES, PluginOwnedComposerContribution,
};
use bmux_text_edit::TextEditBuffer;
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui_components::selectable_list::{
    SelectableListItem, SelectableListOutcome, SelectableListState,
};

/// Process-local scope. A registry replacement, workspace switch or session switch invalidates work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerScope {
    pub session: Option<bcode_session_models::SessionId>,
    pub agent_id: Option<String>,
    pub workspace: Option<PathBuf>,
    pub registry_identity: u64,
    pub registry_revision: u64,
}

#[derive(Debug)]
pub struct ComposerCompletion {
    pub request_id: String,
    pub snapshot: Option<ComposerSnapshot>,
}

#[derive(Default)]
enum SearchStatus {
    #[default]
    Idle,
    Loading,
    Empty,
    Error,
    Ready,
    Incomplete,
}

pub struct ComposerInteraction {
    focused_field: Option<bcode_plugin_sdk::tui::FocusedTextFieldSnapshot>,
    scope: Option<ComposerScope>,
    document: Option<ComposerDocument>,
    selection: Option<ComposerTextRange>,
    request: Option<ComposerSearchRequest>,
    route: Option<PluginOwnedComposerContribution>,
    reactivate_route: Option<PluginOwnedComposerContribution>,
    snapshot: Option<ComposerSnapshot>,
    task: Option<tokio::task::AbortHandle>,
    client: Option<bcode_client::BcodeClient>,
    dismissed: bool,
    status: SearchStatus,
    pub completion_control: bmux_tui_components::completion::CompletionState,
    pub list: SelectableListState,
    pub items: Vec<SelectableListItem>,
    /// Geometry from the latest paint, never a separately guessed hit rectangle.
    pub painted_list: Option<Rect>,
    pub painted_layout: Option<bmux_tui::component::LayoutNode>,
}

impl Default for ComposerInteraction {
    fn default() -> Self {
        Self {
            focused_field: None,
            scope: None,
            document: None,
            selection: None,
            request: None,
            route: None,
            reactivate_route: None,
            snapshot: None,
            task: None,
            client: None,
            dismissed: false,
            status: SearchStatus::default(),
            completion_control: bmux_tui_components::completion::CompletionState::new(),
            list: SelectableListState::new(None),
            items: Vec::new(),
            painted_list: None,
            painted_layout: None,
        }
    }
}

impl Drop for ComposerInteraction {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl ComposerInteraction {
    /// Observe text, cursor AND selection before routing input or admitting a reply.
    /// Returning true requests fresh activation; equal observations never repeat searches.
    pub fn observe(&mut self, scope: ComposerScope, buffer: &TextEditBuffer) -> bool {
        let selection = buffer.selection().map(|range| ComposerTextRange {
            start: range.start,
            end: range.end,
        });
        let new_scope = self.scope.as_ref() != Some(&scope);
        self.focused_field = None;
        let changed = new_scope
            || self.selection != selection
            || self.document.as_ref().is_none_or(|document| {
                document.text != buffer.text() || document.cursor != buffer.cursor_byte_index()
            });
        if !changed {
            return false;
        }
        let reactivate_route = if new_scope {
            None
        } else {
            self.reactivate_route.take()
        };
        self.cancel();
        self.reactivate_route = reactivate_route;
        let (document_id, revision) = if new_scope {
            (uuid::Uuid::new_v4().to_string(), 0)
        } else if let Some(document) = &self.document {
            document.revision.checked_add(1).map_or_else(
                || (uuid::Uuid::new_v4().to_string(), 0),
                |revision| (document.document_id.clone(), revision),
            )
        } else {
            (uuid::Uuid::new_v4().to_string(), 0)
        };
        self.document = Some(ComposerDocument {
            document_id,
            revision,
            text: buffer.text().to_owned(),
            cursor: buffer.cursor_byte_index(),
            selection,
        });
        self.selection = selection;
        self.scope = Some(scope);
        self.dismissed = false;
        true
    }

    pub fn document(&self) -> Option<&ComposerDocument> {
        self.document
            .as_ref()
            .filter(|document| document.validate().is_ok())
    }

    /// Observe the opted-in active owner's field, retaining its exact stale-edit fences.
    pub fn observe_focused_field(
        &mut self,
        scope: ComposerScope,
        field: bcode_plugin_sdk::tui::FocusedTextFieldSnapshot,
    ) -> bool {
        if !field.eligible
            || field.document.validate().is_err()
            || field.content_rect.is_empty()
            || field.anchor.is_empty()
        {
            self.cancel();
            self.focused_field = None;
            self.document = None;
            self.dismissed = true;
            return true;
        }
        let changed = self.scope.as_ref() != Some(&scope)
            || self.focused_field.as_ref().is_none_or(|previous| {
                previous.identity != field.identity
                    || previous.generation != field.generation
                    || previous.document != field.document
                    || previous.eligible != field.eligible
            });
        if changed {
            let same_owner = self.scope.as_ref() == Some(&scope)
                && self
                    .focused_field
                    .as_ref()
                    .is_some_and(|previous| previous.identity == field.identity);
            let finished = self.scope.as_ref() == Some(&scope)
                && (same_owner || self.focused_field.is_none())
                && self.dismissed
                && self.document.as_ref().is_some_and(|document| {
                    document.text == field.document.text
                        && document.cursor == field.document.cursor
                        && document.selection == field.document.selection
                });
            let reactivation = same_owner.then(|| self.reactivate_route.take()).flatten();
            self.cancel();
            self.reactivate_route = reactivation;
            self.document = Some(field.document.clone());
            self.selection = field.document.selection;
            self.scope = Some(scope);
            self.dismissed = finished;
        }
        self.focused_field = Some(field);
        changed
    }

    pub const fn focused_field(&self) -> Option<&bcode_plugin_sdk::tui::FocusedTextFieldSnapshot> {
        self.focused_field.as_ref()
    }

    pub fn focused_edit(&self) -> Option<bcode_plugin_sdk::tui::FocusedTextFieldEdit> {
        Some(bcode_plugin_sdk::tui::FocusedTextFieldEdit {
            expected: self.focused_field.clone()?,
            edit: self.selected_edit()?,
        })
    }

    /// Settle an owner-applied edit; semantic synchronization belongs to the owner.
    pub fn finish_focused_edit(&mut self, result: ComposerDocument) {
        let continuation = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| {
                snapshot
                    .candidates
                    .get(self.list.focused().or_else(|| self.list.selected())?)
            })
            .map_or(ComposerContinuation::Finish, |candidate| {
                candidate.continuation
            });
        let route = self.route.clone();
        self.cancel();
        if continuation == ComposerContinuation::Finish {
            self.document = Some(result);
            self.dismissed = true;
        } else {
            self.reactivate_route = route;
        }
    }

    pub fn start(
        &mut self,
        route: PluginOwnedComposerContribution,
        activation: ComposerActivation,
    ) -> Option<ComposerSearchRequest> {
        if self.dismissed || route.contribution.id.trim().is_empty() {
            return None;
        }
        let request = ComposerSearchRequest {
            contribution_id: route.contribution.id.clone(),
            request_id: uuid::Uuid::new_v4().to_string(),
            document: self.document()?.clone(),
            activation,
            max_results: MAX_COMPOSER_CANDIDATES,
        };
        request.validate().ok()?;
        self.cancel();
        self.route = Some(route);
        self.request = Some(request.clone());
        self.status = SearchStatus::Loading;
        self.completion_control.activate();
        Some(request)
    }

    pub fn retain_task(
        &mut self,
        task: tokio::task::AbortHandle,
        client: bcode_client::BcodeClient,
    ) {
        self.task = Some(task);
        self.client = Some(client);
    }

    /// Cancellation is best effort; request identity is the authoritative late-reply fence.
    pub fn cancel(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            if let (Some(client), Some(request)) = (self.client.take(), self.request.as_ref()) {
                let invocation_id = request.request_id.clone();
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(async move {
                        let _ = client.cancel_plugin_service_invocation(invocation_id).await;
                    });
                }
            }
        }
        self.client = None;
        self.request = None;
        self.route = None;
        self.reactivate_route = None;
        self.snapshot = None;
        self.status = SearchStatus::Idle;
        self.items.clear();
        self.completion_control.dismiss();
        self.painted_list = None;
        self.painted_layout = None;
        self.list = SelectableListState::new(None);
    }

    pub fn dismiss(&mut self) {
        self.cancel();
        self.dismissed = true;
    }

    pub fn complete(&mut self, completion: ComposerCompletion) -> bool {
        let Some(request) = &self.request else {
            return false;
        };
        if completion.request_id != request.request_id
            || self.document.as_ref() != Some(&request.document)
            || self.dismissed
        {
            return false;
        }
        self.task = None;
        self.client = None;
        let Some(snapshot) = completion
            .snapshot
            .filter(|snapshot| snapshot.validate_for(request).is_ok())
        else {
            self.status = SearchStatus::Error;
            return true;
        };
        self.status = match snapshot.effective_state() {
            ComposerSnapshotState::Loading => SearchStatus::Loading,
            ComposerSnapshotState::Error { .. } => SearchStatus::Error,
            ComposerSnapshotState::Truncated => SearchStatus::Incomplete,
            ComposerSnapshotState::Ready if snapshot.candidates.is_empty() => SearchStatus::Empty,
            ComposerSnapshotState::Ready => SearchStatus::Ready,
        };
        self.items = snapshot
            .candidates
            .iter()
            .map(|candidate| {
                // Presentation only: no path parsing, trigger syntax, matching or ranking here.
                let label = candidate.detail.as_ref().map_or_else(
                    || candidate.label.clone(),
                    |detail| format!("{}  {detail}", candidate.label),
                );
                SelectableListItem::new(&candidate.id, label)
            })
            .collect();
        // BMUX reconciles candidate identity, focus and pointer state.
        self.completion_control.reconcile(&self.items);
        self.list = self.completion_control.list_state().get();
        self.snapshot = Some(snapshot);
        true
    }

    #[must_use]
    pub const fn visible(&self) -> bool {
        !matches!(self.status, SearchStatus::Idle)
    }

    #[must_use]
    pub const fn status(&self) -> &'static str {
        match self.status {
            SearchStatus::Idle | SearchStatus::Ready => " Suggestions ",
            SearchStatus::Loading => " Loading suggestions… ",
            SearchStatus::Empty => " No suggestions ",
            SearchStatus::Error => " Suggestions unavailable ",
            SearchStatus::Incomplete => " Suggestions · more results omitted ",
        }
    }

    /// BMUX owns nested interaction arbitration, including loading/error/no-op consumption.
    pub fn command(
        &mut self,
        command: bmux_tui_components::completion::CompletionCommand,
    ) -> (bmux_tui::event::EventOutcome, bool) {
        use bmux_tui_components::completion::{CompletionInteraction, CompletionStatus};
        let status = match self.status {
            SearchStatus::Ready | SearchStatus::Incomplete => CompletionStatus::Candidates,
            SearchStatus::Loading => CompletionStatus::Loading,
            SearchStatus::Empty | SearchStatus::Idle => CompletionStatus::Empty,
            SearchStatus::Error => CompletionStatus::Error,
        };
        let mut accept = false;
        let outcome = CompletionInteraction::new(&self.items, status).handle_command(
            command,
            &mut self.completion_control,
            |_| {
                accept = true;
                false
            },
        );
        self.completion_control.reconcile(&self.items);
        if matches!(
            command,
            bmux_tui_components::completion::CompletionCommand::Next
                | bmux_tui_components::completion::CompletionCommand::Previous
        ) && let Some(layout) = &self.painted_layout
        {
            bmux_tui_components::selectable_list::SelectableListComponent::new(
                layout.id.clone(),
                &self.items,
                self.completion_control.list_state(),
            )
            .reveal_focus(layout);
        }
        self.list = self.completion_control.list_state().get();
        (outcome, accept)
    }

    pub fn selected_edit(&self) -> Option<ComposerEdit> {
        let index = self.list.focused().or_else(|| self.list.selected())?;
        self.snapshot
            .as_ref()?
            .candidates
            .get(index)
            .map(|candidate| candidate.edit.clone())
    }

    pub const fn take_reactivation(&mut self) -> Option<PluginOwnedComposerContribution> {
        self.reactivate_route.take()
    }

    #[must_use]
    pub const fn needs_activation(&self) -> bool {
        !self.dismissed && self.request.is_none()
    }

    pub fn list_event(&mut self, event: &Event) -> SelectableListOutcome {
        use bmux_tui_components::completion::{CompletionInteraction, CompletionStatus};
        let Some(area) = self.painted_list.filter(|area| !area.is_empty()) else {
            return SelectableListOutcome::Ignored;
        };
        let Some(layout) = &self.painted_layout else {
            return SelectableListOutcome::Ignored;
        };
        let status = match self.status {
            SearchStatus::Ready | SearchStatus::Incomplete => CompletionStatus::Candidates,
            SearchStatus::Loading => CompletionStatus::Loading,
            SearchStatus::Error => CompletionStatus::Error,
            SearchStatus::Empty | SearchStatus::Idle => CompletionStatus::Empty,
        };
        let mut accepted = None;
        let mut cx = bmux_tui::component::EventCx::with_clip(layout, area);
        let outcome = cx.with_transform(0, 0, i32::from(area.x), i64::from(area.y), area, |cx| {
            CompletionInteraction::new(&self.items, status).handle_pointer(
                event,
                layout,
                cx,
                &mut self.completion_control,
                |id| {
                    accepted = self.items.iter().position(|item| item.id == id);
                    false
                },
            )
        });
        self.list = self.completion_control.list_state().get();
        accepted.map_or_else(
            || {
                if outcome == bmux_tui::event::EventOutcome::Ignored {
                    SelectableListOutcome::Ignored
                } else {
                    SelectableListOutcome::Redraw
                }
            },
            SelectableListOutcome::Selected,
        )
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use bcode_plugin_sdk::composer::{
        COMPOSER_INTERFACE_ID, ComposerCandidate, ComposerContribution,
    };

    // Transaction fixture for legacy contributor-state tests; production acceptance
    // routes through the owner adapter and its explicit edit result.
    fn apply_fixture(state: &mut ComposerInteraction, buffer: &mut TextEditBuffer) -> bool {
        let Some(document) = state.document.as_ref() else {
            return false;
        };
        let selection = buffer.selection().map(|range| ComposerTextRange {
            start: range.start,
            end: range.end,
        });
        if buffer.text() != document.text
            || buffer.cursor_byte_index() != document.cursor
            || selection != state.selection
        {
            state.cancel();
            return false;
        }
        let Some(edit) = state.selected_edit() else {
            return false;
        };
        let Ok(result) = edit.apply(document) else {
            return false;
        };
        let continuation = state
            .snapshot
            .as_ref()
            .and_then(|snapshot| {
                snapshot
                    .candidates
                    .get(state.list.focused().or_else(|| state.list.selected())?)
            })
            .map_or(ComposerContinuation::Finish, |candidate| {
                candidate.continuation
            });
        // The live BMUX editor validates positions and records one transaction,
        // preserving its pre-edit cursor/selection and existing undo/redo history.
        let expected = buffer.edit_state();
        if buffer.replace_byte_range(
            &expected,
            edit.range.start..edit.range.end,
            &edit.replacement,
            result.cursor,
            None,
        ) != Ok(true)
        {
            return false;
        }
        let route = state.route.clone();
        state.cancel();
        if continuation == ComposerContinuation::Finish {
            state.document = Some(result);
            state.selection = None;
            state.dismissed = true;
        } else {
            state.reactivate_route = route;
        }
        true
    }

    pub fn scope() -> ComposerScope {
        ComposerScope {
            session: None,
            agent_id: Some("build".into()),
            workspace: Some(PathBuf::from("/workspace")),
            registry_identity: 1,
            registry_revision: 0,
        }
    }

    struct Topics;
    impl bcode_plugin_sdk::composer::PluginComposerContributor for Topics {
        fn kind(&self) -> &'static str {
            "example.topic"
        }
        fn activate(&self, document: &ComposerDocument) -> Option<ComposerActivation> {
            document.allows_activation().then(|| ComposerActivation {
                range: ComposerTextRange {
                    start: 0,
                    end: document.text.len(),
                },
                query: document.text.clone(),
            })
        }
    }

    pub fn begin(
        state: &mut ComposerInteraction,
        buffer: &TextEditBuffer,
    ) -> ComposerSearchRequest {
        state.observe(scope(), buffer);
        begin_observed(state)
    }

    pub fn begin_observed(state: &mut ComposerInteraction) -> ComposerSearchRequest {
        state.dismissed = false; // Fixture explicitly starts a new admitted search.
        let mut registry = bcode_plugin_sdk::composer::PluginComposerRegistry::default();
        registry.register(Box::new(Topics)).unwrap();
        let activation = registry
            .activate("example.topic", state.document().unwrap())
            .unwrap()
            .unwrap();
        state
            .start(
                PluginOwnedComposerContribution {
                    plugin_id: "example.topics".into(),
                    contribution: ComposerContribution {
                        id: "topics".into(),
                        activation_kind: "example.topic".into(),
                        service_interface_id: COMPOSER_INTERFACE_ID.into(),
                        priority: 0,
                    },
                },
                activation,
            )
            .unwrap()
    }

    pub fn completion(request: &ComposerSearchRequest) -> ComposerCompletion {
        ComposerCompletion {
            request_id: request.request_id.clone(),
            snapshot: Some(ComposerSnapshot {
                request_id: request.request_id.clone(),
                document_id: request.document.document_id.clone(),
                revision: request.document.revision,
                state: ComposerSnapshotState::Ready,
                candidates: vec![ComposerCandidate {
                    id: "topic".into(),
                    label: "主题 👩‍💻".into(),
                    detail: None,
                    continuation: ComposerContinuation::Finish,
                    edit: ComposerEdit {
                        document_id: request.document.document_id.clone(),
                        revision: request.document.revision,
                        range: request.activation.range,
                        expected_text: request.document.text.clone(),
                        replacement: "主题 👩‍💻".into(),
                        cursor: "主题 👩‍💻".len(),
                    },
                }],
                incomplete: false,
            }),
        }
    }

    #[test]
    fn focused_owner_generation_fences_identical_text_and_geometry_does_not_restart() {
        use bcode_plugin_sdk::tui::{FocusedTextFieldIdentity, FocusedTextFieldSnapshot};
        let mut state = ComposerInteraction::default();
        let field = FocusedTextFieldSnapshot {
            identity: FocusedTextFieldIdentity {
                owner_id: "owner".into(),
                field_id: "answer".into(),
                lifetime: 1,
            },
            generation: 1,
            document: ComposerDocument {
                document_id: "answer".into(),
                revision: 0,
                text: "@topic".into(),
                cursor: 6,
                selection: None,
            },
            eligible: true,
            content_rect: Rect::new(1, 1, 20, 1),
            anchor: Rect::new(7, 1, 1, 1),
        };
        assert!(state.observe_focused_field(scope(), field.clone()));
        let request = begin_observed(&mut state);
        let mut moved = field;
        moved.anchor.x += 1;
        assert!(!state.observe_focused_field(scope(), moved.clone()));
        assert!(state.complete(completion(&request)));
        moved.generation += 1;
        assert!(state.observe_focused_field(scope(), moved.clone()));
        assert!(!state.complete(completion(&request)));
        let request = begin_observed(&mut state);
        moved.identity.field_id = "other".into();
        assert!(state.observe_focused_field(scope(), moved.clone()));
        assert!(!state.complete(completion(&request)));
        moved.eligible = false;
        assert!(state.observe_focused_field(scope(), moved));
        assert!(!state.visible());
        assert!(state.document().is_none());
    }

    #[test]
    fn unpainted_observation_retains_dismissal_until_an_ordinary_edit() {
        let mut state = ComposerInteraction::default();
        let mut buffer = TextEditBuffer::from_text("@topic");
        state.observe(scope(), &buffer);
        state.dismiss();
        assert!(!state.observe(scope(), &buffer));
        assert!(!state.needs_activation());
        buffer.insert_str("x");
        assert!(state.observe(scope(), &buffer));
        assert_eq!(state.document().unwrap().text, "@topicx");
        assert!(state.needs_activation());
    }

    #[test]
    fn loading_commands_consume_without_presentation_or_candidate_effect() {
        use bmux_tui_components::completion::CompletionCommand;
        let mut state = ComposerInteraction::default();
        let buffer = TextEditBuffer::from_text("@topic");
        begin(&mut state, &buffer);
        for command in [
            CompletionCommand::Accept,
            CompletionCommand::Next,
            CompletionCommand::Previous,
        ] {
            let (outcome, accept) = state.command(command);
            assert_eq!(outcome, bmux_tui::event::EventOutcome::Handled);
            assert!(!accept);
        }
    }

    #[test]
    fn generic_edit_preserves_atomic_stale_safety() {
        let mut state = ComposerInteraction::default();
        let mut buffer = TextEditBuffer::from_text("#topic");
        let request = begin(&mut state, &buffer);
        assert!(state.complete(completion(&request)));
        assert!(apply_fixture(&mut state, &mut buffer));
        assert_eq!(buffer.text(), "主题 👩‍💻");
        assert_eq!(buffer.cursor_byte_index(), buffer.text().len());
        buffer.insert_str("x");
        let request = begin(&mut state, &buffer);
        state.complete(completion(&request));
        buffer.insert_str("!");
        let before = buffer.clone();
        assert!(!apply_fixture(&mut state, &mut buffer));
        assert_eq!(buffer.text(), before.text());
    }

    #[test]
    fn continuation_is_explicit_and_keeps_the_original_route() {
        let mut state = ComposerInteraction::default();
        let mut buffer = TextEditBuffer::from_text("#topic");
        let request = begin(&mut state, &buffer);
        assert_eq!(request.contribution_id, "topics");
        let mut reply = completion(&request);
        reply.snapshot.as_mut().unwrap().candidates[0].continuation =
            ComposerContinuation::Reactivate;
        state.complete(reply);
        assert!(apply_fixture(&mut state, &mut buffer));
        state.observe(scope(), &buffer);
        let route = state.take_reactivation().unwrap();
        assert_eq!(route.plugin_id, "example.topics");
        assert_eq!(route.contribution.id, "topics");
        let document = state.document().unwrap();
        let activation =
            bcode_plugin_sdk::composer::PluginComposerContributor::activate(&Topics, document)
                .unwrap();
        let next = state.start(route, activation).unwrap();
        assert_eq!(next.contribution_id, request.contribution_id);
        assert_eq!(next.contribution_id, "topics");
        assert_ne!(next.request_id, request.request_id);
        let mut finished = completion(&next);
        let edit = &mut finished.snapshot.as_mut().unwrap().candidates[0].edit;
        edit.replacement = "主题 finished 👩‍💻".into();
        edit.cursor = edit.replacement.len();
        state.complete(finished);
        assert!(apply_fixture(&mut state, &mut buffer));
        assert_eq!(buffer.text(), "主题 finished 👩‍💻");
        assert!(!state.observe(scope(), &buffer));
        assert!(!state.needs_activation());
    }

    #[test]
    fn rejected_and_noop_transactions_preserve_redo_history() {
        for invalid in [false, true] {
            let mut state = ComposerInteraction::default();
            let mut buffer = TextEditBuffer::from_text("#topic");
            buffer.insert_str("!");
            let future = buffer.edit_state();
            assert!(buffer.undo());
            let before = buffer.edit_state();
            let request = begin(&mut state, &buffer);
            let mut reply = completion(&request);
            let edit = &mut reply.snapshot.as_mut().unwrap().candidates[0].edit;
            if invalid {
                // Valid UTF-8 byte offset inside the ZWJ grapheme: BMUX must reject.
                edit.cursor = "主题 👩".len();
            } else {
                edit.replacement = request.document.text.clone();
                edit.cursor = request.document.cursor;
            }
            assert!(state.complete(reply));
            assert!(!apply_fixture(&mut state, &mut buffer));
            assert_eq!(buffer.edit_state(), before);
            assert!(!buffer.can_undo());
            assert!(buffer.can_redo());
            assert!(buffer.redo());
            assert_eq!(buffer.edit_state(), future);
        }
    }

    #[test]
    fn invalid_utf8_is_rejected_and_bmux_byte_cursor_is_preserved() {
        let mut state = ComposerInteraction::default();
        let mut buffer = TextEditBuffer::from_text("#topic");
        let request = begin(&mut state, &buffer);
        let mut reply = completion(&request);
        reply.snapshot.as_mut().unwrap().candidates[0].edit.cursor = 1;
        assert!(state.complete(reply));
        assert!(!apply_fixture(&mut state, &mut buffer));
        assert_eq!(buffer.text(), "#topic");
        let request = begin(&mut state, &buffer);
        let mut reply = completion(&request);
        let edit = &mut reply.snapshot.as_mut().unwrap().candidates[0].edit;
        edit.replacement = "a\u{301}".into();
        edit.cursor = 1;
        // The transactional API requires resulting grapheme boundaries, even
        // where legacy Absolute motions accepted a UTF-8 boundary inside one.
        let before = buffer.edit_state();
        assert!(state.complete(reply));
        assert!(!apply_fixture(&mut state, &mut buffer));
        assert_eq!(buffer.edit_state(), before);
    }

    #[test]
    fn distinct_bounded_search_states_do_not_offer_invalid_edits() {
        let mut state = ComposerInteraction::default();
        let buffer = TextEditBuffer::from_text("#topic");
        let request = begin(&mut state, &buffer);
        assert!(state.status().contains("Loading"));
        let mut reply = completion(&request);
        reply.snapshot.as_mut().unwrap().candidates.clear();
        state.complete(reply);
        assert!(state.status().contains("No suggestions"));
        let request = begin(&mut state, &buffer);
        let mut reply = completion(&request);
        reply.snapshot.as_mut().unwrap().state = ComposerSnapshotState::Truncated;
        state.complete(reply);
        assert!(state.status().contains("omitted"));
        let request = begin(&mut state, &buffer);
        state.complete(ComposerCompletion {
            request_id: request.request_id,
            snapshot: None,
        });
        assert!(state.status().contains("unavailable"));
        assert!(state.selected_edit().is_none());
    }

    #[test]
    fn dismissal_selection_session_workspace_and_unload_fence_replies() {
        let mut state = ComposerInteraction::default();
        let mut buffer = TextEditBuffer::from_text("#topic");
        let request = begin(&mut state, &buffer);
        state.dismiss();
        assert!(!state.complete(completion(&request)));
        assert!(!state.observe(scope(), &buffer));
        assert!(
            state
                .start(
                    PluginOwnedComposerContribution {
                        plugin_id: "disabled".into(),
                        contribution: ComposerContribution {
                            id: "x".into(),
                            activation_kind: "x".into(),
                            service_interface_id: COMPOSER_INTERFACE_ID.into(),
                            priority: 0
                        }
                    },
                    request.activation
                )
                .is_none()
        );
        buffer.select_all();
        assert!(state.observe(scope(), &buffer));
        buffer.clear_selection();
        let request = begin(&mut state, &buffer);
        let mut changed = scope();
        changed.registry_revision += 1;
        state.observe(changed, &buffer);
        assert!(!state.complete(completion(&request)));
        let request = begin(&mut state, &buffer);
        let mut changed = scope();
        changed.workspace = Some(PathBuf::from("/other"));
        state.observe(changed, &buffer);
        assert!(!state.complete(completion(&request)));
        let request = begin(&mut state, &buffer);
        let mut changed = scope();
        changed.agent_id = Some("review".into());
        state.observe(changed, &buffer);
        assert!(!state.complete(completion(&request)));
        let request = begin(&mut state, &buffer);
        let mut changed = scope();
        changed.session = Some(bcode_session_models::SessionId(uuid::Uuid::new_v4()));
        state.observe(changed, &buffer);
        assert!(!state.complete(completion(&request)));
    }
}
