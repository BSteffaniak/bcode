//! Rendering for the onboarding setup-map shell.

use bcode_settings::{SettingsDbHealth, SetupReadinessReport};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

use super::onboarding::OnboardingShell;

/// Render the onboarding shell into a terminal frame.
pub fn render_onboarding(
    shell: &OnboardingShell,
    frame: &mut Frame<'_>,
    health: &SettingsDbHealth,
    readiness: Option<SetupReadinessReport>,
) {
    let area = frame.area();
    let model = shell.render_model(health, readiness);
    let title = Line::from_spans(vec![
        Span::styled(
            " Bcode Setup Map ",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  secure, flexible, ready to launch"),
    ]);
    frame.write_line_with_fallback_style(
        Rect::new(
            area.x.saturating_add(2),
            area.y.saturating_add(1),
            area.width.saturating_sub(4),
            1,
        ),
        &title,
        Style::new(),
    );

    let mut y = area.y.saturating_add(3);
    for line in model.map_lines {
        if y >= area.y.saturating_add(area.height).saturating_sub(4) {
            break;
        }
        let style = style_for_line(&line);
        frame.write_line_with_fallback_style(
            Rect::new(area.x.saturating_add(4), y, area.width.saturating_sub(8), 1),
            &Line::from_spans(vec![Span::styled(line, style)]),
            Style::new(),
        );
        y = y.saturating_add(1);
    }

    if let Some(panel) = model.degraded_panel {
        y = y.saturating_add(1);
        frame.write_line_with_fallback_style(
            Rect::new(area.x.saturating_add(4), y, area.width.saturating_sub(8), 1),
            &Line::from_spans(vec![Span::styled(
                panel.message,
                Style::new().fg(Color::Yellow),
            )]),
            Style::new(),
        );
    }

    let footer_y = area.y.saturating_add(area.height).saturating_sub(3);
    for (offset, footer) in model.footer_lines.iter().take(2).enumerate() {
        frame.write_line_with_fallback_style(
            Rect::new(
                area.x.saturating_add(2),
                footer_y.saturating_add(u16::try_from(offset).unwrap_or(0)),
                area.width.saturating_sub(4),
                1,
            ),
            &Line::from_spans(vec![Span::styled(
                footer.clone(),
                Style::new().fg(Color::BrightBlack),
            )]),
            Style::new(),
        );
    }
}

fn style_for_line(line: &str) -> Style {
    if line.contains("[current]") {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else if line.contains("[complete]") || line.contains("[secured]") {
        Style::new().fg(Color::Green)
    } else if line.contains("[blocked]") || line.contains("[needs_attention]") {
        Style::new().fg(Color::Red)
    } else if line.contains("[recommended]") {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new().fg(Color::White)
    }
}
