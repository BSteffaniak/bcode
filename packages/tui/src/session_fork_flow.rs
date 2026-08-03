//! Session fork/clone flow for the TUI.

use bcode_session_models::{
    SessionEvent, SessionEventKind, SessionHistoryCursor, SessionHistoryDirection,
    SessionHistoryQuery,
};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing, ModalTheme};

use super::TuiError;

#[derive(Clone)]
pub struct ForkPromptCandidate {
    pub sequence: u64,
    pub text: String,
}

pub async fn load_recent_user_prompts(
    client: &bcode_client::BcodeClient,
    session_id: bcode_session_models::SessionId,
) -> Result<Vec<ForkPromptCandidate>, TuiError> {
    let page = match client
        .session_history_page(
            session_id,
            SessionHistoryQuery {
                cursor: Some(SessionHistoryCursor { sequence: u64::MAX }),
                limit: 256,
                direction: SessionHistoryDirection::Backward,
            },
        )
        .await
    {
        Ok(page) => page,
        Err(error) => return Err(error.into()),
    };
    Ok(page
        .events
        .iter()
        .filter_map(user_prompt_candidate_from_event)
        .collect())
}

fn user_prompt_candidate_from_event(event: &SessionEvent) -> Option<ForkPromptCandidate> {
    let SessionEventKind::UserMessage { text, .. } = &event.kind else {
        return None;
    };
    Some(ForkPromptCandidate {
        sequence: event.sequence,
        text: text.clone(),
    })
}

pub fn render_prompt_picker(
    frame: &mut Frame<'_>,
    prompts: &[ForkPromptCandidate],
    selected: usize,
    theme: super::render::TuiTheme,
) {
    let modal = prompt_picker_modal(theme);
    modal.render(frame.area(), frame);
    let content = modal.content_area(frame.area());
    let mut row = content.y;
    render_picker_line(
        frame,
        &modal,
        content,
        &mut row,
        &Line::from_spans(vec![Span::styled(
            "Choose the prompt to edit in the forked session",
            Style::new().fg(Color::BrightBlack).bg(Color::Black),
        )]),
    );
    for (index, prompt) in prompts.iter().take(10).enumerate() {
        render_picker_prompt_line(frame, &modal, content, &mut row, prompt, index == selected);
    }
    render_picker_help(frame, &modal, content, &mut row);
}

fn prompt_picker_modal(theme: super::render::TuiTheme) -> ModalFrame {
    ModalFrame::new(
        ModalSizing::new(Size::new(72, 12), Size::new(96, 18), Insets::all(4)),
        ModalTheme::dark(theme.accent),
    )
    .title(" Select fork prompt ")
    .padding(Insets::new(1, 2, 1, 2))
    .placement(ModalPlacement::UpperThird)
}

fn render_picker_prompt_line(
    frame: &mut Frame<'_>,
    modal: &ModalFrame,
    content: Rect,
    row: &mut u16,
    prompt: &ForkPromptCandidate,
    selected: bool,
) {
    let selected_style = if selected {
        Style::new()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::White).bg(Color::Black)
    };
    render_picker_line(
        frame,
        modal,
        content,
        row,
        &Line::from_spans(vec![
            Span::styled(format!("#{:<4} ", prompt.sequence), selected_style),
            Span::styled(one_line(&prompt.text), selected_style),
        ]),
    );
}

fn render_picker_help(frame: &mut Frame<'_>, modal: &ModalFrame, content: Rect, row: &mut u16) {
    render_picker_line(
        frame,
        modal,
        content,
        row,
        &Line::from_spans(vec![
            Span::styled(
                "Enter",
                Style::new().add_modifier(Modifier::BOLD).bg(Color::Black),
            ),
            Span::styled(" select  ", Style::new().bg(Color::Black)),
            Span::styled(
                "↑/↓",
                Style::new().add_modifier(Modifier::BOLD).bg(Color::Black),
            ),
            Span::styled(" move  ", Style::new().bg(Color::Black)),
            Span::styled(
                "Esc",
                Style::new().add_modifier(Modifier::BOLD).bg(Color::Black),
            ),
            Span::styled(" cancel", Style::new().bg(Color::Black)),
        ]),
    );
}

fn render_picker_line(
    frame: &mut Frame<'_>,
    modal: &ModalFrame,
    content: Rect,
    row: &mut u16,
    line: &Line,
) {
    if *row >= content.bottom() {
        return;
    }
    modal.render_line(Rect::new(content.x, *row, content.width, 1), line, frame);
    *row = row.saturating_add(1);
}

fn one_line(text: &str) -> String {
    const MAX_CHARS: usize = 80;
    let mut output = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if output.chars().count() > MAX_CHARS {
        output = output.chars().take(MAX_CHARS).collect::<String>();
        output.push('…');
    }
    output
}
