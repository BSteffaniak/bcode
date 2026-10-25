//! Shared BMUX TUI adapter state. This is not a renderer-neutral frontend contract.
//!
//! Native and browser-local BMUX hosts own transport, plugin snapshot acceptance,
//! canonical session state and platform services outside this module. Transcript
//! children retain their accepted source mappings; this recipe never flattens them.

use std::cell::RefCell;

use bmux_keyboard::KeyStroke;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::component::Component;
use bmux_tui::composition::{Column, Flex, SizeBox};
use bmux_tui::prelude::Style;
use bmux_tui_components::text_input::{
    TextInputComponent, TextInputControl, TextInputOutcome, TextInputPolicy, TextInputState,
};

use crate::composer::{ComposerPanel, ComposerStyle, composer_panel};
use crate::transcript_history::TranscriptHistory;
use crate::transcript_viewport::TranscriptViewport;

#[cfg(test)]
mod tests;

pub mod bindings;

/// Ephemeral state used by the native TUI and browser-local BMUX TUI adapter.
#[derive(Debug, Clone)]
pub struct SessionUi {
    /// Real BMUX editor state, including source selection and scrolling.
    pub composer: TextInputState,
    /// The native transcript navigation state machine, not a browser fork.
    pub viewport: TranscriptViewport,
}

impl Default for SessionUi {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionUi {
    /// Create an empty editor and a tail-following viewport.
    #[must_use]
    pub fn new() -> Self {
        Self {
            composer: TextInputState::new(TextEditBuffer::new()),
            viewport: TranscriptViewport::default(),
        }
    }

    /// Resolve a key through the host's effective configured bindings before editing.
    /// Host-owned actions and selection motions return `None`; hosts that handle
    /// those should call `SessionKeyBindings::resolve` directly before application.
    pub fn update_configured_key(
        &mut self,
        key: KeyStroke,
        bindings: &impl bindings::SessionKeyBindings,
    ) -> Option<TextInputOutcome> {
        bindings::apply_editor_binding(&mut self.composer, bindings.resolve(key))
    }

    /// Apply ephemeral UI input, returning platform work without executing it.
    pub fn update(
        &mut self,
        action: SessionUiAction,
        history: &mut impl TranscriptHistory,
    ) -> SessionUiEffect {
        match action {
            SessionUiAction::Key(key) => {
                match TextInputControl::new(&composer_policy()).handle_key(&mut self.composer, key)
                {
                    TextInputOutcome::Submitted => SessionUiEffect::SubmitRequested,
                    TextInputOutcome::Ignored => SessionUiEffect::None,
                    _ => SessionUiEffect::Redraw,
                }
            }
            SessionUiAction::Paste(text) => {
                TextInputControl::new(&composer_policy()).handle_paste(&mut self.composer, &text);
                SessionUiEffect::Redraw
            }
            SessionUiAction::ScrollUp(rows) => {
                self.viewport.scroll_up(rows, history);
                SessionUiEffect::Redraw
            }
            SessionUiAction::ScrollDown(rows) => {
                self.viewport.scroll_down(rows, history);
                SessionUiEffect::Redraw
            }
            SessionUiAction::FollowTail => {
                self.viewport.scroll_to_bottom(history);
                SessionUiEffect::Redraw
            }
        }
    }
}

/// Input already admitted by the host's keymap/platform boundary.
pub enum SessionUiAction {
    /// BMUX editor key, after application shortcuts have been handled.
    Key(KeyStroke),
    /// Text supplied by the platform; no clipboard access occurs here.
    Paste(String),
    /// Move toward older transcript rows.
    ScrollUp(usize),
    /// Move toward newer transcript rows.
    ScrollDown(usize),
    /// Resume following live output.
    FollowTail,
}

/// Work the host must route through its application/platform boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionUiEffect {
    /// No work.
    None,
    /// Ephemeral presentation changed.
    Redraw,
    /// Submit the draft through the application's normal action boundary.
    /// The draft is deliberately retained until application acceptance.
    SubmitRequested,
}

/// Caller-owned identities; hosts must namespace these across simultaneous panes.
pub struct SessionUiIds<'a> {
    /// Root stack identity.
    pub root: &'a str,
    /// Composer border identity.
    pub composer: &'a str,
    /// Editor identity.
    pub editor: &'a str,
    /// Composer size constraint identity.
    pub composer_size: &'a str,
}

/// Native composer policy shared with browser-local BMUX hosts.
#[must_use]
pub const fn composer_policy() -> TextInputPolicy {
    TextInputPolicy::chat_composer()
}

/// Semantic presentation styles for the shared editor recipe.
#[derive(Clone, Copy)]
pub struct SessionComposerStyle {
    /// Composer surface and border.
    pub panel: ComposerStyle,
    /// Editor text.
    pub text: Style,
    /// Selected source text.
    pub selection: Style,
    /// Placeholder text.
    pub muted: Style,
}

/// Compose the real native editor, retaining BMUX source selection and hit geometry.
#[must_use]
pub fn composer<'a>(
    ids: &SessionUiIds<'_>,
    state: &'a RefCell<TextInputState>,
    policy: &'a TextInputPolicy,
    style: SessionComposerStyle,
    focused: bool,
) -> ComposerPanel<'a> {
    let input = TextInputComponent::new(ids.editor.to_owned(), state, policy)
        .placeholder("Ask Bcode…", style.muted)
        .style(style.text)
        .selection_style(style.selection)
        .focused(focused);
    composer_panel(style.panel, input).id(ids.composer.to_owned())
}

/// Stack an accepted transcript component above the shared editor using BMUX only.
///
/// The transcript child owns source selection and accepted complete plugin snapshots.
/// It is never converted into paint calls, ANSI, or a second wrapping engine here.
#[must_use]
pub fn compose<'a>(
    ids: &SessionUiIds<'_>,
    transcript: impl Component + 'a,
    editor: impl Component + 'a,
    composer_height: u16,
) -> Column<'a> {
    Column::new()
        .id(ids.root.to_owned())
        .flex(Flex::new(1, transcript))
        .child(
            SizeBox::new(editor)
                .id(ids.composer_size.to_owned())
                .height(u64::from(composer_height)),
        )
}
