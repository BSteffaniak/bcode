//! Session fork/clone flow for the TUI.

use std::io::Write;

use bcode_session_models::{
    SessionEvent, SessionEventKind, SessionHistoryCursor, SessionHistoryDirection,
    SessionHistoryQuery,
};
use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing, ModalTheme};
use bmux_tui_components::text_input::TextInputControl;

use super::helpers;
use super::runtime_context::{TuiIo, TuiServices};
use super::session_flow::{self, ActiveChat};
use super::{TuiError, session_fork_dialog, session_fork_dialog_render};

/// Open the fork dialog for the current session.
pub async fn fork_current_session<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    let source_title = chat
        .app
        .session_title()
        .map_or_else(|| session_id.to_string(), ToString::to_string);
    let mut dialog = session_fork_dialog::SessionForkDialog::new(
        session_fork_dialog::SessionForkDialogMode::Fork,
        &format!("[fork] {source_title}"),
    );
    let submission = run_dialog(io, chat, &mut dialog).await?;
    let Some(prompt) = select_prompt_for_fork(io, services, session_id).await? else {
        chat.app.set_status("fork canceled".to_owned());
        return Ok(());
    };
    let result = services
        .client
        .fork_session(session_id, prompt.sequence, submission.name)
        .await?;
    let draft = result.draft.or(Some(prompt.text));
    if submission.switch_after_create {
        let new_session_id = result.session.id;
        session_flow::switch_session(io.terminal, services.client, chat, new_session_id)?;
        if submission.install_draft {
            if let Some(draft) = draft.as_deref() {
                chat.app.replace_composer_with(draft);
            }
        } else {
            chat.app.replace_composer_with("");
        }
        chat.app
            .set_status("forked session and switched".to_owned());
    } else {
        if submission.install_draft {
            if let Some(draft) = draft.as_deref() {
                chat.app.replace_composer_with(draft);
            }
        } else {
            chat.app.replace_composer_with("");
        }
        chat.app
            .set_status(format!("forked session {}", result.session.id));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ForkPromptCandidate {
    sequence: u64,
    text: String,
}

async fn select_prompt_for_fork<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    session_id: bcode_session_models::SessionId,
) -> Result<Option<ForkPromptCandidate>, TuiError> {
    let prompts = recent_user_prompts(services, session_id).await?;
    if prompts.is_empty() {
        return Ok(None);
    }
    let mut selected = 0_usize;
    loop {
        io.terminal.resize(helpers::terminal_area()?);
        io.terminal
            .draw(|frame| render_prompt_picker(frame, &prompts, selected))?;
        let Some(event) = io.input.recv().await? else {
            continue;
        };
        match event {
            Event::Resize(size) => io.terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Key(stroke) => match stroke.key {
                KeyCode::Escape => return Ok(None),
                KeyCode::Enter => return Ok(prompts.get(selected).cloned()),
                KeyCode::Up if selected > 0 => selected = selected.saturating_sub(1),
                KeyCode::Down if selected + 1 < prompts.len() => selected += 1,
                _ => {}
            },
            Event::Paste(_) | Event::Mouse(_) => {}
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
    }
}

async fn recent_user_prompts(
    services: &TuiServices<'_>,
    session_id: bcode_session_models::SessionId,
) -> Result<Vec<ForkPromptCandidate>, TuiError> {
    let page = services
        .client
        .session_history_page(
            session_id,
            SessionHistoryQuery {
                cursor: Some(SessionHistoryCursor { sequence: u64::MAX }),
                limit: 256,
                direction: SessionHistoryDirection::Backward,
            },
        )
        .await?;
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

fn render_prompt_picker(frame: &mut Frame<'_>, prompts: &[ForkPromptCandidate], selected: usize) {
    let modal = ModalFrame::new(
        ModalSizing::new(Size::new(72, 12), Size::new(96, 18), Insets::all(4)),
        ModalTheme::dark(Color::Cyan),
    )
    .title(" Select fork prompt ")
    .padding(Insets::new(1, 2, 1, 2))
    .placement(ModalPlacement::UpperThird);
    modal.render(frame.area(), frame);
    let content = modal.content_area(frame.area());
    let mut row = content.y;
    render_picker_line(
        frame,
        &modal,
        content,
        &mut row,
        Line::from_spans(vec![Span::styled(
            "Choose the prompt to edit in the forked session",
            Style::new().fg(Color::BrightBlack).bg(Color::Black),
        )]),
    );
    for (index, prompt) in prompts.iter().take(10).enumerate() {
        let selected_style = if index == selected {
            Style::new()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::White).bg(Color::Black)
        };
        render_picker_line(
            frame,
            &modal,
            content,
            &mut row,
            Line::from_spans(vec![
                Span::styled(format!("#{:<4} ", prompt.sequence), selected_style),
                Span::styled(one_line(&prompt.text), selected_style),
            ]),
        );
    }
    render_picker_line(
        frame,
        &modal,
        content,
        &mut row,
        Line::from_spans(vec![
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
    line: Line,
) {
    if *row >= content.bottom() {
        return;
    }
    modal.render_line(Rect::new(content.x, *row, content.width, 1), &line, frame);
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

/// Open the clone dialog for the current session, create the clone, and optionally switch to it.
pub async fn clone_current_session<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    let source_title = chat
        .app
        .session_title()
        .map_or_else(|| session_id.to_string(), ToString::to_string);
    let mut dialog = session_fork_dialog::SessionForkDialog::new(
        session_fork_dialog::SessionForkDialogMode::Clone,
        &format!("[clone] {source_title}"),
    );
    let submission = run_dialog(io, chat, &mut dialog).await?;
    if !submission.install_draft {
        chat.app.replace_composer_with("");
    }
    let result = services
        .client
        .clone_session(session_id, submission.name)
        .await?;
    if submission.switch_after_create {
        let new_session_id = result.session.id;
        session_flow::switch_session(io.terminal, services.client, chat, new_session_id)?;
        chat.app
            .set_status("cloned session and switched".to_owned());
    } else {
        chat.app.apply_session_summary(&result.session);
        chat.app
            .set_status(format!("cloned session {}", result.session.id));
    }
    Ok(())
}

async fn run_dialog<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    chat: &mut ActiveChat,
    dialog: &mut session_fork_dialog::SessionForkDialog,
) -> Result<session_fork_dialog::SessionForkDialogSubmission, TuiError> {
    loop {
        io.terminal.resize(helpers::terminal_area()?);
        io.terminal
            .draw(|frame| session_fork_dialog_render::render_dialog(dialog, frame))?;
        let Some(event) = io.input.recv().await? else {
            continue;
        };
        match event {
            Event::Resize(size) => io.terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text)
                if dialog.focus() == session_fork_dialog::SessionForkDialogFocus::Name =>
            {
                let _ = TextInputControl::new(&session_fork_dialog::name_input_policy())
                    .handle_paste(dialog.name_mut(), &text);
            }
            Event::Paste(_) => {}
            Event::Key(stroke) => match stroke.key {
                KeyCode::Escape => return Err(TuiError::Canceled),
                KeyCode::Tab => dialog.focus_next(),
                KeyCode::Enter => return Ok(dialog.submission()),
                KeyCode::Left => dialog.value_previous(),
                KeyCode::Right => dialog.value_next(),
                _ if dialog.focus() == session_fork_dialog::SessionForkDialogFocus::Name => {
                    let _ = TextInputControl::new(&session_fork_dialog::name_input_policy())
                        .handle_key(dialog.name_mut(), stroke);
                }
                _ => {}
            },
            Event::Mouse(_) => {}
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
        let _ = &chat;
    }
}
