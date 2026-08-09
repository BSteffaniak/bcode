//! Bcode setup/onboarding presentation recipes.

use bmux_tui::prelude::{Modifier, Style};
use bmux_tui_components::theme::ComponentTheme;

/// Semantic setup location state, independent of setup workflow data types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupSpotState {
    Complete,
    Current,
    Recommended,
    Blocked,
    Visited,
    Inactive,
}

/// Resolve a setup spot's semantic style and interaction emphasis.
#[must_use]
pub const fn setup_spot_style(
    state: SetupSpotState,
    focused: bool,
    hovered: bool,
    pressed: bool,
    theme: ComponentTheme,
) -> Style {
    let base = match state {
        SetupSpotState::Complete => theme.success,
        SetupSpotState::Current => theme.info.add_modifier(Modifier::BOLD),
        SetupSpotState::Recommended => theme.warning,
        SetupSpotState::Blocked => theme.error.add_modifier(Modifier::BOLD),
        SetupSpotState::Visited => theme.focused,
        SetupSpotState::Inactive => theme.muted,
    };
    if pressed {
        base.add_modifier(Modifier::REVERSED)
    } else if focused || hovered {
        base.add_modifier(Modifier::BOLD)
    } else {
        base
    }
}

/// Resolve the setup-board path style.
#[must_use]
pub const fn setup_path_style(theme: ComponentTheme) -> Style {
    theme.muted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressed_state_wins_over_focus() {
        let style = setup_spot_style(
            SetupSpotState::Current,
            true,
            false,
            true,
            ComponentTheme::default(),
        );
        assert!(style.modifiers.contains(Modifier::REVERSED));
    }
}
