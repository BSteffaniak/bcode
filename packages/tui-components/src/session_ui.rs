//! Shared BMUX TUI composer recipe. This is not a renderer-neutral frontend contract.
//!
//! Native and browser-local BMUX hosts own transport, transcript state, canonical session
//! state and platform services outside this module. The transcript component a host stacks
//! above the composer retains its own source mappings; this recipe never flattens them.

use std::cell::RefCell;

use bmux_tui::component::Component;
use bmux_tui::composition::{Column, Flex, SizeBox};
use bmux_tui::prelude::Style;
use bmux_tui_components::text_input::{TextInputComponent, TextInputPolicy, TextInputState};

use crate::composer::{ComposerPanel, ComposerStyle, composer_panel};

#[cfg(test)]
mod tests;

pub mod bindings;

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

/// Stack a transcript component above the shared editor using BMUX only.
///
/// The transcript component owns its source selection; it is never converted into paint calls,
/// ANSI, or a second wrapping engine here.
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
