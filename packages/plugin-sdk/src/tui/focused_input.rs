//! Opt-in access to a focused, owner-managed text field at the terminal boundary.

use bcode_plugin_models::composer::{
    ComposerDocument, ComposerEdit, ComposerTextRange, ComposerValidationError,
    MAX_COMPOSER_DOCUMENT_BYTES,
};
use bmux_text_edit::{TextEditBuffer, TextEditState, TextEditTransactionError};
use bmux_tui::geometry::Rect;
use bmux_tui_components::text_input::TextInputState;

/// Semantic edit outcome, independent of event consumption, redraw and effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusedTextFieldEditResult {
    /// The live editor changed in one history-preserving transaction.
    Applied,
    /// A validated text-identical request left all editor state untouched.
    Noop,
    /// The editor was not changed.
    Rejected(ComposerValidationError),
}

/// Identity of one field lifetime. Never reuse it after the owning editor is replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusedTextFieldIdentity {
    /// Stable identity of the surface or editor owner.
    pub owner_id: String,
    /// Owner-local field identity.
    pub field_id: String,
    /// Monotonic lifetime discriminator, including replacement and interaction restart.
    pub lifetime: u64,
}

/// Bounded immutable state of a currently focused, visible, non-secret editor.
///
/// Owners must validate the document before exposing it, omit secret fields entirely,
/// and advance generation whenever focus, editor state or eligibility changes. Geometry
/// comes from the retained measured presentation, never independent text measurement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusedTextFieldSnapshot {
    /// Field and owner lifetime being addressed.
    pub identity: FocusedTextFieldIdentity,
    /// Focus/editor generation; must not wrap or be reused.
    pub generation: u64,
    /// Validated, bounded portable text, UTF-8 cursor and normalized selection.
    pub document: ComposerDocument,
    /// Whether this field currently permits completion edits.
    pub eligible: bool,
    /// Visible content rectangle in the host's coordinate space.
    pub content_rect: Rect,
    /// Measured visible caret anchor in the same coordinate space.
    pub anchor: Rect,
}

/// One synchronous edit request fenced by the complete originating field state.
///
/// The owner re-reads its live editor, validates this request, then performs a single
/// history-preserving replacement. This value never contains a mutable editor reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusedTextFieldEdit {
    /// Exact field state used to obtain the candidate.
    pub expected: FocusedTextFieldSnapshot,
    /// Portable bounded replacement; not a submit or execution request.
    pub edit: ComposerEdit,
}

/// Small terminal adapter retaining observation fences and measured geometry, not an editor.
///
/// Owners call `observe` on ordinary edits, undo/redo, cursor/selection and eligibility
/// transitions (including focus away and back without painting), and `painted` after
/// visible presentation. Never mark secrets eligible. Semantic callbacks stay owner-owned.
/// Clones are speculative snapshots of the same field lifetime, not independent
/// simultaneous owners. Commit only one candidate after successful presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusedTextFieldAdapter {
    identity: FocusedTextFieldIdentity,
    generation: u64,
    revision: u64,
    eligible: bool,
    observed: Option<TextEditState>,
    painted_generation: Option<u64>,
    content_rect: Rect,
    anchor: Rect,
    exhausted: bool,
}

static NEXT_FIELD_LIFETIME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_lifetime() -> Option<u64> {
    NEXT_FIELD_LIFETIME
        .fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |value| value.checked_add(1),
        )
        .ok()
}

impl FocusedTextFieldAdapter {
    /// Create an independently fenced field lifetime. Counter exhaustion disables it.
    #[must_use]
    pub fn new(owner_id: impl Into<String>, field_id: impl Into<String>) -> Self {
        let lifetime = next_lifetime();
        Self {
            identity: FocusedTextFieldIdentity {
                owner_id: owner_id.into(),
                field_id: field_id.into(),
                lifetime: lifetime.unwrap_or(0),
            },
            generation: 0,
            revision: 0,
            eligible: false,
            observed: None,
            painted_generation: None,
            content_rect: Rect::default(),
            anchor: Rect::default(),
            exhausted: lifetime.is_none(),
        }
    }

    /// Begin a new editor lifetime (replacement or interaction restart), clearing all fences.
    pub fn reset(&mut self) {
        let lifetime = next_lifetime();
        self.identity.lifetime = lifetime.unwrap_or(0);
        self.generation = 0;
        self.revision = 0;
        self.eligible = false;
        self.observed = None;
        self.painted_generation = None;
        self.content_rect = Rect::default();
        self.anchor = Rect::default();
        self.exhausted = lifetime.is_none();
    }

    /// Observe live editor/eligibility changes even when no frame is presented.
    /// Hidden, secret and oversized content is not retained. Changes invalidate geometry.
    pub fn observe(&mut self, buffer: &TextEditBuffer, eligible: bool) {
        let eligible =
            eligible && buffer.text().len() <= MAX_COMPOSER_DOCUMENT_BYTES && !self.exhausted;
        let observed = eligible.then(|| buffer.edit_state());
        if self.eligible == eligible && self.observed == observed {
            return;
        }
        self.painted_generation = None;
        self.anchor = Rect::default();
        if let (Some(generation), Some(revision)) =
            (self.generation.checked_add(1), self.revision.checked_add(1))
        {
            self.generation = generation;
            self.revision = revision;
            self.eligible = eligible;
            self.observed = observed;
        } else {
            self.exhausted = true;
            self.eligible = false;
            self.observed = None;
        }
    }

    /// Observe the owner's live input without retaining a mutable reference.
    pub fn observe_input(&mut self, state: &TextInputState, eligible: bool) {
        self.observe(state.buffer(), eligible);
    }

    /// Record measured, visibly presented geometry for the exact observed buffer.
    /// This never grants eligibility; call `observe` for current owner policy first.
    pub fn painted(&mut self, buffer: &TextEditBuffer, content_rect: Rect, anchor: Rect) {
        self.observe(buffer, self.eligible);
        self.content_rect = content_rect;
        self.anchor = anchor;
        self.painted_generation =
            (self.eligible && self.visible_geometry()).then_some(self.generation);
    }

    /// Record retained visible input geometry in the host coordinate space.
    pub fn painted_input(&mut self, state: &TextInputState, content_rect: Rect, anchor: Rect) {
        self.painted(state.buffer(), content_rect, anchor);
    }
    /// Snapshot the bounded live buffer, or disable completion for invalid/hidden state.
    #[must_use]
    pub fn snapshot(&self, buffer: &TextEditBuffer) -> Option<FocusedTextFieldSnapshot> {
        if !self.eligible
            || self.exhausted
            || !self.visible_geometry()
            || self.painted_generation != Some(self.generation)
            || buffer.text().len() > MAX_COMPOSER_DOCUMENT_BYTES
            || self.observed.as_ref() != Some(&buffer.edit_state())
            || self.identity.owner_id.trim().is_empty()
            || self.identity.field_id.trim().is_empty()
        {
            return None;
        }
        let document = ComposerDocument {
            document_id: format!("focused-field-{}", self.identity.lifetime),
            revision: self.revision,
            text: buffer.text().to_owned(),
            cursor: buffer.cursor_byte_index(),
            selection: buffer.selection().map(|range| ComposerTextRange {
                start: range.start,
                end: range.end,
            }),
        };
        document.validate().ok()?;
        Some(FocusedTextFieldSnapshot {
            identity: self.identity.clone(),
            generation: self.generation,
            document,
            eligible: true,
            content_rect: self.content_rect,
            anchor: self.anchor,
        })
    }

    fn visible_geometry(&self) -> bool {
        let content = self.content_rect;
        let anchor = self.anchor;
        content.width > 0
            && content.height > 0
            && anchor.width > 0
            && anchor.height > 0
            && anchor.x >= content.x
            && anchor.y >= content.y
            && u32::from(anchor.x) + u32::from(anchor.width)
                <= u32::from(content.x) + u32::from(content.width)
            && u32::from(anchor.y) + u32::from(anchor.height)
                <= u32::from(content.y) + u32::from(content.height)
    }

    /// Snapshot an owner-managed input using its live buffer and retained owner geometry.
    #[must_use]
    pub fn snapshot_input(&self, state: &TextInputState) -> Option<FocusedTextFieldSnapshot> {
        self.snapshot(state.buffer())
    }

    fn prepare(
        &self,
        buffer: &TextEditBuffer,
        edit: &FocusedTextFieldEdit,
    ) -> Result<Option<ComposerDocument>, ComposerValidationError> {
        let current = self
            .snapshot(buffer)
            .ok_or(ComposerValidationError::Stale)?;
        let result = edit.validate(&current)?;
        if result.is_some() && self.generation == u64::MAX {
            return Err(ComposerValidationError::RevisionExhausted);
        }
        Ok(result)
    }

    /// Validate fresh live state and atomically replace one range in the existing history.
    /// No-op/rejected operations leave editor, history and metadata untouched. Applied
    /// advances revision/generation and invalidates the anchor until the owner repaints.
    pub fn apply(
        &mut self,
        buffer: &mut TextEditBuffer,
        edit: &FocusedTextFieldEdit,
    ) -> FocusedTextFieldEditResult {
        let next = match self.prepare(buffer, edit) {
            Ok(Some(next)) => next,
            Ok(None) => return FocusedTextFieldEditResult::Noop,
            Err(error) => return FocusedTextFieldEditResult::Rejected(error),
        };
        self.replace(buffer, edit, &next)
    }

    fn replace(
        &mut self,
        buffer: &mut TextEditBuffer,
        edit: &FocusedTextFieldEdit,
        next: &ComposerDocument,
    ) -> FocusedTextFieldEditResult {
        let expected = buffer.edit_state();
        match buffer.replace_byte_range(
            &expected,
            edit.edit.range.start..edit.edit.range.end,
            &edit.edit.replacement,
            next.cursor,
            None,
        ) {
            Ok(true) => {
                self.revision = next.revision;
                self.generation += 1;
                self.observed = Some(buffer.edit_state());
                self.painted_generation = None;
                self.anchor = Rect::default();
                FocusedTextFieldEditResult::Applied
            }
            Ok(false) => FocusedTextFieldEditResult::Noop,
            Err(TextEditTransactionError::Stale) => {
                FocusedTextFieldEditResult::Rejected(ComposerValidationError::Stale)
            }
            Err(
                TextEditTransactionError::InvalidRange | TextEditTransactionError::InvalidPosition,
            ) => FocusedTextFieldEditResult::Rejected(ComposerValidationError::InvalidPayload),
        }
    }

    /// Apply to the live input buffer; do not reset or replace the owner input state.
    /// Portable prevalidation avoids invalidating the input's layout cache for no-ops
    /// and stale payloads. BMUX position rejection may release derived layout only.
    pub fn apply_input(
        &mut self,
        state: &mut TextInputState,
        edit: &FocusedTextFieldEdit,
    ) -> FocusedTextFieldEditResult {
        let next = match self.prepare(state.buffer(), edit) {
            Ok(Some(next)) => next,
            Ok(None) => return FocusedTextFieldEditResult::Noop,
            Err(error) => return FocusedTextFieldEditResult::Rejected(error),
        };
        self.replace(state.buffer_mut(), edit, &next)
    }
}

impl FocusedTextFieldEdit {
    /// Validate against freshly materialized owner state without changing the editor.
    ///
    /// `None` is a text-identical edit: leave cursor, selection, revision and history
    /// untouched. Geometry changes alone do not stale an otherwise identical editor.
    /// Owners must still verify that the field remains focused and visible.
    ///
    /// # Errors
    /// Returns `Stale` for changed owner, lifetime, generation, document or eligibility;
    /// returns portable edit validation errors for invalid or exhausted replacements.
    pub fn validate(
        &self,
        current: &FocusedTextFieldSnapshot,
    ) -> Result<Option<ComposerDocument>, ComposerValidationError> {
        if !current.eligible
            || !self.expected.eligible
            || current.identity != self.expected.identity
            || current.generation != self.expected.generation
            || current.document != self.expected.document
        {
            return Err(ComposerValidationError::Stale);
        }
        let result = self.edit.apply(&current.document)?;
        if result.text == current.document.text {
            Ok(None)
        } else {
            Ok(Some(result))
        }
    }
}

/// Reusable behavioral assertions for real owner-managed focused fields.
///
/// Kept at the terminal SDK boundary so plugin tests can exercise their actual hooks
/// without duplicating fixture inventories or adding portable terminal dependencies.
pub mod test_support {
    use super::{
        ComposerEdit, ComposerTextRange, ComposerValidationError, FocusedTextFieldEdit,
        FocusedTextFieldEditResult, FocusedTextFieldSnapshot,
    };

    /// Assert that text-identical and stale completion requests preserve a live field.
    ///
    /// Supply an eligible, visibly painted field without active selection. `apply` should
    /// invoke the real owner's explicit completion hook and return its edit outcome;
    /// it can also assert unchanged owner history and semantic callback counters.
    /// Owner-specific focus, hidden-field and editor replacement transitions remain
    /// the owner's tests rather than being simulated by this assertion.
    ///
    /// # Panics
    /// Panics if the fixture is absent, invalid or exhausted, if a text-identical edit
    /// is not `Noop`, or if stale generation, lifetime or revision edits fail to reject
    /// without changing the exposed field snapshot.
    pub fn assert_noop_and_stale_preserve_field<O>(
        owner: &mut O,
        snapshot: impl Fn(&O) -> Option<FocusedTextFieldSnapshot>,
        apply: impl Fn(&mut O, &FocusedTextFieldEdit) -> FocusedTextFieldEditResult,
    ) {
        let expected = snapshot(owner).expect("eligible painted conformance field");
        let cursor = expected.document.cursor;
        let noop = FocusedTextFieldEdit {
            edit: ComposerEdit {
                document_id: expected.document.document_id.clone(),
                revision: expected.document.revision,
                range: ComposerTextRange {
                    start: cursor,
                    end: cursor,
                },
                expected_text: String::new(),
                replacement: String::new(),
                cursor,
            },
            expected: expected.clone(),
        };
        assert_eq!(noop.validate(&expected), Ok(None), "valid no-op fixture");
        assert_eq!(apply(owner, &noop), FocusedTextFieldEditResult::Noop);
        assert_eq!(snapshot(owner).as_ref(), Some(&expected));
        let mut stale = noop.clone();
        stale.expected.generation ^= 1;
        let mut lifetime = noop.clone();
        lifetime.expected.identity.lifetime ^= 1;
        let mut revision = noop;
        revision.edit.revision ^= 1;
        for request in [stale, lifetime, revision] {
            assert_eq!(
                apply(owner, &request),
                FocusedTextFieldEditResult::Rejected(ComposerValidationError::Stale)
            );
            assert_eq!(snapshot(owner).as_ref(), Some(&expected));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_plugin_models::composer::ComposerTextRange;

    fn request() -> FocusedTextFieldEdit {
        FocusedTextFieldEdit {
            expected: FocusedTextFieldSnapshot {
                identity: FocusedTextFieldIdentity {
                    owner_id: "question-1".into(),
                    field_id: "answer".into(),
                    lifetime: 1,
                },
                generation: 2,
                document: ComposerDocument {
                    document_id: "answer-1".into(),
                    revision: 3,
                    text: "#é".into(),
                    cursor: 3,
                    selection: None,
                },
                eligible: true,
                content_rect: Rect::new(1, 1, 10, 1),
                anchor: Rect::new(3, 1, 1, 1),
            },
            edit: ComposerEdit {
                document_id: "answer-1".into(),
                revision: 3,
                range: ComposerTextRange { start: 0, end: 3 },
                expected_text: "#é".into(),
                replacement: "topic".into(),
                cursor: 5,
            },
        }
    }

    fn adapter(buffer: &TextEditBuffer) -> FocusedTextFieldAdapter {
        let mut adapter = FocusedTextFieldAdapter::new("owner", "field");
        adapter.observe(buffer, true);
        adapter.painted(buffer, Rect::new(1, 1, 12, 2), Rect::new(2, 1, 1, 1));
        adapter
    }

    fn live_request(
        adapter: &FocusedTextFieldAdapter,
        buffer: &TextEditBuffer,
        replacement: &str,
    ) -> FocusedTextFieldEdit {
        let expected = adapter.snapshot(buffer).unwrap();
        let edit = ComposerEdit {
            document_id: expected.document.document_id.clone(),
            revision: expected.document.revision,
            range: ComposerTextRange {
                start: 0,
                end: buffer.text().len(),
            },
            expected_text: buffer.text().into(),
            replacement: replacement.into(),
            cursor: replacement.len(),
        };
        FocusedTextFieldEdit { expected, edit }
    }

    #[test]
    fn shared_conformance_assertion_exercises_live_adapter_hooks() {
        let buffer = TextEditBuffer::from_text("#é");
        let mut fixture = (adapter(&buffer), buffer);
        test_support::assert_noop_and_stale_preserve_field(
            &mut fixture,
            |(adapter, buffer)| adapter.snapshot(buffer),
            |(adapter, buffer), request| {
                let before = buffer.clone();
                let result = adapter.apply(buffer, request);
                assert_eq!(*buffer, before);
                result
            },
        );
    }

    #[test]
    fn atomic_live_edit_retains_unicode_cursor_and_history() {
        let mut buffer = TextEditBuffer::from_text("é👩‍💻中e\u{301}");
        let before = buffer.edit_state();
        let mut adapter = adapter(&buffer);
        let request = live_request(&adapter, &buffer, "完成 👩‍💻");
        assert_eq!(
            adapter.apply(&mut buffer, &request),
            FocusedTextFieldEditResult::Applied
        );
        let after = buffer.edit_state();
        assert_eq!(buffer.text(), "完成 👩‍💻");
        assert_eq!(buffer.cursor_byte_index(), buffer.text().len());
        assert!(buffer.selection().is_none());
        assert!(adapter.snapshot(&buffer).is_none());
        assert!(buffer.undo());
        assert_eq!(buffer.edit_state(), before);
        assert!(buffer.redo());
        assert_eq!(buffer.edit_state(), after);
    }

    #[test]
    fn noops_and_rejections_preserve_history_cursor_and_metadata() {
        let mut buffer = TextEditBuffer::from_text("é👩‍💻中e\u{301}");
        buffer.insert_char('!');
        let mut adapter = adapter(&buffer);
        let before = buffer.clone();
        let snapshot = adapter.snapshot(&buffer);
        let mut request = live_request(&adapter, &buffer, buffer.text());
        request.edit.cursor = 0;
        assert_eq!(
            adapter.apply(&mut buffer, &request),
            FocusedTextFieldEditResult::Noop
        );
        assert_eq!(buffer, before);
        assert_eq!(adapter.snapshot(&buffer), snapshot);
        request.edit.replacement = "e\u{301}".into();
        request.edit.cursor = 1; // UTF-8 boundary, but not a BMUX grapheme boundary.
        assert_eq!(
            adapter.apply(&mut buffer, &request),
            FocusedTextFieldEditResult::Rejected(ComposerValidationError::InvalidPayload)
        );
        assert_eq!(buffer, before);
        assert_eq!(adapter.snapshot(&buffer), snapshot);
        request.edit.cursor = 3;
        request.expected.generation += 1;
        assert_eq!(
            adapter.apply(&mut buffer, &request),
            FocusedTextFieldEditResult::Rejected(ComposerValidationError::Stale)
        );
        assert_eq!(buffer, before);
    }

    #[test]
    fn observation_fences_unpainted_cursor_focus_roundtrip_and_replacement() {
        let mut buffer = TextEditBuffer::from_text("#é");
        let mut adapter = adapter(&buffer);
        let request = live_request(&adapter, &buffer, "topic");
        buffer.move_cursor(bmux_text_edit::TextMotion::Start);
        assert!(adapter.snapshot(&buffer).is_none());
        adapter.observe(&buffer, true);
        assert!(adapter.snapshot(&buffer).is_none());
        adapter.painted(&buffer, Rect::new(1, 1, 12, 2), Rect::new(1, 1, 1, 1));
        assert!(adapter.snapshot(&buffer).is_some());
        adapter.observe(&buffer, false);
        adapter.observe(&buffer, true);
        assert!(adapter.snapshot(&buffer).is_none());
        adapter.painted(&buffer, Rect::new(1, 1, 12, 2), Rect::new(1, 1, 1, 1));
        assert_eq!(
            adapter.apply(&mut buffer, &request),
            FocusedTextFieldEditResult::Rejected(ComposerValidationError::Stale)
        );
        let previous = adapter.snapshot(&buffer).unwrap();
        adapter.reset();
        adapter.observe(&buffer, true);
        adapter.painted(&buffer, Rect::new(1, 1, 12, 2), Rect::new(1, 1, 1, 1));
        assert_ne!(
            adapter.snapshot(&buffer).unwrap().identity,
            previous.identity
        );
    }

    #[test]
    fn retained_geometry_requires_visible_contained_anchor_but_resize_is_not_an_edit() {
        let buffer = TextEditBuffer::from_text("#é");
        let mut adapter = adapter(&buffer);
        let original = adapter.snapshot(&buffer).unwrap();
        for (content, anchor) in [
            (Rect::default(), Rect::new(0, 0, 1, 1)),
            (Rect::new(1, 1, 1, 1), Rect::new(2, 1, 1, 1)),
            (Rect::new(1, 1, 1, 1), Rect::new(1, 1, 2, 1)),
            (Rect::new(1, 1, 1, 1), Rect::default()),
        ] {
            adapter.painted(&buffer, content, anchor);
            assert!(adapter.snapshot(&buffer).is_none());
        }
        adapter.painted(&buffer, Rect::new(3, 3, 1, 1), Rect::new(3, 3, 1, 1));
        let resized = adapter.snapshot(&buffer).unwrap();
        assert_eq!(resized.generation, original.generation);
        assert_eq!(resized.document, original.document);
    }

    #[test]
    fn live_input_edits_use_existing_owner_buffer_and_noop_keeps_state() {
        let mut input = TextInputState::new(TextEditBuffer::new());
        input.buffer_mut().insert_str("#é");
        let mut adapter = FocusedTextFieldAdapter::new("owner", "input");
        adapter.observe_input(&input, true);
        adapter.painted_input(&input, Rect::new(1, 1, 12, 2), Rect::new(2, 1, 1, 1));
        let request = live_request(&adapter, input.buffer(), "#é");
        let before = input.clone();
        assert_eq!(
            adapter.apply_input(&mut input, &request),
            FocusedTextFieldEditResult::Noop
        );
        assert_eq!(input, before);
        let request = live_request(&adapter, input.buffer(), "topic");
        assert_eq!(
            adapter.apply_input(&mut input, &request),
            FocusedTextFieldEditResult::Applied
        );
        assert!(input.buffer_mut().undo());
        assert_eq!(input.buffer().text(), "#é");
        assert!(adapter.snapshot_input(&input).is_none());
    }

    #[test]
    fn active_selection_and_revision_exhaustion_leave_history_untouched() {
        let mut buffer = TextEditBuffer::from_text("#é");
        buffer.select_all();
        let mut adapter = adapter(&buffer);
        let request = live_request(&adapter, &buffer, "topic");
        let before = buffer.clone();
        assert_eq!(
            adapter.apply(&mut buffer, &request),
            FocusedTextFieldEditResult::Rejected(ComposerValidationError::InvalidPayload)
        );
        assert_eq!(buffer, before);
        buffer.move_cursor(bmux_text_edit::TextMotion::End);
        adapter.observe(&buffer, true);
        adapter.revision = u64::MAX;
        adapter.painted(&buffer, Rect::new(1, 1, 12, 2), Rect::new(2, 1, 1, 1));
        let request = live_request(&adapter, &buffer, "topic");
        let before = buffer.clone();
        assert_eq!(
            adapter.apply(&mut buffer, &request),
            FocusedTextFieldEditResult::Rejected(ComposerValidationError::RevisionExhausted)
        );
        assert_eq!(buffer, before);
    }

    #[test]
    fn exhausted_generation_and_hidden_state_fail_closed() {
        let mut buffer = TextEditBuffer::from_text("#é");
        let mut adapter = adapter(&buffer);
        adapter.generation = u64::MAX;
        adapter.painted_generation = Some(u64::MAX);
        let request = live_request(&adapter, &buffer, "topic");
        assert_eq!(
            adapter.apply(&mut buffer, &request),
            FocusedTextFieldEditResult::Rejected(ComposerValidationError::RevisionExhausted)
        );
        assert_eq!(buffer.text(), "#é");
        adapter.observe(&buffer, false);
        assert!(adapter.exhausted);
        assert!(adapter.observed.is_none());
        assert!(adapter.snapshot(&buffer).is_none());
    }

    #[test]
    fn validates_unicode_replacement_without_mutating_snapshot() {
        let request = request();
        let result = request.validate(&request.expected).unwrap().unwrap();
        assert_eq!(result.text, "topic");
        assert_eq!(result.cursor, 5);
        assert_eq!(result.revision, 4);
        assert_eq!(request.expected.document.text, "#é");
    }

    #[test]
    fn rejects_stale_focus_lifetime_text_cursor_selection_and_eligibility() {
        let request = request();
        let original = request.expected.clone();
        let mut stale = Vec::new();
        let mut current = original.clone();
        current.identity.owner_id = "other-owner".into();
        stale.push(current);
        let mut current = original.clone();
        current.identity.field_id = "other-field".into();
        stale.push(current);
        let mut current = original.clone();
        current.identity.lifetime += 1;
        stale.push(current);
        let mut current = original.clone();
        current.generation += 1;
        stale.push(current);
        let mut current = original.clone();
        current.document.text = "#a".into();
        stale.push(current);
        let mut current = original.clone();
        current.document.cursor = 0;
        stale.push(current);
        let mut current = original.clone();
        current.document.selection = Some(ComposerTextRange { start: 0, end: 3 });
        stale.push(current);
        let mut current = original;
        current.eligible = false;
        stale.push(current);
        for current in stale {
            assert_eq!(
                request.validate(&current),
                Err(ComposerValidationError::Stale)
            );
        }
    }

    #[test]
    fn text_identical_acceptance_preserves_cursor_and_invalid_edit_is_rejected() {
        let mut request = request();
        request
            .edit
            .replacement
            .clone_from(&request.expected.document.text);
        request.edit.cursor = 0;
        assert_eq!(request.validate(&request.expected), Ok(None));
        request.edit.range.end = 2;
        assert_eq!(
            request.validate(&request.expected),
            Err(ComposerValidationError::InvalidPayload)
        );
    }
}
