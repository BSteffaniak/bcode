//! Theme resolution and presentation state.

use std::num::ParseIntError;

use bmux_tui::style::Color;

use super::app::BmuxApp;

/// Fully resolved target theme derived from app state and configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedTheme {
    /// Target accent color for chrome, dialogs, and selection affordances.
    pub accent: Color,
}

/// Theme currently presented by the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentedTheme {
    /// Presented accent color for chrome, dialogs, and selection affordances.
    pub accent: Color,
}

impl From<ResolvedTheme> for PresentedTheme {
    fn from(theme: ResolvedTheme) -> Self {
        Self {
            accent: theme.accent,
        }
    }
}

/// Neutral accent shown before daemon-backed agent metadata has loaded.
pub const PENDING_AGENT_METADATA_ACCENT: Color = Color::Rgb(100, 116, 139);

/// Resolve the target theme from app state.
#[must_use]
pub fn resolve_theme(app: &BmuxApp) -> ResolvedTheme {
    ResolvedTheme {
        accent: target_agent_accent(
            app.display_agent_id(),
            app.display_agent_accent(),
            app.is_agent_metadata_hydrated(),
        ),
    }
}

/// Resolve an agent accent from explicit metadata, hydration state, and fallback palette.
#[must_use]
pub fn target_agent_accent(
    agent_id: &str,
    configured_accent: Option<&str>,
    agent_metadata_hydrated: bool,
) -> Color {
    configured_accent
        .and_then(parse_agent_accent_color)
        .unwrap_or_else(|| {
            if agent_metadata_hydrated {
                fallback_agent_accent_color(agent_id)
            } else {
                PENDING_AGENT_METADATA_ACCENT
            }
        })
}

fn parse_agent_accent_color(accent: &str) -> Option<Color> {
    let hex = accent.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let [red, green, blue] = parse_hex_rgb(hex).ok()?;
    Some(Color::Rgb(red, green, blue))
}

fn parse_hex_rgb(hex: &str) -> Result<[u8; 3], ParseIntError> {
    Ok([
        u8::from_str_radix(&hex[0..2], 16)?,
        u8::from_str_radix(&hex[2..4], 16)?,
        u8::from_str_radix(&hex[4..6], 16)?,
    ])
}

fn fallback_agent_accent_color(agent_id: &str) -> Color {
    const PALETTE: [Color; 6] = [
        Color::Cyan,
        Color::Rgb(167, 139, 250),
        Color::Rgb(52, 211, 153),
        Color::Rgb(245, 158, 11),
        Color::Rgb(96, 165, 250),
        Color::Rgb(244, 114, 182),
    ];
    let hash = agent_id.bytes().fold(0_usize, |hash, byte| {
        hash.wrapping_mul(33).wrapping_add(usize::from(byte))
    });
    PALETTE[hash % PALETTE.len()]
}
