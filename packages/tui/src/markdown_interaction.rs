//! Stable keyboard focus state for visible Markdown contributions.

/// Keyboard focus over the currently visible actionable Markdown contributions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarkdownFocusState {
    focused: Option<String>,
    visible: Vec<String>,
}

impl MarkdownFocusState {
    /// Reconcile focus with actionable contributions in document order.
    pub fn reconcile(&mut self, visible: Vec<String>) {
        if self
            .focused
            .as_ref()
            .is_some_and(|focused| visible.contains(focused))
        {
            self.visible = visible;
            return;
        }

        let previous_index = self
            .focused
            .as_ref()
            .and_then(|focused| self.visible.iter().position(|id| id == focused));
        self.visible = visible;
        self.focused = previous_index
            .and_then(|index| self.visible.get(index).or_else(|| self.visible.last()))
            .cloned();
    }

    /// Move focus to the next visible contribution, wrapping at the end.
    pub fn focus_next(&mut self) -> bool {
        self.move_focus(false)
    }

    /// Move focus to the previous visible contribution, wrapping at the start.
    pub fn focus_previous(&mut self) -> bool {
        self.move_focus(true)
    }

    /// Return the focused contribution ID.
    #[must_use]
    pub fn focused(&self) -> Option<&str> {
        self.focused.as_deref()
    }

    fn move_focus(&mut self, reverse: bool) -> bool {
        if self.visible.is_empty() {
            return self.focused.take().is_some();
        }
        let next = self.focused.as_ref().and_then(|focused| {
            self.visible
                .iter()
                .position(|id| id == focused)
                .map(|index| {
                    if reverse {
                        index.checked_sub(1).unwrap_or(self.visible.len() - 1)
                    } else {
                        (index + 1) % self.visible.len()
                    }
                })
        });
        self.focused = Some(
            self.visible[next.unwrap_or_else(|| if reverse { self.visible.len() - 1 } else { 0 })]
                .clone(),
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::MarkdownFocusState;

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn traversal_wraps_in_both_directions() {
        let mut state = MarkdownFocusState::default();
        state.reconcile(ids(&["a", "b"]));
        assert!(state.focus_next());
        assert_eq!(state.focused(), Some("a"));
        assert!(state.focus_next());
        assert_eq!(state.focused(), Some("b"));
        assert!(state.focus_next());
        assert_eq!(state.focused(), Some("a"));
        assert!(state.focus_previous());
        assert_eq!(state.focused(), Some("b"));
    }

    #[test]
    fn reconciliation_preserves_identity_and_uses_disappeared_position() {
        let mut state = MarkdownFocusState::default();
        state.reconcile(ids(&["a", "b", "c"]));
        state.focus_next();
        state.focus_next();
        state.reconcile(ids(&["x", "b", "c"]));
        assert_eq!(state.focused(), Some("b"));

        state.reconcile(ids(&["x", "c"]));
        assert_eq!(state.focused(), Some("c"));
        state.reconcile(Vec::new());
        assert_eq!(state.focused(), None);
    }
}
