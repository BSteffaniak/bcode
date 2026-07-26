//! Stable keyboard and mouse interaction state for Markdown contributions.

use std::collections::BTreeMap;

use bcode_markdown_render::MarkdownContributionKind;

/// One currently visible actionable Markdown contribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleMarkdownContribution {
    /// Stable renderer-owned contribution identity.
    pub id: String,
    /// Typed semantic payload used for activation.
    pub kind: MarkdownContributionKind,
}

/// Keyboard focus and typed dispatch data for visible Markdown contributions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarkdownInteractionState {
    focused: Option<String>,
    visible_order: Vec<String>,
    visible: BTreeMap<String, MarkdownContributionKind>,
}

impl MarkdownInteractionState {
    /// Reconcile focus and dispatch data with contributions in visible document order.
    pub fn reconcile(&mut self, contributions: Vec<VisibleMarkdownContribution>) {
        let previous_index = self
            .focused
            .as_ref()
            .and_then(|focused| self.visible_order.iter().position(|id| id == focused));
        let mut visible_order = Vec::new();
        let mut visible = BTreeMap::new();
        for contribution in contributions {
            if visible
                .insert(contribution.id.clone(), contribution.kind)
                .is_none()
            {
                visible_order.push(contribution.id);
            }
        }

        if self
            .focused
            .as_ref()
            .is_some_and(|focused| visible.contains_key(focused))
        {
            self.visible_order = visible_order;
            self.visible = visible;
            return;
        }

        self.visible_order = visible_order;
        self.visible = visible;
        self.focused = previous_index
            .and_then(|index| {
                self.visible_order
                    .get(index)
                    .or_else(|| self.visible_order.last())
            })
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

    /// Focus a visible contribution by its stable identity.
    pub fn focus(&mut self, contribution_id: &str) -> bool {
        if !self.visible.contains_key(contribution_id) {
            return false;
        }
        let changed = self.focused.as_deref() != Some(contribution_id);
        self.focused = Some(contribution_id.to_owned());
        changed
    }

    /// Focus a typed target contribution by its stable identity.
    pub fn focus_target(&mut self, contribution_id: &str) -> bool {
        self.focus(contribution_id)
    }

    /// Return the focused contribution ID.
    #[must_use]
    pub fn focused(&self) -> Option<&str> {
        self.focused.as_deref()
    }

    /// Return the typed payload for a visible contribution.
    #[must_use]
    pub fn contribution(&self, contribution_id: &str) -> Option<&MarkdownContributionKind> {
        self.visible.get(contribution_id)
    }

    /// Return the focused typed contribution payload.
    #[must_use]
    pub fn focused_contribution(&self) -> Option<&MarkdownContributionKind> {
        self.focused
            .as_deref()
            .and_then(|focused| self.visible.get(focused))
    }

    fn move_focus(&mut self, reverse: bool) -> bool {
        if self.visible_order.is_empty() {
            return self.focused.take().is_some();
        }
        let next = self.focused.as_ref().and_then(|focused| {
            self.visible_order
                .iter()
                .position(|id| id == focused)
                .map(|index| {
                    if reverse {
                        index.checked_sub(1).unwrap_or(self.visible_order.len() - 1)
                    } else {
                        (index + 1) % self.visible_order.len()
                    }
                })
        });
        self.focused = Some(
            self.visible_order[next.unwrap_or_else(|| {
                if reverse {
                    self.visible_order.len() - 1
                } else {
                    0
                }
            })]
            .clone(),
        );
        true
    }
}

/// Extract the stable contribution identity from a Markdown hit-region ID.
#[must_use]
pub fn contribution_id_from_hit(hit_id: &str) -> Option<&str> {
    hit_id
        .strip_prefix("markdown:")?
        .rsplit_once(':')
        .map(|(contribution_id, _)| contribution_id)
        .filter(|contribution_id| !contribution_id.is_empty())
}

#[cfg(test)]
mod tests {
    use bcode_markdown_render::{
        MarkdownContributionKind, MarkdownDestination, MarkdownLink, MarkdownLinkKind,
    };

    use super::{MarkdownInteractionState, VisibleMarkdownContribution, contribution_id_from_hit};

    fn contribution(id: &str) -> VisibleMarkdownContribution {
        VisibleMarkdownContribution {
            id: id.to_owned(),
            kind: MarkdownContributionKind::Link {
                link: MarkdownLink {
                    destination: "https://example.com".to_owned(),
                    title: None,
                    reference_id: None,
                    kind: MarkdownLinkKind::Inline,
                },
                label: id.to_owned(),
                destination: MarkdownDestination::Web(
                    "https://example.com".parse().expect("valid URL"),
                ),
            },
        }
    }

    #[test]
    fn traversal_wraps_in_both_directions() {
        let mut state = MarkdownInteractionState::default();
        state.reconcile(vec![contribution("a"), contribution("b")]);
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
    fn reconciliation_preserves_identity_payload_and_disappeared_position() {
        let mut state = MarkdownInteractionState::default();
        state.reconcile(vec![
            contribution("a"),
            contribution("b"),
            contribution("c"),
        ]);
        state.focus_next();
        state.focus_next();
        state.reconcile(vec![
            contribution("x"),
            contribution("b"),
            contribution("c"),
        ]);
        assert_eq!(state.focused(), Some("b"));
        assert!(state.contribution("b").is_some());

        // Scrolling/clipping may remove contributions before the focused item
        // without changing the surviving typed identity.
        state.reconcile(vec![contribution("b"), contribution("c")]);
        assert_eq!(state.focused(), Some("b"));

        // Replacement removes stale payload and chooses the same visible
        // document-order position, clamped at the end.
        state.reconcile(vec![contribution("x"), contribution("c")]);
        assert_eq!(state.focused(), Some("x"));
        assert!(state.contribution("b").is_none());
        state.reconcile(Vec::new());
        assert_eq!(state.focused(), None);
        assert!(state.contribution("c").is_none());
    }

    #[test]
    fn resize_rect_duplicates_do_not_duplicate_focus_order() {
        let mut state = MarkdownInteractionState::default();
        state.reconcile(vec![
            contribution("wrapped"),
            contribution("wrapped"),
            contribution("next"),
        ]);

        state.focus_next();
        assert_eq!(state.focused(), Some("wrapped"));
        state.focus_next();
        assert_eq!(state.focused(), Some("next"));
    }

    #[test]
    fn mouse_hit_identity_preserves_colons_in_contribution_id() {
        assert_eq!(
            contribution_id_from_hit("markdown:transcript:7:link:12:3"),
            Some("transcript:7:link:12")
        );
        assert_eq!(contribution_id_from_hit("transcript"), None);
    }
}
