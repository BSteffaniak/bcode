//! Configured input admission for the shared BMUX TUI adapter.
//!
//! The host supplies its effective keymap (including overrides and contributed
//! bindings). Unknown/disabled shortcuts never fall through to default Enter or
//! editor behavior. Application commands stay outside this ephemeral UI kernel.

use bmux_keyboard::KeyStroke;
use bmux_text_edit::{TextEditCommand, TextMotion};
use bmux_tui_components::text_input::{TextInputControl, TextInputOutcome, TextInputState};

use super::composer_policy;

#[cfg(test)]
mod tests;

/// A key already resolved against the effective configured chat keymap.
pub enum SessionKeyBinding {
    /// An editor command resolved by the configured editor bindings.
    Editor(TextEditCommand),
    /// Selection motion; native hosts may apply their visual-row navigation policy.
    Selection(TextMotion),
    /// A verified literal text key, not a fallback shortcut.
    Literal(KeyStroke),
    /// An application command, chord/prefix, plugin action or other host-owned key.
    Host,
    /// Explicitly ignored or disabled binding; do not run default editor behavior.
    Ignored,
}

/// Effective TUI binding resolver. It is not an application/frontend contract.
pub trait SessionKeyBindings {
    /// Resolve one key with overrides/contributions already applied.
    fn resolve(&self, key: KeyStroke) -> SessionKeyBinding;
}

/// Apply one configured editor binding.
///
/// Hosts retain application actions and
/// selection policies; `Host`/`Ignored` never become an implicit submit key.
/// Returns `None` for host-owned work, including source selection motion.
pub fn apply_editor_binding(
    state: &mut TextInputState,
    binding: SessionKeyBinding,
) -> Option<TextInputOutcome> {
    match binding {
        SessionKeyBinding::Editor(command) => {
            state.buffer_mut().apply_command(command);
            Some(TextInputOutcome::Edited)
        }
        SessionKeyBinding::Literal(key) => Some(
            TextInputControl::new(&composer_policy())
                .handle_key_with_history_action(state, key, None),
        ),
        SessionKeyBinding::Selection(_) | SessionKeyBinding::Host => None,
        SessionKeyBinding::Ignored => Some(TextInputOutcome::Ignored),
    }
}
