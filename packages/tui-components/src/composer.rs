//! Bcode composer presentation recipe.

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::geometry::Insets;
use bmux_tui::prelude::Style;

/// Semantic styles for Bcode's message composer shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComposerStyle {
    /// Focused composer border.
    pub border: Style,
    /// Raised composer surface.
    pub surface: Style,
}

/// Build the Bcode message composer panel.
#[must_use]
pub fn composer_panel(style: ComposerStyle) -> Panel {
    Panel::new()
        .border(Border::single().style(style.border))
        .title(" Message ")
        .padding(Insets::new(0, 1, 0, 1))
        .background(style.surface)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::geometry::Rect;

    #[test]
    fn composer_panel_preserves_one_cell_horizontal_padding() {
        let panel = composer_panel(ComposerStyle {
            border: Style::new(),
            surface: Style::new(),
        });
        assert_eq!(
            panel.inner_area(Rect::new(0, 0, 20, 5)),
            Rect::new(2, 1, 16, 3)
        );
    }
}
