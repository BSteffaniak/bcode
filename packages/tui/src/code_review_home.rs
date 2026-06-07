//! Code review workspace home/picker TUI.

use std::io::Write;
use std::path::PathBuf;

use bcode_client::BcodeClient;
use bcode_code_review_models::{
    ArchiveReviewWorkspaceRequest, ArchiveReviewWorkspaceResponse,
    CODE_REVIEW_SERVICE_INTERFACE_ID, CreateReviewWorkspaceRequest, CreateReviewWorkspaceResponse,
    ListReviewWorkspacesRequest, ListReviewWorkspacesResponse, OP_REVIEW_WORKSPACE_ARCHIVE,
    OP_REVIEW_WORKSPACE_CREATE, OP_REVIEW_WORKSPACE_LIST, OP_REVIEW_WORKSPACE_UPDATE,
    ReviewWorkspace, UpdateReviewWorkspaceRequest, UpdateReviewWorkspaceResponse,
};
use bmux_keyboard::KeyCode;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui::terminal::Terminal;

use super::terminal_events::TuiInput;
use super::{TuiError, helpers};

/// Review home outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewHomeOutcome {
    /// Open an existing or newly created review target.
    OpenWorkspace {
        /// Review workspace to open.
        workspace: ReviewWorkspace,
    },
    /// Exit without opening a review.
    Exit,
}

#[derive(Debug, Clone)]
struct ReviewHomeApp {
    repo_path: PathBuf,
    workspaces: Vec<ReviewWorkspace>,
    selected: usize,
    status_message: Option<String>,
    search_query: String,
    search_active: bool,
    rename_buffer: Option<String>,
    should_exit: bool,
    outcome: Option<ReviewHomeOutcome>,
}

impl ReviewHomeApp {
    const fn new(repo_path: PathBuf, workspaces: Vec<ReviewWorkspace>) -> Self {
        Self {
            repo_path,
            workspaces,
            selected: 0,
            status_message: None,
            search_query: String::new(),
            search_active: false,
            rename_buffer: None,
            should_exit: false,
            outcome: None,
        }
    }

    fn visible_indices(&self) -> Vec<usize> {
        let query = self.search_query.trim().to_ascii_lowercase();
        self.workspaces
            .iter()
            .enumerate()
            .filter_map(|(index, workspace)| {
                if query.is_empty()
                    || workspace.title.to_ascii_lowercase().contains(&query)
                    || workspace
                        .repo_root
                        .display()
                        .to_string()
                        .to_ascii_lowercase()
                        .contains(&query)
                {
                    Some(index)
                } else {
                    None
                }
            })
            .collect()
    }

    fn selected_workspace_index(&self) -> Option<usize> {
        self.visible_indices().get(self.selected).copied()
    }

    fn clamp_selection(&mut self) {
        self.selected = self
            .selected
            .min(self.visible_indices().len().saturating_sub(1));
    }

    fn move_down(&mut self) -> bool {
        let next = self
            .selected
            .saturating_add(1)
            .min(self.visible_indices().len().saturating_sub(1));
        if next == self.selected {
            return false;
        }
        self.selected = next;
        true
    }

    const fn move_up(&mut self) -> bool {
        let next = self.selected.saturating_sub(1);
        if next == self.selected {
            return false;
        }
        self.selected = next;
        true
    }

    fn open_workspace(&mut self, workspace: ReviewWorkspace) -> bool {
        self.outcome = Some(ReviewHomeOutcome::OpenWorkspace { workspace });
        self.should_exit = true;
        true
    }

    fn open_selected(&mut self) -> bool {
        let Some(workspace_index) = self.selected_workspace_index() else {
            self.status_message =
                Some("no matching review workspace selected; press n to create one".to_string());
            return true;
        };
        let Some(workspace) = self.workspaces.get(workspace_index) else {
            self.status_message = Some("selected review workspace is unavailable".to_string());
            return true;
        };
        self.open_workspace(workspace.clone())
    }
    fn start_rename(&mut self) -> bool {
        let Some(workspace_index) = self.selected_workspace_index() else {
            self.status_message = Some("no matching review workspace selected".to_string());
            return true;
        };
        let Some(workspace) = self.workspaces.get(workspace_index) else {
            self.status_message = Some("selected review workspace is unavailable".to_string());
            return true;
        };
        self.rename_buffer = Some(workspace.title.clone());
        self.status_message = Some("rename review".to_string());
        true
    }

    fn cancel_rename(&mut self) -> bool {
        if self.rename_buffer.is_none() {
            return false;
        }
        self.rename_buffer = None;
        self.status_message = Some("rename canceled".to_string());
        true
    }

    fn handle_rename_key(&mut self, key: KeyCode) -> ReviewHomeKeyOutcome {
        match key {
            KeyCode::Char(ch) => {
                self.rename_buffer
                    .as_mut()
                    .map_or(ReviewHomeKeyOutcome::Ignored, |buffer| {
                        buffer.push(ch);
                        ReviewHomeKeyOutcome::Redraw
                    })
            }
            KeyCode::Backspace => {
                self.rename_buffer
                    .as_mut()
                    .map_or(ReviewHomeKeyOutcome::Ignored, |buffer| {
                        buffer.pop();
                        ReviewHomeKeyOutcome::Redraw
                    })
            }
            KeyCode::Enter => ReviewHomeKeyOutcome::SubmitRename,
            KeyCode::Escape => {
                self.cancel_rename();
                ReviewHomeKeyOutcome::Redraw
            }
            _ => ReviewHomeKeyOutcome::Ignored,
        }
    }

    fn toggle_search(&mut self) -> bool {
        self.search_active = !self.search_active;
        self.status_message = Some(if self.search_active {
            "search reviews".to_string()
        } else {
            "search closed".to_string()
        });
        true
    }

    fn clear_search(&mut self) -> bool {
        if self.search_query.is_empty() && !self.search_active {
            return false;
        }
        self.search_query.clear();
        self.search_active = false;
        self.selected = 0;
        self.status_message = Some("search cleared".to_string());
        true
    }

    fn handle_search_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Char(ch) => {
                self.search_query.push(ch);
                self.selected = 0;
                true
            }
            KeyCode::Backspace => {
                self.search_query.pop();
                self.clamp_selection();
                true
            }
            KeyCode::Enter => {
                self.search_active = false;
                self.open_selected()
            }
            KeyCode::Escape => self.clear_search(),
            _ => false,
        }
    }
}

enum ReviewHomeKeyOutcome {
    Redraw,
    SubmitRename,
    Ignored,
}

/// Run the review home/picker.
///
/// # Errors
///
/// Returns an error when review workspaces cannot be loaded or terminal I/O fails.
pub async fn run<W: Write>(
    terminal: &mut Terminal<&mut W>,
    repo_path: PathBuf,
) -> Result<ReviewHomeOutcome, TuiError> {
    let client = BcodeClient::default_endpoint();
    let mut app = match load_workspaces(&client, repo_path.clone()).await {
        Ok(workspaces) => ReviewHomeApp::new(repo_path, workspaces),
        Err(error) => {
            let mut app = ReviewHomeApp::new(repo_path, Vec::new());
            app.status_message = Some(format!("failed to load workspaces: {error}"));
            app
        }
    };
    let mut input = TuiInput::start();
    let mut needs_redraw = true;

    while !app.should_exit {
        if helpers::resize_from_terminal(terminal)? {
            needs_redraw = true;
        }
        if needs_redraw {
            terminal.draw(|frame| render(&app, frame))?;
            needs_redraw = false;
        }
        let Some(event) = input.recv().await? else {
            continue;
        };
        if let Event::Key(key) = event {
            needs_redraw = if app.rename_buffer.is_some() {
                match app.handle_rename_key(key.key) {
                    ReviewHomeKeyOutcome::Redraw => true,
                    ReviewHomeKeyOutcome::SubmitRename => {
                        match submit_rename_workspace(&client, &mut app).await {
                            Ok(redraw) => redraw,
                            Err(error) => {
                                app.status_message =
                                    Some(format!("failed to rename workspace: {error}"));
                                true
                            }
                        }
                    }
                    ReviewHomeKeyOutcome::Ignored => false,
                }
            } else if app.search_active {
                app.handle_search_key(key.key)
            } else {
                match key.key {
                    KeyCode::Char('q') | KeyCode::Escape => {
                        app.outcome = Some(ReviewHomeOutcome::Exit);
                        app.should_exit = true;
                        true
                    }
                    KeyCode::Char('/') => app.toggle_search(),
                    KeyCode::Char('r') => app.start_rename(),
                    KeyCode::Char('j') | KeyCode::Down => app.move_down(),
                    KeyCode::Char('k') | KeyCode::Up => app.move_up(),
                    KeyCode::Enter => app.open_selected(),
                    KeyCode::Char('x') => match archive_selected_workspace(&client, &mut app).await
                    {
                        Ok(archived) => archived,
                        Err(error) => {
                            app.status_message =
                                Some(format!("failed to archive workspace: {error}"));
                            true
                        }
                    },
                    KeyCode::Char('n') => {
                        match create_workspace(&client, app.repo_path.clone()).await {
                            Ok(workspace) => app.open_workspace(workspace),
                            Err(error) => {
                                app.status_message =
                                    Some(format!("failed to create workspace: {error}"));
                                true
                            }
                        }
                    }
                    _ => false,
                }
            };
        }
    }

    Ok(app.outcome.unwrap_or(ReviewHomeOutcome::Exit))
}

async fn load_workspaces(
    client: &BcodeClient,
    repo_path: PathBuf,
) -> Result<Vec<ReviewWorkspace>, TuiError> {
    let payload = serde_json::to_vec(&ListReviewWorkspacesRequest {
        repo_path,
        include_archived: false,
    })
    .map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            CODE_REVIEW_SERVICE_INTERFACE_ID.to_string(),
            OP_REVIEW_WORKSPACE_LIST.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let response: ListReviewWorkspacesResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    Ok(response.workspaces)
}

async fn submit_rename_workspace(
    client: &BcodeClient,
    app: &mut ReviewHomeApp,
) -> Result<bool, TuiError> {
    let Some(new_title) = app.rename_buffer.take() else {
        return Ok(false);
    };
    let new_title = new_title.trim().to_string();
    if new_title.is_empty() {
        app.status_message = Some("review title cannot be empty".to_string());
        return Ok(true);
    }
    let Some(workspace_index) = app.selected_workspace_index() else {
        app.status_message = Some("no matching review workspace selected".to_string());
        return Ok(true);
    };
    let Some(existing) = app.workspaces.get(workspace_index) else {
        app.status_message = Some("selected review workspace is unavailable".to_string());
        return Ok(true);
    };
    let mut workspace = existing.clone();
    workspace.title = new_title.clone();
    let payload = serde_json::to_vec(&UpdateReviewWorkspaceRequest {
        repo_path: app.repo_path.clone(),
        workspace,
    })
    .map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            CODE_REVIEW_SERVICE_INTERFACE_ID.to_string(),
            OP_REVIEW_WORKSPACE_UPDATE.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let response: UpdateReviewWorkspaceResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    app.workspaces[workspace_index] = response.workspace;
    app.status_message = Some(format!("renamed review to {new_title}"));
    Ok(true)
}

async fn archive_selected_workspace(
    client: &BcodeClient,
    app: &mut ReviewHomeApp,
) -> Result<bool, TuiError> {
    let Some(workspace_index) = app.selected_workspace_index() else {
        app.status_message = Some("no matching review workspace selected".to_string());
        return Ok(true);
    };
    let Some(workspace) = app.workspaces.get(workspace_index) else {
        app.status_message = Some("no review workspace selected".to_string());
        return Ok(true);
    };
    let payload = serde_json::to_vec(&ArchiveReviewWorkspaceRequest {
        repo_path: app.repo_path.clone(),
        workspace_id: workspace.id.clone(),
    })
    .map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            CODE_REVIEW_SERVICE_INTERFACE_ID.to_string(),
            OP_REVIEW_WORKSPACE_ARCHIVE.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let response: ArchiveReviewWorkspaceResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    if response.archived {
        let archived = app.workspaces.remove(workspace_index);
        app.clamp_selection();
        app.status_message = Some(format!("archived {}", archived.title));
    } else {
        app.status_message = Some("workspace was not found".to_string());
    }
    Ok(true)
}

async fn create_workspace(
    client: &BcodeClient,
    repo_path: PathBuf,
) -> Result<ReviewWorkspace, TuiError> {
    let payload = serde_json::to_vec(&CreateReviewWorkspaceRequest {
        repo_path,
        title: Some("Untitled review".to_string()),
        sources: Vec::new(),
    })
    .map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            CODE_REVIEW_SERVICE_INTERFACE_ID.to_string(),
            OP_REVIEW_WORKSPACE_CREATE.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let response: CreateReviewWorkspaceResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    Ok(response.workspace)
}

fn render(app: &ReviewHomeApp, frame: &mut Frame<'_>) {
    let area = frame.area();
    frame.fill(area, " ", Style::new().fg(Color::White).bg(Color::Black));
    render_header(area, frame);
    let body = Rect::new(
        area.x,
        area.y.saturating_add(2),
        area.width,
        area.height.saturating_sub(4),
    );
    render_workspaces(app, body, frame);
    render_footer(app, area, frame);
}

fn render_header(area: Rect, frame: &mut Frame<'_>) {
    frame.write_line(
        Rect::new(area.x, area.y, area.width, 1),
        &Line::from_spans(vec![Span::styled(
            " Bcode Reviews ",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]),
    );
    frame.write_line(
        Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
        &Line::from_spans(vec![Span::styled(
            " enter open   n new   / search   r rename   x archive   j/k move   q exit ",
            Style::new().fg(Color::BrightBlack).bg(Color::Black),
        )]),
    );
}

fn render_workspaces(app: &ReviewHomeApp, area: Rect, frame: &mut Frame<'_>) {
    let visible = app.visible_indices();
    if app.workspaces.is_empty() {
        frame.write_line(
            Rect::new(area.x, area.y, area.width, 1),
            &Line::from_spans(vec![Span::styled(
                " No review workspaces yet. Press n to create one.",
                Style::new().fg(Color::White).bg(Color::Black),
            )]),
        );
        return;
    }
    if visible.is_empty() {
        frame.write_line(
            Rect::new(area.x, area.y, area.width, 1),
            &Line::from_spans(vec![Span::styled(
                " No matching review workspaces.",
                Style::new().fg(Color::White).bg(Color::Black),
            )]),
        );
        return;
    }
    for (row, workspace_index) in visible
        .into_iter()
        .take(usize::from(area.height))
        .enumerate()
    {
        let Some(workspace) = app.workspaces.get(workspace_index) else {
            continue;
        };
        let selected = row == app.selected;
        let style = if selected {
            Style::new().fg(Color::Black).bg(Color::Yellow)
        } else {
            Style::new().fg(Color::White).bg(Color::Black)
        };
        let source_count = workspace
            .sources
            .iter()
            .filter(|source| source.included)
            .count();
        let text = format!(" {}  {} source(s)", workspace.title, source_count);
        frame.write_line(
            Rect::new(
                area.x,
                area.y
                    .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
                area.width,
                1,
            ),
            &Line::from_spans(vec![Span::styled(text, style)]),
        );
    }
}

fn render_footer(app: &ReviewHomeApp, area: Rect, frame: &mut Frame<'_>) {
    let text = app.rename_buffer.as_ref().map_or_else(
        || {
            if app.search_active || !app.search_query.is_empty() {
                format!("search: {}", app.search_query)
            } else {
                app.status_message.clone().unwrap_or_else(|| {
                    "review home: enter open, n new, / search, r rename, x archive".to_string()
                })
            }
        },
        |rename| format!("rename: {rename}"),
    );
    frame.write_line(
        Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
        &Line::from_spans(vec![Span::styled(
            text.as_str(),
            Style::new().fg(Color::White).bg(Color::BrightBlack),
        )]),
    );
}
