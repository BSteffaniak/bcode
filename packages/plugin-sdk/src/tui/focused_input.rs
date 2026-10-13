//! Opt-in access to a focused, owner-managed text field at the terminal boundary.

use bcode_plugin_models::composer::{ComposerDocument, ComposerEdit, ComposerValidationError};
use bmux_tui::geometry::Rect;

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
