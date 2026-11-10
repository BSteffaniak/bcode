//! Inline transcript tooling state that shapes structure resolution: inline shell expansion
//! (C39) and the rows reserved for an inline interaction surface (D10).
//!
//! Both are ephemeral presentation state (A3): bounded, reconciled on residency, never
//! persisted.

use std::collections::BTreeSet;

use bcode_presentation_policy::models::{Disclosure, ResolvedPresentation};
use bcode_session_view_models::{TranscriptViewItem, TranscriptViewItemId, TranscriptViewItemKind};

use super::item_facts::{extract_target, tool_arguments};
use crate::layout::producers::tool::SHELL_MAX_ROWS;

/// Most items whose shell output is expanded inline at once; the oldest expansion is dropped
/// first.
pub const MAX_SHELL_EXPANSIONS: usize = 64;
/// Most logical rows an inline interaction reserves in the transcript (D10).
pub const MAX_INLINE_INTERACTION_ROWS: u32 = 4096;
/// Section of a tool unit that holds its shell screen.
pub const SHELL_OUTPUT_SECTION: &str = "output";

/// The interaction surface the host places inside the transcript
/// (`interactions.placement = transcript`): which interaction it shows and how many logical
/// rows it needs at the current width.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InlineInteraction {
    /// Interaction id of the surface; it reserves rows in the unresolved interaction item with
    /// this id.
    pub interaction_id: String,
    /// Logical rows the surface needs; clamped to at least one and at most
    /// [`MAX_INLINE_INTERACTION_ROWS`].
    pub rows: u32,
}

impl InlineInteraction {
    /// A reservation of `rows` rows for `interaction_id`, clamped to the D10 bounds.
    #[must_use]
    pub fn new(interaction_id: impl Into<String>, rows: u32) -> Self {
        Self {
            interaction_id: interaction_id.into(),
            rows: rows.clamp(1, MAX_INLINE_INTERACTION_ROWS),
        }
    }
}

/// Inline tooling state consulted by structure resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InlineTools {
    /// Items whose shell output is expanded, oldest first.
    shells: Vec<TranscriptViewItemId>,
    /// The active inline interaction reservation.
    interaction: Option<InlineInteraction>,
    /// Bumped by every change; resolution re-places items when it moves.
    revision: u64,
    /// Last disclosure a global detail cycle applied to every unit; the next cycle continues
    /// from it. Not a placement input.
    detail_cycle: Option<Disclosure>,
}

impl InlineTools {
    /// Revision bumped by every change.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Last disclosure a global detail cycle applied.
    #[must_use]
    pub const fn detail_cycle(&self) -> Option<Disclosure> {
        self.detail_cycle
    }

    /// Record the disclosure a global detail cycle applied.
    pub const fn set_detail_cycle(&mut self, disclosure: Disclosure) {
        self.detail_cycle = Some(disclosure);
    }

    /// Whether `item`'s shell output is expanded.
    #[must_use]
    pub fn shell_expanded(&self, item: &TranscriptViewItemId) -> bool {
        self.shells.contains(item)
    }

    /// Expand or collapse `item`'s shell output; returns whether anything changed.
    pub fn set_shell_expanded(&mut self, item: &TranscriptViewItemId, expanded: bool) -> bool {
        let present = self.shell_expanded(item);
        if present == expanded {
            return false;
        }
        if expanded {
            self.shells.push(item.clone());
            let excess = self.shells.len().saturating_sub(MAX_SHELL_EXPANSIONS);
            self.shells.drain(..excess);
        } else {
            self.shells.retain(|candidate| candidate != item);
        }
        self.bump();
        true
    }

    /// The active inline interaction reservation.
    #[must_use]
    pub const fn interaction(&self) -> Option<&InlineInteraction> {
        self.interaction.as_ref()
    }

    /// Replace the inline interaction reservation; returns whether it changed.
    pub fn set_interaction(&mut self, interaction: Option<InlineInteraction>) -> bool {
        if self.interaction == interaction {
            return false;
        }
        self.interaction = interaction;
        self.bump();
        true
    }

    /// Rows reserved in an item whose unresolved interaction id is `interaction`.
    #[must_use]
    pub fn reserved_rows(&self, interaction: Option<&str>) -> Option<u32> {
        let reserved = self.interaction.as_ref()?;
        (Some(reserved.interaction_id.as_str()) == interaction).then_some(reserved.rows)
    }

    /// Forget shell expansions of items that left the resident window (D12 analogue).
    pub fn reconcile(&mut self, resident: &BTreeSet<&TranscriptViewItemId>) {
        let before = self.shells.len();
        self.shells.retain(|item| resident.contains(item));
        if self.shells.len() != before {
            self.bump();
        }
    }

    const fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

/// Inline tooling an item supports.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum InlineSurface {
    /// None.
    #[default]
    None,
    /// A tool whose output is a shell screen (C39).
    Shell,
    /// An unresolved interactive request with this interaction id (D10).
    Interaction(String),
}

impl InlineSurface {
    /// The inline tooling `item` supports.
    #[must_use]
    pub fn of(item: &TranscriptViewItem) -> Self {
        pending_interaction(item).map_or_else(
            || {
                if is_shell_invocation(item) {
                    Self::Shell
                } else {
                    Self::None
                }
            },
            Self::Interaction,
        )
    }

    /// Interaction id of a pending interaction.
    #[must_use]
    pub fn interaction(&self) -> Option<&str> {
        match self {
            Self::Interaction(id) => Some(id),
            Self::None | Self::Shell => None,
        }
    }
}

/// Whether `item` is a tool invocation whose output is a shell screen: its arguments carry a
/// `command`, the field the tool producer renders shell output for.
#[must_use]
pub fn is_shell_invocation(item: &TranscriptViewItem) -> bool {
    matches!(
        item.kind,
        TranscriptViewItemKind::ToolInvocation { .. } | TranscriptViewItemKind::ToolRequest { .. }
    ) && tool_arguments(item)
        .and_then(|arguments| extract_target(arguments, ["command"]))
        .is_some()
}

/// Interaction id of an unresolved interactive request item.
#[must_use]
pub fn pending_interaction(item: &TranscriptViewItem) -> Option<String> {
    match &item.kind {
        TranscriptViewItemKind::Interaction { interaction } if !interaction.resolved => {
            Some(interaction.interaction_id.clone())
        }
        _ => None,
    }
}

/// Open a unit's shell screen inline (C39): the unit and its output section expand, and the
/// output shows up to [`SHELL_MAX_ROWS`] rows.
pub fn expand_shell_output(presentation: &mut ResolvedPresentation) {
    presentation.disclosure = Disclosure::Expanded;
    let mut section = presentation.section(SHELL_OUTPUT_SECTION);
    section.visible = true;
    section.disclosure = Disclosure::Expanded;
    section.preview.lines = section.preview.lines.max(SHELL_MAX_ROWS);
    presentation
        .sections
        .insert(SHELL_OUTPUT_SECTION.to_owned(), section);
}
