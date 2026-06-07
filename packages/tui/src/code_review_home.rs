//! Code review workspace home/picker TUI.

use std::io::Write;
use std::path::PathBuf;

use bcode_client::BcodeClient;
use bcode_code_review_models::{
    CODE_REVIEW_SERVICE_INTERFACE_ID, CreateReviewWorkspaceRequest, CreateReviewWorkspaceResponse,
    ListReviewWorkspacesRequest, ListReviewWorkspacesResponse, OP_REVIEW_WORKSPACE_CREATE,
    OP_REVIEW_WORKSPACE_LIST, ReviewWorkspace,
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
            should_exit: false,
            outcome: None,
        }
    }

    fn move_down(&mut self) -> bool {
        let next = self
            .selected
            .saturating_add(1)
            .min(self.workspaces.len().saturating_sub(1));
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

    fn open_selected(&mut self) -> bool {
        let Some(workspace) = self.workspaces.get(self.selected) else {
            self.status_message =
                Some("no review workspace selected; press n to create one".to_string());
            return true;
        };
        self.outcome = Some(ReviewHomeOutcome::OpenWorkspace {
            workspace: workspace.clone(),
        });
        self.should_exit = true;
        true
    }

    fn create_new(&mut self) -> bool {
        self.outcome = Some(ReviewHomeOutcome::OpenWorkspace {
            workspace: ReviewWorkspace {
                id: "transient-new-review-workspace".to_string(),
                title: "Untitled review".to_string(),
                repo_root: self.repo_path.clone(),
                sources: Vec::new(),
                created_at_ms: None,
                updated_at_ms: None,
                archived_at_ms: None,
            },
        });
        self.should_exit = true;
        true
    }
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
            needs_redraw = match key.key {
                KeyCode::Char('q') | KeyCode::Escape => {
                    app.outcome = Some(ReviewHomeOutcome::Exit);
                    app.should_exit = true;
                    true
                }
                KeyCode::Char('j') | KeyCode::Down => app.move_down(),
                KeyCode::Char('k') | KeyCode::Up => app.move_up(),
                KeyCode::Enter => app.open_selected(),
                KeyCode::Char('n') => {
                    match create_workspace(&client, app.repo_path.clone()).await {
                        Ok(workspace) => {
                            app.workspaces.insert(0, workspace);
                            app.selected = 0;
                            app.status_message = Some("created review workspace".to_string());
                            app.create_new()
                        }
                        Err(error) => {
                            app.status_message =
                                Some(format!("failed to create workspace: {error}"));
                            true
                        }
                    }
                }
                _ => false,
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
            " enter open   n new   j/k move   q exit ",
            Style::new().fg(Color::BrightBlack).bg(Color::Black),
        )]),
    );
}

fn render_workspaces(app: &ReviewHomeApp, area: Rect, frame: &mut Frame<'_>) {
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
    for (row, workspace) in app
        .workspaces
        .iter()
        .take(usize::from(area.height))
        .enumerate()
    {
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
    let text = app
        .status_message
        .as_deref()
        .unwrap_or("review home: open an existing workspace or create a new one");
    frame.write_line(
        Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
        &Line::from_spans(vec![Span::styled(
            text,
            Style::new().fg(Color::White).bg(Color::BrightBlack),
        )]),
    );
}
