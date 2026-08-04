//! Rendering for the onboarding setup-map shell.

use bcode_settings::{SettingsDbHealth, SetupReadinessReport};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;

use super::onboarding::OnboardingShell;
use super::theme::PresentedTheme;

const HERO_HEIGHT: u16 = 4;

/// Return the board viewport area for a terminal area.
#[must_use]
pub const fn onboarding_board_area(area: Rect) -> Rect {
    let content_y = area.y.saturating_add(HERO_HEIGHT).saturating_add(1);
    let footer_height = 3;
    let content_height = area
        .height
        .saturating_sub(HERO_HEIGHT)
        .saturating_sub(footer_height)
        .saturating_sub(2);
    let map_width = area.width.saturating_mul(45) / 100;
    Rect::new(
        area.x.saturating_add(3),
        content_y.saturating_add(1),
        map_width.saturating_sub(5),
        content_height.saturating_sub(2),
    )
}

/// Render the onboarding shell into a terminal frame.
pub fn render_onboarding(
    shell: &OnboardingShell,
    frame: &mut Frame<'_>,
    health: &SettingsDbHealth,
    readiness: Option<SetupReadinessReport>,
    theme: &PresentedTheme,
) {
    let area = frame.area();
    let model = shell.render_model(health, readiness);
    render_hero_panel(area, frame, theme);

    let content_y = area.y.saturating_add(HERO_HEIGHT).saturating_add(1);
    let footer_height = 3;
    let content_height = area
        .height
        .saturating_sub(HERO_HEIGHT)
        .saturating_sub(footer_height)
        .saturating_sub(2);
    let map_width = area.width.saturating_mul(45) / 100;
    let map_area = Rect::new(
        area.x.saturating_add(2),
        content_y,
        map_width.saturating_sub(3),
        content_height,
    );
    let detail_area = Rect::new(
        area.x.saturating_add(map_width).saturating_add(1),
        content_y,
        area.width.saturating_sub(map_width).saturating_sub(3),
        content_height,
    );

    let board_area = onboarding_board_area(area);
    render_setup_map_panel(shell, board_area, map_area, frame, theme);
    render_detail_panel(
        &model.focused_detail.title,
        &model.focused_detail.story,
        &model.focused_detail.status,
        &model.focused_detail.actions,
        detail_area,
        frame,
        theme,
    );

    if let Some(panel) = model.degraded_panel {
        let degraded_y = area.y.saturating_add(area.height).saturating_sub(5);
        render_status_line(&panel.message, degraded_y, area, theme.warning, frame);
    }

    let footer_y = area.y.saturating_add(area.height).saturating_sub(3);
    for (offset, footer) in model.footer_lines.iter().take(2).enumerate() {
        render_status_line(
            footer,
            footer_y.saturating_add(u16::try_from(offset).unwrap_or(0)),
            area,
            theme.muted,
            frame,
        );
    }
    if let Some(confirmation) = model.pending_confirmation {
        render_confirmation_modal(&confirmation.title, &confirmation.body, area, frame, theme);
    }
}

fn render_hero_panel(area: Rect, frame: &mut Frame<'_>, theme: &PresentedTheme) {
    let hero = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        HERO_HEIGHT,
    );
    render_box(hero, "Base Camp", theme.focused, frame);
    frame.write_line_with_fallback_style(
        Rect::new(
            hero.x.saturating_add(2),
            hero.y.saturating_add(1),
            hero.width.saturating_sub(4),
            1,
        ),
        &Line::from_spans(vec![
            Span::styled(
                "Bcode Setup Map",
                theme.focused.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  · secure vaults, clear choices, clean launch",
                theme.muted,
            ),
        ]),
        Style::new(),
    );
    frame.write_line_with_fallback_style(
        Rect::new(
            hero.x.saturating_add(2),
            hero.y.saturating_add(2),
            hero.width.saturating_sub(4),
            1,
        ),
        &Line::from_spans(vec![Span::styled(
            "Move through the quest board, review what will change, then launch when ready.",
            theme.text,
        )]),
        Style::new(),
    );
}

fn render_setup_map_panel(
    shell: &OnboardingShell,
    board_area: Rect,
    panel_area: Rect,
    frame: &mut Frame<'_>,
    theme: &PresentedTheme,
) {
    render_box(
        panel_area,
        "Quest Board · drag / arrows to pan",
        theme.border,
        frame,
    );
    shell.render_board(board_area, frame, theme);
}

fn render_confirmation_modal(
    title: &str,
    body: &str,
    area: Rect,
    frame: &mut Frame<'_>,
    theme: &PresentedTheme,
) {
    let modal_width = area.width.saturating_mul(2) / 3;
    let modal_height = 6;
    let modal_x = area
        .x
        .saturating_add(area.width.saturating_sub(modal_width) / 2);
    let modal_y = area
        .y
        .saturating_add(area.height.saturating_sub(modal_height) / 2);
    let modal = Rect::new(modal_x, modal_y, modal_width, modal_height);
    render_box(modal, title, theme.warning, frame);
    let lines = [body, "Press y to confirm, n or Esc to cancel."];
    for (offset, line) in lines.iter().enumerate() {
        frame.write_line_with_fallback_style(
            Rect::new(
                modal.x.saturating_add(2),
                modal
                    .y
                    .saturating_add(1)
                    .saturating_add(u16::try_from(offset).unwrap_or(0)),
                modal.width.saturating_sub(4),
                1,
            ),
            &Line::from_spans(vec![Span::styled(
                (*line).to_owned(),
                theme.warning.add_modifier(Modifier::BOLD),
            )]),
            Style::new(),
        );
    }
}

fn render_detail_panel(
    title: &str,
    story: &str,
    status: &str,
    actions: &[String],
    area: Rect,
    frame: &mut Frame<'_>,
    theme: &PresentedTheme,
) {
    render_box(area, "Story Card", theme.border, frame);
    let lines = [
        format!("◇ {title}"),
        format!("status: {}", status_badge(status)),
        story.to_owned(),
        "actions:".to_owned(),
    ];
    let mut y = area.y.saturating_add(1);
    for line in lines {
        if y >= area.y.saturating_add(area.height).saturating_sub(1) {
            return;
        }
        frame.write_line_with_fallback_style(
            Rect::new(area.x.saturating_add(2), y, area.width.saturating_sub(4), 1),
            &Line::from_spans(vec![Span::styled(line, theme.text)]),
            Style::new(),
        );
        y = y.saturating_add(1);
    }
    for action in actions {
        if y >= area.y.saturating_add(area.height).saturating_sub(1) {
            return;
        }
        frame.write_line_with_fallback_style(
            Rect::new(area.x.saturating_add(4), y, area.width.saturating_sub(6), 1),
            &Line::from_spans(vec![Span::styled(format!("• {action}"), theme.info)]),
            Style::new(),
        );
        y = y.saturating_add(1);
    }
}

fn render_status_line(text: &str, y: u16, area: Rect, style: Style, frame: &mut Frame<'_>) {
    frame.write_line_with_fallback_style(
        Rect::new(area.x.saturating_add(2), y, area.width.saturating_sub(4), 1),
        &Line::from_spans(vec![Span::styled(text.to_owned(), style)]),
        Style::new(),
    );
}

fn render_box(area: Rect, title: &str, style: Style, frame: &mut Frame<'_>) {
    if area.width < 4 || area.height < 2 {
        return;
    }
    let horizontal = "─".repeat(usize::from(area.width.saturating_sub(2)));
    let top = format!("╭{horizontal}╮");
    let bottom = format!("╰{horizontal}╯");
    frame.write_line_with_fallback_style(
        Rect::new(area.x, area.y, area.width, 1),
        &Line::from_spans(vec![Span::styled(top, style)]),
        Style::new(),
    );
    frame.write_line_with_fallback_style(
        Rect::new(
            area.x.saturating_add(2),
            area.y,
            area.width.saturating_sub(4),
            1,
        ),
        &Line::from_spans(vec![Span::styled(
            format!(" {title} "),
            style.add_modifier(Modifier::BOLD),
        )]),
        Style::new(),
    );
    for y in area.y.saturating_add(1)..area.y.saturating_add(area.height).saturating_sub(1) {
        frame.write_line_with_fallback_style(
            Rect::new(area.x, y, 1, 1),
            &Line::from_spans(vec![Span::styled("│", style)]),
            Style::new(),
        );
        frame.write_line_with_fallback_style(
            Rect::new(area.x.saturating_add(area.width).saturating_sub(1), y, 1, 1),
            &Line::from_spans(vec![Span::styled("│", style)]),
            Style::new(),
        );
    }
    frame.write_line_with_fallback_style(
        Rect::new(
            area.x,
            area.y.saturating_add(area.height).saturating_sub(1),
            area.width,
            1,
        ),
        &Line::from_spans(vec![Span::styled(bottom, style)]),
        Style::new(),
    );
}

fn status_badge(status: &str) -> String {
    format!("⟦{status}⟧")
}
