//! Portable shortcut edge handling and explicit compositor integration examples.
//!
//! This module does not register global shortcuts. A compositor or embedding host
//! supplies binding identifiers and key edges, then sends returned actions through
//! the public service API. No portal support is implied by these adapters.
use std::collections::BTreeMap;

use crate::{Error, Result, config::Config, contracts::Action};

/// Meaning of a configured shortcut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingMode {
    /// Press starts capture; release stops it.
    Hold,
    /// Each distinct press toggles capture; release only rearms the binding.
    Toggle,
}

/// An input edge supplied by the platform integration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyEvent {
    /// Initial press (duplicate presses are also suppressed).
    Pressed,
    /// Key release.
    Released,
    /// Auto-repeat; never initiates a dictation action.
    Repeat,
}

#[derive(Clone, Copy, Debug)]
struct Binding {
    mode: BindingMode,
    pressed: bool,
}

/// Edge state for independently configurable hold and toggle bindings.
///
/// Binding identifiers are opaque platform strings, compared exactly. Platform
/// adapters must normalize equivalent key combinations before constructing this
/// map, and must not feed synthetic repeat events as new presses.
#[derive(Debug, Default)]
pub struct Bindings {
    entries: BTreeMap<String, Binding>,
}

impl Bindings {
    /// Build bindings from configuration without registering OS resources.
    ///
    /// # Errors
    /// Returns an invalid-input error for blank or duplicate bindings.
    pub fn from_config(config: &Config) -> Result<Self> {
        let mut bindings = Self::default();
        if let Some(key) = &config.hold_shortcut {
            bindings.insert(key.clone(), BindingMode::Hold)?;
        }
        if let Some(key) = &config.toggle_shortcut {
            bindings.insert(key.clone(), BindingMode::Toggle)?;
        }
        Ok(bindings)
    }

    /// Register one opaque binding identifier.
    ///
    /// # Errors
    /// Returns an invalid-input error for blank or already registered identifiers.
    pub fn insert(&mut self, key: String, mode: BindingMode) -> Result<()> {
        if key.trim().is_empty() {
            return Err(Error::Invalid("shortcut binding must not be blank".into()));
        }
        if self.entries.contains_key(&key) {
            return Err(Error::Invalid("shortcut bindings must be distinct".into()));
        }
        self.entries.insert(
            key,
            Binding {
                mode,
                pressed: false,
            },
        );
        Ok(())
    }

    /// Consume a binding edge and return at most one control action.
    ///
    /// Unregistered identifiers, repeats, duplicate presses and unmatched releases
    /// are ignored. A release from one binding never rearms another binding.
    pub fn event(&mut self, key: &str, event: KeyEvent) -> Option<Action> {
        let binding = self.entries.get_mut(key)?;
        match event {
            KeyEvent::Repeat => None,
            KeyEvent::Pressed if binding.pressed => None,
            KeyEvent::Pressed => {
                binding.pressed = true;
                Some(match binding.mode {
                    BindingMode::Hold => Action::Start,
                    BindingMode::Toggle => Action::Toggle,
                })
            }
            KeyEvent::Released if !binding.pressed => None,
            KeyEvent::Released => {
                binding.pressed = false;
                (binding.mode == BindingMode::Hold).then_some(Action::Stop)
            }
        }
    }

    /// Reset edge state after lost input ownership or a disconnected input source.
    ///
    /// A held hold-to-talk binding requests cancellation rather than transcription:
    /// a lost release must not silently commit a recording. Toggle state belongs to
    /// the service, not the key state, and is not cancelled by this reset.
    pub fn reset(&mut self) -> Option<Action> {
        let mut held = false;
        for binding in self.entries.values_mut() {
            held |= binding.pressed && binding.mode == BindingMode::Hold;
            binding.pressed = false;
        }
        held.then_some(Action::Cancel)
    }
}

/// Supported configuration snippet formats; these are not runtime backends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compositor {
    /// Hyprland `bind` and `bindr` directives.
    Hyprland,
    /// Sway press and release `bindsym` directives.
    Sway,
}

/// Example compositor configuration using fixed example keys and PATH-resolved Vox.
///
/// Run `vox serve` separately first. Adjust keys and executable path explicitly.
/// Compositors own these bindings; `hold_shortcut` and `toggle_shortcut` in the
/// Vox configuration do not register them automatically. No repeated bindings are
/// enabled, so holding the key does not send repeated start/toggle operations.
#[must_use]
pub const fn snippet(compositor: Compositor) -> &'static str {
    match compositor {
        Compositor::Hyprland => {
            "# Run vox serve separately. Example keys; adjust for your desktop.\n\
# No binde: auto-repeat must not restart recording.\n\
bind = SUPER SHIFT, V, exec, vox start\n\
bindr = SUPER SHIFT, V, exec, vox stop\n\
bind = SUPER SHIFT, B, exec, vox toggle\n\
bind = SUPER SHIFT, Escape, exec, vox cancel\n"
        }
        Compositor::Sway => {
            "# Run vox serve separately. Example keys; adjust for your desktop.\n\
bindsym --no-repeat Mod4+Shift+v exec vox start\n\
bindsym --no-repeat --release Mod4+Shift+v exec vox stop\n\
bindsym --no-repeat Mod4+Shift+b exec vox toggle\n\
bindsym --no-repeat Mod4+Shift+Escape exec vox cancel\n"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bindings() -> Bindings {
        let config = Config {
            hold_shortcut: Some("hold".into()),
            toggle_shortcut: Some("toggle".into()),
            ..Config::default()
        };
        Bindings::from_config(&config).expect("valid bindings")
    }

    #[test]
    fn hold_emits_one_start_and_one_stop_per_physical_press() {
        let mut bindings = bindings();
        assert_eq!(bindings.event("hold", KeyEvent::Released), None);
        assert_eq!(bindings.event("hold", KeyEvent::Repeat), None);
        assert_eq!(
            bindings.event("hold", KeyEvent::Pressed),
            Some(Action::Start)
        );
        assert_eq!(bindings.event("hold", KeyEvent::Pressed), None);
        assert_eq!(bindings.event("hold", KeyEvent::Repeat), None);
        assert_eq!(
            bindings.event("hold", KeyEvent::Released),
            Some(Action::Stop)
        );
        assert_eq!(bindings.event("hold", KeyEvent::Released), None);
        assert_eq!(
            bindings.event("hold", KeyEvent::Pressed),
            Some(Action::Start)
        );
    }

    #[test]
    fn toggle_rearms_only_on_its_own_release() {
        let mut bindings = bindings();
        assert_eq!(
            bindings.event("toggle", KeyEvent::Pressed),
            Some(Action::Toggle)
        );
        assert_eq!(bindings.event("hold", KeyEvent::Released), None);
        assert_eq!(bindings.event("toggle", KeyEvent::Repeat), None);
        assert_eq!(bindings.event("toggle", KeyEvent::Pressed), None);
        assert_eq!(bindings.event("toggle", KeyEvent::Released), None);
        assert_eq!(
            bindings.event("toggle", KeyEvent::Pressed),
            Some(Action::Toggle)
        );
    }

    #[test]
    fn unrelated_bindings_do_not_dispatch() {
        let mut bindings = bindings();
        assert_eq!(bindings.event("unknown", KeyEvent::Pressed), None);
        assert_eq!(bindings.event("unknown", KeyEvent::Released), None);
    }

    #[test]
    fn reset_cancels_held_capture_and_rearms_both_bindings() {
        let mut bindings = bindings();
        let _ = bindings.event("hold", KeyEvent::Pressed);
        let _ = bindings.event("toggle", KeyEvent::Pressed);
        assert_eq!(bindings.reset(), Some(Action::Cancel));
        assert_eq!(bindings.reset(), None);
        assert_eq!(bindings.event("hold", KeyEvent::Released), None);
        assert_eq!(
            bindings.event("toggle", KeyEvent::Pressed),
            Some(Action::Toggle)
        );
    }

    #[test]
    fn reset_does_not_stop_a_toggle_operation() {
        let mut bindings = bindings();
        let _ = bindings.event("toggle", KeyEvent::Pressed);
        assert_eq!(bindings.reset(), None);
    }

    #[test]
    fn invalid_or_duplicate_bindings_are_rejected_without_replacement() {
        let mut bindings = Bindings::default();
        assert!(bindings.insert("  ".into(), BindingMode::Hold).is_err());
        bindings.insert("key".into(), BindingMode::Hold).unwrap();
        assert!(bindings.insert("key".into(), BindingMode::Toggle).is_err());
        assert_eq!(
            bindings.event("key", KeyEvent::Pressed),
            Some(Action::Start)
        );
        let config = Config {
            hold_shortcut: Some("same".into()),
            toggle_shortcut: Some("same".into()),
            ..Config::default()
        };
        assert!(Bindings::from_config(&config).is_err());
    }
}
