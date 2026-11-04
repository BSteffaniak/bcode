//! Pending multi-stroke input state for key sequences such as `g g`.

use bmux_keyboard::KeyStroke;

use crate::registry::{KeymapRegistry, SequenceMatch, normalize_stroke};

/// Outcome of feeding one stroke into a [`KeySequence`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SequenceStep {
    /// A binding completed; the pending buffer was cleared.
    Action(String),
    /// More strokes are required. Callers should present continuations and may time out.
    Pending,
    /// No binding matched this stroke. Callers may treat it as unbound input.
    Unmatched(KeyStroke),
}

/// Per-scope pending key-sequence buffer owned by one input handler.
///
/// A stroke that breaks a pending sequence abandons the prefix and is retried alone, so an
/// interrupted sequence never swallows the next independent shortcut.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeySequence {
    scope: String,
    pending: Vec<KeyStroke>,
}

impl KeySequence {
    /// Feed one stroke resolved in `scope`. Changing scope discards any pending prefix.
    pub fn feed(
        &mut self,
        registry: &KeymapRegistry,
        scope: &str,
        stroke: KeyStroke,
    ) -> SequenceStep {
        if self.scope != scope {
            scope.clone_into(&mut self.scope);
            self.pending.clear();
        }
        let stroke = normalize_stroke(stroke);
        self.pending.push(stroke);
        match registry.resolve_sequence(scope, &self.pending) {
            SequenceMatch::Complete(action) => {
                self.pending.clear();
                SequenceStep::Action(action.to_owned())
            }
            SequenceMatch::Pending => SequenceStep::Pending,
            SequenceMatch::NoMatch if self.pending.len() > 1 => {
                self.pending.clear();
                self.feed(registry, scope, stroke)
            }
            SequenceMatch::NoMatch => {
                self.pending.clear();
                SequenceStep::Unmatched(stroke)
            }
        }
    }

    /// Strokes typed so far toward an incomplete sequence.
    #[must_use]
    pub fn pending(&self) -> &[KeyStroke] {
        &self.pending
    }

    /// Whether a sequence prefix is awaiting completion.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Abandon any pending prefix, for example after a timeout or focus change.
    pub fn reset(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{KeymapAction, resolve_registry};

    fn stroke(key: &str) -> KeyStroke {
        bmux_keyboard::parse_key_stroke(key).unwrap()
    }

    fn registry() -> KeymapRegistry {
        resolve_registry(
            [
                KeymapAction {
                    id: "top".into(),
                    scope: "list".into(),
                    description: "Top".into(),
                    defaults: vec!["g g".into()],
                },
                KeymapAction {
                    id: "down".into(),
                    scope: "list".into(),
                    description: "Down".into(),
                    defaults: vec!["j".into()],
                },
            ],
            BTreeMap::new(),
        )
        .unwrap()
    }

    #[test]
    fn completes_sequences_and_single_keys() {
        let registry = registry();
        let mut sequence = KeySequence::default();
        assert_eq!(
            sequence.feed(&registry, "list", stroke("g")),
            SequenceStep::Pending
        );
        assert_eq!(sequence.pending(), [stroke("g")]);
        assert_eq!(
            sequence.feed(&registry, "list", stroke("g")),
            SequenceStep::Action("top".into())
        );
        assert!(!sequence.is_pending());
        assert_eq!(
            sequence.feed(&registry, "list", stroke("j")),
            SequenceStep::Action("down".into())
        );
    }

    #[test]
    fn interrupted_prefix_retries_the_breaking_stroke() {
        let registry = registry();
        let mut sequence = KeySequence::default();
        sequence.feed(&registry, "list", stroke("g"));
        assert_eq!(
            sequence.feed(&registry, "list", stroke("j")),
            SequenceStep::Action("down".into())
        );
        sequence.feed(&registry, "list", stroke("g"));
        assert_eq!(
            sequence.feed(&registry, "list", stroke("x")),
            SequenceStep::Unmatched(stroke("x"))
        );
        assert!(!sequence.is_pending());
    }

    #[test]
    fn scope_change_discards_pending_prefix() {
        let registry = registry();
        let mut sequence = KeySequence::default();
        sequence.feed(&registry, "list", stroke("g"));
        assert_eq!(
            sequence.feed(&registry, "other", stroke("g")),
            SequenceStep::Unmatched(stroke("g"))
        );
    }
}
