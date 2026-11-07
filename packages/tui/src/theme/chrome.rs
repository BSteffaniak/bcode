//! Terminal adaptation of resolved theme chrome.

use bcode_tui_theme::chrome::{BorderStyle, FocusEffect, TuiChrome};
use bcode_tui_theme::definition::ResolvedThemeDefinition;
use bmux_tui::chrome::BorderSet;
use bmux_tui::gradient::ColorGradient;
use bmux_tui_components::theme::ComponentChrome;

/// BMUX glyph set for a theme border style; `None` for no border or a
/// leading-bar-only style that has no panel glyph set.
#[must_use]
pub const fn border_set(style: BorderStyle) -> Option<BorderSet> {
    match style {
        BorderStyle::None | BorderStyle::Left => None,
        BorderStyle::Ascii => Some(BorderSet::ASCII),
        BorderStyle::Single => Some(BorderSet::SINGLE),
        BorderStyle::Rounded => Some(BorderSet::ROUNDED),
        BorderStyle::Thick => Some(BorderSet::THICK),
        BorderStyle::Double => Some(BorderSet::DOUBLE),
    }
}

/// Focus gradient for the chrome, when its effect and colors allow one.
///
/// Gradients need two explicit RGB endpoints; terminal-owned colors keep the
/// focused border style instead of guessing RGB values.
#[must_use]
pub fn focus_gradient(chrome: &TuiChrome) -> Option<ColorGradient> {
    if chrome.focus == FocusEffect::None {
        return None;
    }
    let mut colors = chrome.focus_colors.iter().filter_map(|color| color.rgb());
    let from = colors.next()?;
    let to = colors.next()?;
    Some(ColorGradient::new(from, to))
}

/// Chrome for one surface: an unset style keeps the component's default glyphs.
fn surface_chrome(style: Option<BorderStyle>, chrome: &TuiChrome) -> ComponentChrome {
    ComponentChrome {
        border_set: style.and_then(border_set),
        focus_gradient: focus_gradient(chrome),
    }
}

/// Chrome for panels, panes, pickers, and inputs.
#[must_use]
pub fn component_chrome(theme: &ResolvedThemeDefinition) -> ComponentChrome {
    surface_chrome(theme.chrome.borders.panel, &theme.chrome)
}

/// Chrome for dialogs and modals.
#[must_use]
pub fn dialog_chrome(theme: &ResolvedThemeDefinition) -> ComponentChrome {
    surface_chrome(theme.chrome.borders.dialog, &theme.chrome)
}

#[cfg(test)]
mod tests {
    use bcode_tui_theme::chrome::TuiChrome;
    use bmux_tui::style::Color;

    use super::*;

    #[test]
    fn gradients_need_two_rgb_colors_and_an_effect() {
        let mut chrome = TuiChrome {
            focus: FocusEffect::Gradient,
            focus_colors: vec![
                Color::Rgb(255, 0, 170),
                Color::Cyan,
                Color::Rgb(0, 255, 255),
            ],
            ..TuiChrome::default()
        };
        let gradient = focus_gradient(&chrome).expect("two rgb colors");
        assert_eq!(gradient.from, bmux_color::Rgb::new(255, 0, 170));
        assert_eq!(gradient.to, bmux_color::Rgb::new(0, 255, 255));
        chrome.focus_colors = vec![Color::Cyan, Color::Magenta];
        assert_eq!(focus_gradient(&chrome), None);
        chrome.focus = FocusEffect::None;
        chrome.focus_colors = vec![Color::Rgb(1, 1, 1), Color::Rgb(2, 2, 2)];
        assert_eq!(focus_gradient(&chrome), None);
    }

    #[test]
    fn themes_without_chrome_keep_component_defaults() {
        let theme = ResolvedThemeDefinition::empty("plain", "Plain");
        assert_eq!(component_chrome(&theme), ComponentChrome::DEFAULT);
        assert_eq!(dialog_chrome(&theme), ComponentChrome::DEFAULT);
    }

    #[test]
    fn border_styles_map_to_bmux_glyph_sets() {
        assert_eq!(border_set(BorderStyle::Rounded), Some(BorderSet::ROUNDED));
        assert_eq!(border_set(BorderStyle::None), None);
        assert_eq!(border_set(BorderStyle::Left), None);
    }
}
