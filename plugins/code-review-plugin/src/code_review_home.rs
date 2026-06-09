//! Code review workspace home/picker TUI.

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use bcode_client::BcodeClient;
use bcode_code_review_models::{
    ArchiveReviewWorkspaceRequest, ArchiveReviewWorkspaceResponse,
    CODE_REVIEW_SERVICE_INTERFACE_ID, CreateReviewWorkspaceRequest, CreateReviewWorkspaceResponse,
    ListReviewWorkspacesRequest, ListReviewWorkspacesResponse, OP_REVIEW_WORKSPACE_ARCHIVE,
    OP_REVIEW_WORKSPACE_CREATE, OP_REVIEW_WORKSPACE_LIST, OP_REVIEW_WORKSPACE_UPDATE, ReviewSource,
    ReviewSourceKind, ReviewWorkspace, ReviewWorkspaceListItem, UpdateReviewWorkspaceRequest,
    UpdateReviewWorkspaceResponse,
};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui::terminal::Terminal;

use crate::terminal_events::TuiInput;
use crate::tui_host_types::{TuiError, helpers};

/// Review home outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewHomeOutcome {
    /// Open an existing or newly created review target.
    OpenWorkspace {
        /// Review workspace to open.
        workspace: ReviewWorkspace,
        /// Whether to open directly in build/source-composition mode.
        build_mode: bool,
    },
    /// Exit without opening a review.
    Exit,
}

#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
struct ReviewHomeApp {
    repo_path: PathBuf,
    workspace_items: Vec<ReviewWorkspaceListItem>,
    selected: usize,
    status_message: Option<String>,
    search_query: String,
    search_active: bool,
    rename_buffer: Option<String>,
    new_review_buffer: Option<String>,
    include_archived: bool,
    details_visible: bool,
    draft_filter_active: bool,
    help_visible: bool,
    should_exit: bool,
    outcome: Option<ReviewHomeOutcome>,
}

impl ReviewHomeApp {
    const fn new(repo_path: PathBuf, workspace_items: Vec<ReviewWorkspaceListItem>) -> Self {
        Self {
            repo_path,
            workspace_items,
            selected: 0,
            status_message: None,
            search_query: String::new(),
            search_active: false,
            rename_buffer: None,
            new_review_buffer: None,
            include_archived: false,
            details_visible: true,
            draft_filter_active: false,
            help_visible: false,
            should_exit: false,
            outcome: None,
        }
    }

    fn workspace_item(&self, index: usize) -> Option<&ReviewWorkspaceListItem> {
        self.workspace_items.get(index)
    }

    fn workspace(&self, index: usize) -> Option<&ReviewWorkspace> {
        self.workspace_item(index).map(|item| &item.workspace)
    }

    fn visible_indices(&self) -> Vec<usize> {
        let query = self.search_query.trim().to_ascii_lowercase();
        self.workspace_items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                if self.draft_filter_active && item.draft_count == 0 {
                    return None;
                }
                if query.is_empty() || workspace_item_matches_query(item, &query) {
                    Some(index)
                } else {
                    None
                }
            })
            .collect()
    }

    fn visible_selection_bounds(&self, height: usize) -> (usize, usize) {
        let visible = self.visible_indices();
        if visible.is_empty() || height == 0 {
            return (0, 0);
        }
        let first = self.selected.saturating_add(1).saturating_sub(height);
        let last = first.saturating_add(height).min(visible.len());
        (first, last)
    }

    fn selected_workspace_index(&self) -> Option<usize> {
        self.visible_indices().get(self.selected).copied()
    }

    fn clamp_selection(&mut self) {
        self.selected = self
            .selected
            .min(self.visible_indices().len().saturating_sub(1));
    }

    fn selected_workspace(&self) -> Option<ReviewWorkspace> {
        self.selected_workspace_index()
            .and_then(|index| self.workspace(index).cloned())
    }

    fn selected_workspace_or_status(&mut self) -> Option<ReviewWorkspace> {
        let Some(workspace) = self.selected_workspace() else {
            self.status_message =
                Some("no matching review workspace selected; press n to create one".to_string());
            return None;
        };
        Some(workspace)
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

    fn open_workspace(&mut self, workspace: ReviewWorkspace, build_mode: bool) -> bool {
        self.outcome = Some(ReviewHomeOutcome::OpenWorkspace {
            workspace,
            build_mode,
        });
        self.should_exit = true;
        true
    }

    fn open_workspace_unless_archived(
        &mut self,
        workspace: ReviewWorkspace,
        build_mode: bool,
    ) -> bool {
        if workspace.archived_at_ms.is_some() {
            self.status_message =
                Some("review is archived; press R to restore it first".to_string());
            return true;
        }
        self.open_workspace(workspace, build_mode)
    }

    fn open_most_recent(&mut self) -> bool {
        let Some(index) = self
            .visible_indices()
            .into_iter()
            .find(|index| {
                self.workspace(*index)
                    .is_some_and(|workspace| workspace.archived_at_ms.is_none())
            })
            .or_else(|| self.visible_indices().first().copied())
        else {
            self.status_message =
                Some("no matching review workspace selected; press n to create one".to_string());
            return true;
        };
        let Some(workspace) = self.workspace(index).cloned() else {
            return true;
        };
        let build_mode = workspace_should_open_in_build_mode(&workspace);
        self.open_workspace(workspace, build_mode)
    }

    fn open_selected(&mut self) -> bool {
        let Some(workspace) = self.selected_workspace_or_status() else {
            return true;
        };
        let build_mode = workspace_should_open_in_build_mode(&workspace);
        self.open_workspace_unless_archived(workspace, build_mode)
    }

    fn open_selected_in_build_mode(&mut self) -> bool {
        let Some(workspace) = self.selected_workspace_or_status() else {
            return true;
        };
        self.open_workspace_unless_archived(workspace, true)
    }

    fn open_selected_in_review_mode(&mut self) -> bool {
        let Some(workspace) = self.selected_workspace_or_status() else {
            return true;
        };
        self.open_workspace_unless_archived(workspace, false)
    }

    fn select_review_by_health(&mut self, target: &str, forward: bool) -> bool {
        let visible = self.visible_indices();
        if visible.is_empty() {
            self.status_message = Some("no matching review workspaces".to_string());
            return true;
        }
        let step = if forward {
            1
        } else {
            visible.len().saturating_sub(1)
        };
        let mut selected = self.selected % visible.len();
        for _ in 0..visible.len() {
            selected = selected.saturating_add(step) % visible.len();
            let workspace_index = visible[selected];
            if self
                .workspace_item(workspace_index)
                .is_some_and(|item| workspace_health_label(item) == target)
            {
                self.selected = selected;
                self.status_message = Some(format!("selected {target} review"));
                return true;
            }
        }
        self.status_message = Some(format!("no {target} reviews in the current filter"));
        true
    }

    fn select_next_setup_review(&mut self) -> bool {
        self.select_review_by_health("setup", true)
    }

    fn select_previous_setup_review(&mut self) -> bool {
        self.select_review_by_health("setup", false)
    }

    fn select_next_draft_review(&mut self) -> bool {
        self.select_review_by_health("drafts", true)
    }

    fn select_previous_draft_review(&mut self) -> bool {
        self.select_review_by_health("drafts", false)
    }

    fn select_next_published_review(&mut self) -> bool {
        self.select_review_by_health("published", true)
    }

    fn select_previous_published_review(&mut self) -> bool {
        self.select_review_by_health("published", false)
    }

    fn start_new_review(&mut self) -> bool {
        self.new_review_buffer = Some(String::new());
        self.status_message = Some("new review title; enter uses Untitled review".to_string());
        true
    }

    fn cancel_new_review(&mut self) -> bool {
        if self.new_review_buffer.is_none() {
            return false;
        }
        self.new_review_buffer = None;
        self.status_message = Some("new review canceled".to_string());
        true
    }

    fn handle_new_review_key(&mut self, key: KeyCode) -> ReviewHomeKeyOutcome {
        match key {
            KeyCode::Char(ch) => {
                self.new_review_buffer
                    .as_mut()
                    .map_or(ReviewHomeKeyOutcome::Ignored, |buffer| {
                        buffer.push(ch);
                        ReviewHomeKeyOutcome::Redraw
                    })
            }
            KeyCode::Backspace => {
                self.new_review_buffer
                    .as_mut()
                    .map_or(ReviewHomeKeyOutcome::Ignored, |buffer| {
                        buffer.pop();
                        ReviewHomeKeyOutcome::Redraw
                    })
            }
            KeyCode::Enter => ReviewHomeKeyOutcome::SubmitNewReview,
            KeyCode::Escape => {
                self.cancel_new_review();
                ReviewHomeKeyOutcome::Redraw
            }
            _ => ReviewHomeKeyOutcome::Ignored,
        }
    }

    fn start_rename(&mut self) -> bool {
        let Some(workspace_index) = self.selected_workspace_index() else {
            self.status_message = Some("no matching review workspace selected".to_string());
            return true;
        };
        let Some(workspace) = self.workspace(workspace_index) else {
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

    fn toggle_draft_filter(&mut self) -> bool {
        self.draft_filter_active = !self.draft_filter_active;
        self.clamp_selection();
        self.status_message = Some(if self.draft_filter_active {
            "showing reviews with drafts".to_string()
        } else {
            "showing all matching reviews".to_string()
        });
        true
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
    SubmitNewReview,
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
            needs_redraw = handle_key_event(&client, &mut app, key).await;
        }
    }

    Ok(app.outcome.unwrap_or(ReviewHomeOutcome::Exit))
}

async fn handle_key_event(
    client: &BcodeClient,
    app: &mut ReviewHomeApp,
    stroke: KeyStroke,
) -> bool {
    if stroke.modifiers.ctrl
        || stroke.modifiers.alt
        || stroke.modifiers.super_key
        || stroke.modifiers.hyper
        || stroke.modifiers.meta
    {
        return false;
    }
    let key = normalized_home_key(stroke);
    if app.new_review_buffer.is_some() {
        return handle_new_review_key(client, app, key).await;
    }
    if app.rename_buffer.is_some() {
        return handle_rename_key(client, app, key).await;
    }
    if app.search_active {
        return app.handle_search_key(key);
    }
    handle_normal_key(client, app, key).await
}

const fn normalized_home_key(stroke: KeyStroke) -> KeyCode {
    if !stroke.modifiers.shift {
        return stroke.key;
    }
    match stroke.key {
        KeyCode::Char('/') => KeyCode::Char('?'),
        KeyCode::Char(ch) if ch.is_ascii_lowercase() => KeyCode::Char(ch.to_ascii_uppercase()),
        key => key,
    }
}

async fn handle_new_review_key(
    client: &BcodeClient,
    app: &mut ReviewHomeApp,
    key: KeyCode,
) -> bool {
    match app.handle_new_review_key(key) {
        ReviewHomeKeyOutcome::Redraw => true,
        ReviewHomeKeyOutcome::SubmitNewReview => match submit_new_review(client, app).await {
            Ok(redraw) => redraw,
            Err(error) => {
                app.status_message = Some(format!("failed to create workspace: {error}"));
                true
            }
        },
        ReviewHomeKeyOutcome::SubmitRename | ReviewHomeKeyOutcome::Ignored => false,
    }
}

async fn handle_rename_key(client: &BcodeClient, app: &mut ReviewHomeApp, key: KeyCode) -> bool {
    match app.handle_rename_key(key) {
        ReviewHomeKeyOutcome::Redraw => true,
        ReviewHomeKeyOutcome::SubmitRename => match submit_rename_workspace(client, app).await {
            Ok(redraw) => redraw,
            Err(error) => {
                app.status_message = Some(format!("failed to rename workspace: {error}"));
                true
            }
        },
        ReviewHomeKeyOutcome::SubmitNewReview | ReviewHomeKeyOutcome::Ignored => false,
    }
}

async fn handle_normal_key(client: &BcodeClient, app: &mut ReviewHomeApp, key: KeyCode) -> bool {
    match key {
        KeyCode::Char('q') | KeyCode::Escape => {
            app.outcome = Some(ReviewHomeOutcome::Exit);
            app.should_exit = true;
            true
        }
        KeyCode::Char('/') => app.toggle_search(),
        KeyCode::Char('r') => app.start_rename(),
        KeyCode::Char('j') | KeyCode::Down => app.move_down(),
        KeyCode::Char('k') | KeyCode::Up => app.move_up(),
        KeyCode::Char('d') => {
            app.details_visible = !app.details_visible;
            true
        }
        KeyCode::Char('D') => app.toggle_draft_filter(),
        KeyCode::Char('?') => {
            app.help_visible = !app.help_visible;
            true
        }
        KeyCode::Enter => app.open_selected(),
        KeyCode::Char('c') => app.open_most_recent(),
        KeyCode::Char('o') => app.open_selected_in_review_mode(),
        KeyCode::Char('b') => app.open_selected_in_build_mode(),
        KeyCode::Char('x') => match archive_selected_workspace(client, app).await {
            Ok(archived) => archived,
            Err(error) => {
                app.status_message = Some(format!("failed to archive workspace: {error}"));
                true
            }
        },
        KeyCode::Char('n' | 'e') => app.start_new_review(),
        KeyCode::Char('u' | 's' | 'w' | 'l' | 'v') => {
            create_and_open_preset_for_key(client, app, key).await
        }
        KeyCode::Char('S') => app.select_next_setup_review(),
        KeyCode::Char('U') => app.select_previous_setup_review(),
        KeyCode::Char('F') => app.select_next_draft_review(),
        KeyCode::Char('B') => app.select_previous_draft_review(),
        KeyCode::Char('p') => app.select_next_published_review(),
        KeyCode::Char('P') => app.select_previous_published_review(),
        KeyCode::Char('a') => toggle_archived(client, app).await,
        KeyCode::Char('g') => {
            app.selected = 0;
            true
        }
        KeyCode::Char('G') => {
            app.selected = app.visible_indices().len().saturating_sub(1);
            true
        }
        KeyCode::Char('R') => match restore_selected_workspace(client, app).await {
            Ok(restored) => restored,
            Err(error) => {
                app.status_message = Some(format!("failed to restore workspace: {error}"));
                true
            }
        },
        _ => false,
    }
}

async fn create_and_open_preset_for_key(
    client: &BcodeClient,
    app: &mut ReviewHomeApp,
    key: KeyCode,
) -> bool {
    let KeyCode::Char(ch) = key else {
        return false;
    };
    let Some((title, source_kind)) = (match ch {
        'u' => Some(("Unstaged changes", ReviewSourceKind::WorkingTreeUnstaged)),
        's' => Some(("Staged changes", ReviewSourceKind::IndexStaged)),
        'w' => Some(("Working tree review", ReviewSourceKind::WorkingTreeAndIndex)),
        'l' => Some(("Last commit review", ReviewSourceKind::LastCommit)),
        'v' => Some(("Repository browser review", ReviewSourceKind::Repository)),
        _ => None,
    }) else {
        return false;
    };
    create_and_open_preset_workspace(client, app, title, vec![source_kind]).await
}

async fn toggle_archived(client: &BcodeClient, app: &mut ReviewHomeApp) -> bool {
    app.include_archived = !app.include_archived;
    match load_workspaces_with_archived(client, app.repo_path.clone(), app.include_archived).await {
        Ok(workspaces) => {
            app.workspace_items = workspaces;
            app.clamp_selection();
            app.status_message = Some(if app.include_archived {
                "showing archived reviews".to_string()
            } else {
                "hiding archived reviews".to_string()
            });
        }
        Err(error) => {
            app.include_archived = !app.include_archived;
            app.status_message = Some(format!("failed to toggle archived reviews: {error}"));
        }
    }
    true
}

async fn load_workspaces(
    client: &BcodeClient,
    repo_path: PathBuf,
) -> Result<Vec<ReviewWorkspaceListItem>, TuiError> {
    load_workspaces_with_archived(client, repo_path, false).await
}

async fn load_workspaces_with_archived(
    client: &BcodeClient,
    repo_path: PathBuf,
    include_archived: bool,
) -> Result<Vec<ReviewWorkspaceListItem>, TuiError> {
    let payload = serde_json::to_vec(&ListReviewWorkspacesRequest {
        repo_path,
        include_archived,
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
    let mut items = if response.items.is_empty() {
        response
            .workspaces
            .into_iter()
            .map(|workspace| ReviewWorkspaceListItem {
                workspace,
                thread_count: 0,
                draft_count: 0,
                last_publish: None,
            })
            .collect()
    } else {
        response.items
    };
    sort_workspace_items_recent_first(&mut items);
    Ok(items)
}

async fn create_and_open_preset_workspace(
    client: &BcodeClient,
    app: &mut ReviewHomeApp,
    title: &str,
    source_kinds: Vec<ReviewSourceKind>,
) -> bool {
    match create_workspace_with_sources(
        client,
        app.repo_path.clone(),
        title.to_string(),
        source_kinds,
    )
    .await
    {
        Ok(workspace) => {
            app.workspace_items.push(ReviewWorkspaceListItem {
                workspace: workspace.clone(),
                thread_count: 0,
                draft_count: 0,
                last_publish: None,
            });
            sort_workspace_items_recent_first(&mut app.workspace_items);
            app.open_workspace(workspace, false)
        }
        Err(error) => {
            app.status_message = Some(format!("failed to create workspace: {error}"));
            true
        }
    }
}

async fn submit_new_review(
    client: &BcodeClient,
    app: &mut ReviewHomeApp,
) -> Result<bool, TuiError> {
    let title = app
        .new_review_buffer
        .take()
        .unwrap_or_else(|| "Untitled review".to_string())
        .trim()
        .to_string();
    let title = if title.is_empty() {
        "Untitled review".to_string()
    } else {
        title
    };
    let workspace =
        create_workspace_with_sources(client, app.repo_path.clone(), title, Vec::new()).await?;
    app.open_workspace(workspace, true);
    Ok(true)
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
    let Some(existing) = app.workspace(workspace_index) else {
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
    app.workspace_items[workspace_index].workspace = response.workspace;
    app.status_message = Some(format!("renamed review to {new_title}"));
    Ok(true)
}

async fn restore_selected_workspace(
    client: &BcodeClient,
    app: &mut ReviewHomeApp,
) -> Result<bool, TuiError> {
    let Some(workspace_index) = app.selected_workspace_index() else {
        app.status_message = Some("no matching review workspace selected".to_string());
        return Ok(true);
    };
    let Some(existing) = app.workspace(workspace_index) else {
        app.status_message = Some("no review workspace selected".to_string());
        return Ok(true);
    };
    if existing.archived_at_ms.is_none() {
        app.status_message = Some("selected review is not archived".to_string());
        return Ok(true);
    }
    let mut workspace = existing.clone();
    workspace.archived_at_ms = None;
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
    app.workspace_items[workspace_index].workspace = response.workspace;
    app.status_message = Some("restored review workspace".to_string());
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
    let Some(workspace) = app.workspace(workspace_index) else {
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
        let archived = app.workspace_items.remove(workspace_index);
        app.clamp_selection();
        app.status_message = Some(format!("archived {}", archived.workspace.title));
    } else {
        app.status_message = Some("workspace was not found".to_string());
    }
    Ok(true)
}

async fn create_workspace_with_sources(
    client: &BcodeClient,
    repo_path: PathBuf,
    title: String,
    source_kinds: Vec<ReviewSourceKind>,
) -> Result<ReviewWorkspace, TuiError> {
    let payload = serde_json::to_vec(&CreateReviewWorkspaceRequest {
        repo_path,
        title: Some(title),
        sources: source_kinds
            .into_iter()
            .enumerate()
            .map(|(index, kind)| ReviewSource {
                id: format!("source-{}", index.saturating_add(1)),
                label: kind.label(),
                kind,
                included: true,
            })
            .collect(),
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
    render_header(app, area, frame);
    let body = Rect::new(
        area.x,
        area.y.saturating_add(2),
        area.width,
        area.height.saturating_sub(4),
    );
    render_workspaces(app, body, frame);
    render_footer(app, area, frame);
    if app.help_visible {
        render_help(area, frame);
    }
}

fn render_header(app: &ReviewHomeApp, area: Rect, frame: &mut Frame<'_>) {
    frame.write_line(
        Rect::new(area.x, area.y, area.width, 1),
        &Line::from_spans(vec![Span::styled(
            format!(" Bcode Reviews  {} ", review_home_summary_label(app)),
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]),
    );
    let filter_label = active_filter_label(app);
    frame.write_line(
        Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
        &Line::from_spans(vec![Span::styled(
            format!(
                " {filter_label}  enter open   c latest   n new   u/s/w/l/v presets   S setup   F drafts   / search   ? help "
            ),
            Style::new().fg(Color::BrightBlack).bg(Color::Black),
        )]),
    );
}

fn review_home_summary_label(app: &ReviewHomeApp) -> String {
    let visible_count = app.visible_indices().len();
    let active_count = app
        .workspace_items
        .iter()
        .filter(|item| item.workspace.archived_at_ms.is_none())
        .count();
    let draft_count = app
        .workspace_items
        .iter()
        .filter(|item| item.draft_count != 0)
        .count();
    let setup_count = app
        .workspace_items
        .iter()
        .filter(|item| workspace_health_label(item) == "setup")
        .count();
    let published_count = app
        .workspace_items
        .iter()
        .filter(|item| workspace_health_label(item) == "published")
        .count();
    format!(
        "{visible_count} visible · {active_count} active · {draft_count} drafts · {published_count} published · {setup_count} setup"
    )
}

fn active_filter_label(app: &ReviewHomeApp) -> String {
    let archived = if app.include_archived {
        "archived:on"
    } else {
        "archived:off"
    };
    let drafts = if app.draft_filter_active {
        "drafts:on"
    } else {
        "drafts:all"
    };
    let search = if app.search_query.trim().is_empty() {
        "search:off".to_string()
    } else {
        format!("search:{}", app.search_query.trim())
    };
    format!("[{archived} {drafts} {search}]")
}

const fn review_home_help_lines() -> &'static [&'static str] {
    &[
        " Review Picker Help",
        "",
        " enter               open selected review; setup opens in build mode",
        " c                   continue latest visible active review",
        " o                   force-open selected review in review mode",
        " build mode: attach sources, fix diagnostics, then m to review",
        " n                   new empty review: name it, then add sources",
        " u/s/w/l/v           quick-create unstaged/staged/worktree/last/repo",
        " S/U                 next/previous setup review",
        " F/B                 next/previous draft review",
        " p/P                 next/previous published review",
        " j/k or arrows       move selection",
        " /                   search title, health, source, branch, commit, publish, id",
        " a                   show/hide archived reviews",
        " D                   show only reviews with drafts",
        " d                   show/hide details pane",
        " r                   rename selected review",
        " x                   archive selected review",
        " archived             press R to restore before opening",
        " R                   restore selected archived review",
        " q or esc            exit",
    ]
}

fn render_workspaces(app: &ReviewHomeApp, area: Rect, frame: &mut Frame<'_>) {
    if app.details_visible && area.width >= 80 {
        let details_width = (area.width / 3).clamp(28, 48);
        let list_width = area.width.saturating_sub(details_width).saturating_sub(1);
        let list_area = Rect::new(area.x, area.y, list_width, area.height);
        let details_area = Rect::new(
            area.x.saturating_add(list_width).saturating_add(1),
            area.y,
            details_width,
            area.height,
        );
        render_workspace_list(app, list_area, frame);
        render_workspace_details(app, details_area, frame);
    } else {
        render_workspace_list(app, area, frame);
    }
}

fn render_empty_review_home(app: &ReviewHomeApp, area: Rect, frame: &mut Frame<'_>) {
    let lines = [
        "No review workspaces yet.",
        "",
        "Start fast:",
        "  u  unstaged changes",
        "  s  staged changes",
        "  w  working tree review",
        "  v  repository browser review",
        "",
        "Or press n to name a custom review, then attach sources in build mode.",
    ];
    for (row, line) in lines.iter().take(usize::from(area.height)).enumerate() {
        let style = if row == 0 {
            Style::new()
                .fg(Color::Cyan)
                .bg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::White).bg(Color::Black)
        };
        frame.write_line(
            Rect::new(
                area.x,
                area.y
                    .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
                area.width,
                1,
            ),
            &Line::from_spans(vec![Span::styled(format!(" {line}"), style)]),
        );
    }
    if let Some(message) = &app.status_message {
        let y = area
            .y
            .saturating_add(u16::try_from(lines.len()).unwrap_or(u16::MAX))
            .saturating_add(1);
        if y < area.bottom() {
            frame.write_line(
                Rect::new(area.x, y, area.width, 1),
                &Line::from_spans(vec![Span::styled(
                    format!(" {message}"),
                    Style::new().fg(Color::Yellow).bg(Color::Black),
                )]),
            );
        }
    }
}

fn render_workspace_list(app: &ReviewHomeApp, area: Rect, frame: &mut Frame<'_>) {
    let visible = app.visible_indices();
    if app.workspace_items.is_empty() {
        render_empty_review_home(app, area, frame);
        return;
    }
    if visible.is_empty() {
        frame.write_line(
            Rect::new(area.x, area.y, area.width, 1),
            &Line::from_spans(vec![Span::styled(
                if app.draft_filter_active {
                    " No review workspaces with drafts match the active filters."
                } else {
                    " No matching review workspaces."
                },
                Style::new().fg(Color::White).bg(Color::Black),
            )]),
        );
        return;
    }
    let visible_height = usize::from(area.height);
    let (first_visible_row, last_visible_row) = app.visible_selection_bounds(visible_height);
    for (row, workspace_index) in visible
        .into_iter()
        .skip(first_visible_row)
        .take(last_visible_row.saturating_sub(first_visible_row))
        .enumerate()
    {
        let Some(item) = app.workspace_item(workspace_index) else {
            continue;
        };
        let workspace = &item.workspace;
        let selected = first_visible_row.saturating_add(row) == app.selected;
        let style = if selected {
            Style::new().fg(Color::Black).bg(Color::Yellow)
        } else if workspace.archived_at_ms.is_some() {
            Style::new().fg(Color::BrightBlack).bg(Color::Black)
        } else {
            Style::new().fg(Color::White).bg(Color::Black)
        };
        let status = workspace_health_label(item);
        let text = format!("{status:8}  {}", workspace_row_text(item));
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

fn render_workspace_details(app: &ReviewHomeApp, area: Rect, frame: &mut Frame<'_>) {
    let Some(index) = app.selected_workspace_index() else {
        frame.write_line(
            Rect::new(area.x, area.y, area.width, 1),
            &Line::from_spans(vec![Span::styled(
                " No review selected",
                Style::new().fg(Color::BrightBlack),
            )]),
        );
        return;
    };
    let Some(item) = app.workspace_item(index) else {
        return;
    };
    let workspace = &item.workspace;
    let mut lines = vec![
        format!(" {}", workspace.title),
        format!(" health: {}", workspace_health_label(item)),
        format!(" id: {}", workspace.id),
        format!(" repo: {}", workspace.repo_root.display()),
        format!(
            " updated: {}",
            workspace
                .updated_at_ms
                .or(workspace.created_at_ms)
                .map_or_else(|| "unknown".to_string(), relative_time_label)
        ),
    ];
    if let Some(archived_at_ms) = workspace.archived_at_ms {
        lines.push(format!(
            " archived: {}",
            relative_time_label(archived_at_ms)
        ));
    }
    lines.push(format!(
        " drafts: {} comment(s) across {} thread(s)",
        item.draft_count, item.thread_count
    ));
    if let Some(record) = &item.last_publish {
        lines.push(format!(
            " published: {} via {}",
            relative_time_label(record.created_at_ms),
            record.publisher_id
        ));
        lines.push(format!(
            " output: {}",
            record.output.as_deref().unwrap_or("none")
        ));
        lines.push(format!(" result: {}", record.message));
    } else {
        lines.push(" published: never".to_string());
    }
    lines.push(format!(
        " sources: {}/{} included",
        workspace
            .sources
            .iter()
            .filter(|source| source.included)
            .count(),
        workspace.sources.len()
    ));
    lines.push(format!(" next: {}", workspace_next_action(item)));
    lines.push(String::new());
    for source in workspace
        .sources
        .iter()
        .take(usize::from(area.height).saturating_sub(8))
    {
        let included = if source.included { "✓" } else { " " };
        lines.push(format!(
            " [{included}] {} — {}",
            source_kind_label(&source.kind),
            source.label
        ));
    }
    for (row, line) in lines.iter().take(usize::from(area.height)).enumerate() {
        frame.write_line(
            Rect::new(
                area.x,
                area.y
                    .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
                area.width,
                1,
            ),
            &Line::from_spans(vec![Span::styled(
                line.clone(),
                if row == 0 {
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(Color::BrightBlack)
                },
            )]),
        );
    }
}

const fn source_kind_label(kind: &ReviewSourceKind) -> &'static str {
    match kind {
        ReviewSourceKind::WorkingTreeUnstaged => "unstaged",
        ReviewSourceKind::IndexStaged => "staged",
        ReviewSourceKind::WorkingTreeAndIndex => "worktree",
        ReviewSourceKind::LastCommit => "last commit",
        ReviewSourceKind::Commit { .. } => "commit",
        ReviewSourceKind::CommitRange { .. } => "range",
        ReviewSourceKind::BranchCompare { .. } => "branch",
        ReviewSourceKind::File { .. } => "file",
        ReviewSourceKind::FileRange { .. } => "file range",
        ReviewSourceKind::Repository => "repository",
    }
}

fn sort_workspace_items_recent_first(items: &mut [ReviewWorkspaceListItem]) {
    items.sort_by(|left, right| {
        workspace_sort_timestamp(&right.workspace)
            .cmp(&workspace_sort_timestamp(&left.workspace))
            .then_with(|| left.workspace.title.cmp(&right.workspace.title))
    });
}

fn workspace_sort_timestamp(workspace: &ReviewWorkspace) -> u64 {
    workspace
        .updated_at_ms
        .or(workspace.created_at_ms)
        .unwrap_or(0)
}

fn workspace_item_matches_query(item: &ReviewWorkspaceListItem, query: &str) -> bool {
    item.thread_count.to_string().contains(query)
        || item.draft_count.to_string().contains(query)
        || workspace_health_label(item).contains(query)
        || workspace_next_action(item).contains(query)
        || item.last_publish.as_ref().is_some_and(|publish| {
            publish.publisher_id.to_ascii_lowercase().contains(query)
                || publish.review_id.to_ascii_lowercase().contains(query)
                || publish
                    .workspace_id
                    .as_deref()
                    .is_some_and(|workspace_id| workspace_id.to_ascii_lowercase().contains(query))
                || publish
                    .output
                    .as_deref()
                    .is_some_and(|output| output.to_ascii_lowercase().contains(query))
                || publish.message.to_ascii_lowercase().contains(query)
        })
        || workspace_matches_query(&item.workspace, query)
}

fn workspace_matches_query(workspace: &ReviewWorkspace, query: &str) -> bool {
    workspace.title.to_ascii_lowercase().contains(query)
        || workspace.id.to_ascii_lowercase().contains(query)
        || workspace
            .repo_root
            .display()
            .to_string()
            .to_ascii_lowercase()
            .contains(query)
        || workspace
            .archived_at_ms
            .is_some_and(|archived_at_ms| archived_at_ms.to_string().contains(query))
        || workspace.sources.iter().any(|source| {
            source.label.to_ascii_lowercase().contains(query)
                || source.id.to_ascii_lowercase().contains(query)
                || source_kind_search_text(&source.kind).contains(query)
        })
}

fn source_kind_search_text(kind: &ReviewSourceKind) -> String {
    match kind {
        ReviewSourceKind::WorkingTreeUnstaged => "unstaged working tree".to_string(),
        ReviewSourceKind::IndexStaged => "staged index".to_string(),
        ReviewSourceKind::WorkingTreeAndIndex => "working tree staged unstaged".to_string(),
        ReviewSourceKind::LastCommit => "last commit".to_string(),
        ReviewSourceKind::Commit { rev } => format!("commit {rev}").to_ascii_lowercase(),
        ReviewSourceKind::CommitRange {
            base,
            head,
            merge_base,
        } => format!("range {base} {head} merge-base {merge_base}").to_ascii_lowercase(),
        ReviewSourceKind::BranchCompare {
            base_branch,
            head_branch,
            merge_base,
        } => format!("branch compare {base_branch} {head_branch} merge-base {merge_base}")
            .to_ascii_lowercase(),
        ReviewSourceKind::File { path } => format!("file {path}").to_ascii_lowercase(),
        ReviewSourceKind::FileRange { path, start, end } => {
            format!("file range {path} {start} {end}").to_ascii_lowercase()
        }
        ReviewSourceKind::Repository => "repository".to_string(),
    }
}

fn workspace_should_open_in_build_mode(workspace: &ReviewWorkspace) -> bool {
    workspace.sources.is_empty() || workspace.sources.iter().all(|source| !source.included)
}

fn workspace_health_label(item: &ReviewWorkspaceListItem) -> &'static str {
    let workspace = &item.workspace;
    if workspace.archived_at_ms.is_some() {
        "archived"
    } else if workspace.sources.is_empty() {
        "setup"
    } else if workspace.sources.iter().all(|source| !source.included) {
        "needs sources"
    } else if item.draft_count > 0 && item.last_publish.is_none() {
        "drafts"
    } else if item.last_publish.is_some() {
        "published"
    } else {
        "active"
    }
}

fn workspace_next_action(item: &ReviewWorkspaceListItem) -> String {
    let workspace = &item.workspace;
    let included_count = workspace
        .sources
        .iter()
        .filter(|source| source.included)
        .count();
    if workspace.archived_at_ms.is_some() {
        return "restore review to continue".to_string();
    }
    if workspace.sources.is_empty() {
        return "open and add sources".to_string();
    }
    if included_count == 0 {
        return "open and include at least one source".to_string();
    }
    if item.draft_count > 0 && item.last_publish.is_none() {
        return "open and publish drafts".to_string();
    }
    if item.draft_count > 0 {
        return "open and continue draft review".to_string();
    }
    "open and continue review".to_string()
}

fn workspace_row_text(item: &ReviewWorkspaceListItem) -> String {
    let workspace = &item.workspace;
    let source_count = workspace
        .sources
        .iter()
        .filter(|source| source.included)
        .count();
    let updated = workspace
        .updated_at_ms
        .or(workspace.created_at_ms)
        .map_or_else(|| "unknown".to_string(), relative_time_label);
    let sources = workspace
        .sources
        .iter()
        .filter(|source| source.included)
        .take(3)
        .map(|source| source.label.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let suffix = if sources.is_empty() {
        format!("{source_count} source(s)")
    } else if source_count > 3 {
        format!("{source_count} source(s): {sources}, …")
    } else {
        format!("{source_count} source(s): {sources}")
    };
    let archived = if workspace.archived_at_ms.is_some() {
        "  · archived"
    } else {
        ""
    };
    let draft_suffix = if item.draft_count == 0 {
        "no drafts".to_string()
    } else {
        format!(
            "{} draft(s)/{} thread(s)",
            item.draft_count, item.thread_count
        )
    };
    let health = workspace_health_label(item);
    let next_action = workspace_next_action(item);
    format!(
        " {}  · {}  · {health}  · {}  · next: {}  · {}{}",
        workspace.title, updated, draft_suffix, next_action, suffix, archived
    )
}

fn relative_time_label(timestamp_ms: u64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        });
    let elapsed_seconds = now_ms.saturating_sub(timestamp_ms) / 1_000;
    match elapsed_seconds {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{}m ago", elapsed_seconds / 60),
        3_600..=86_399 => format!("{}h ago", elapsed_seconds / 3_600),
        86_400..=604_799 => format!("{}d ago", elapsed_seconds / 86_400),
        _ => format!("{}w ago", elapsed_seconds / 604_800),
    }
}

fn render_help(area: Rect, frame: &mut Frame<'_>) {
    let lines = review_home_help_lines();
    let width = area.width.min(72);
    let height = area.height.min(
        u16::try_from(lines.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2),
    );
    if width < 24 || height < 4 {
        return;
    }
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(height) / 2);
    let popup = Rect::new(x, y, width, height);
    frame.fill(
        popup,
        " ",
        Style::new().fg(Color::White).bg(Color::BrightBlack),
    );
    for (index, text) in lines.iter().enumerate() {
        let y = popup
            .y
            .saturating_add(1)
            .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
        if y >= popup.bottom() {
            break;
        }
        frame.write_line(
            Rect::new(
                popup.x.saturating_add(1),
                y,
                popup.width.saturating_sub(2),
                1,
            ),
            &Line::from_spans(vec![Span::styled(
                text.to_string(),
                Style::new().fg(Color::White).bg(Color::BrightBlack),
            )]),
        );
    }
}

fn render_footer(app: &ReviewHomeApp, area: Rect, frame: &mut Frame<'_>) {
    let text = app.new_review_buffer.as_ref().map_or_else(
        || {
            app.rename_buffer.as_ref().map_or_else(
                || {
                    if app.search_active || !app.search_query.is_empty() {
                        format!("search: {}", app.search_query)
                    } else if let Some(item) = app
                        .selected_workspace_index()
                        .and_then(|index| app.workspace_item(index))
                    {
                        format!(
                            "{}: {}  · enter open  c latest  b build  o review  ? help",
                            workspace_health_label(item),
                            workspace_next_action(item)
                        )
                    } else {
                        app.status_message.clone().unwrap_or_else(|| {
                            "review home: n new, u/s/w/l/v presets, / search title/health/source/publish, ? help"
                        .to_string()
                        })
                    }
                },
                |rename| format!("rename: {rename}"),
            )
        },
        |title| {
            if title.is_empty() {
                "new review title: <enter for Untitled review>".to_string()
            } else {
                format!("new review title: {title}")
            }
        },
    );
    frame.write_line(
        Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
        &Line::from_spans(vec![Span::styled(
            text.as_str(),
            Style::new().fg(Color::White).bg(Color::BrightBlack),
        )]),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::ReviewPublishRecord;

    fn workspace(id: &str, sources: Vec<ReviewSource>, archived: bool) -> ReviewWorkspace {
        ReviewWorkspace {
            id: id.to_string(),
            title: id.to_string(),
            repo_root: PathBuf::from("/repo"),
            sources,
            created_at_ms: Some(1),
            updated_at_ms: Some(2),
            archived_at_ms: archived.then_some(3),
        }
    }

    fn source(id: &str, included: bool) -> ReviewSource {
        ReviewSource {
            id: id.to_string(),
            kind: ReviewSourceKind::LastCommit,
            label: id.to_string(),
            included,
        }
    }

    fn item(workspace: ReviewWorkspace) -> ReviewWorkspaceListItem {
        ReviewWorkspaceListItem {
            workspace,
            thread_count: 0,
            draft_count: 0,
            last_publish: None,
        }
    }

    #[test]
    fn selected_archived_review_must_be_restored_before_opening() {
        let mut app = ReviewHomeApp::new(
            PathBuf::from("/repo"),
            vec![item(workspace(
                "archived",
                vec![source("source", true)],
                true,
            ))],
        );

        assert!(app.open_selected());

        assert_eq!(app.outcome, None);
        assert!(!app.should_exit);
        assert_eq!(
            app.status_message.as_deref(),
            Some("review is archived; press R to restore it first")
        );
    }

    #[test]
    fn repository_preset_opens_review_workspace() {
        let mut app = ReviewHomeApp::new(PathBuf::from("/repo"), Vec::new());
        let workspace = workspace("repo", vec![source("repo-source", true)], false);

        assert!(app.open_workspace(workspace.clone(), false));

        assert_eq!(
            app.outcome,
            Some(ReviewHomeOutcome::OpenWorkspace {
                workspace,
                build_mode: false,
            })
        );
    }

    #[test]
    fn search_matches_health_and_publish_metadata() {
        let mut published = item(workspace("review", vec![source("source", true)], false));
        published.last_publish = Some(ReviewPublishRecord {
            id: "publish-1".to_string(),
            workspace_id: Some("review".to_string()),
            review_id: "github-review".to_string(),
            publisher_id: "github".to_string(),
            submitted: true,
            output: Some("https://example.test/pr/1".to_string()),
            message: "submitted".to_string(),
            created_at_ms: 4,
        });
        let setup = item(workspace("new-review", Vec::new(), false));

        assert!(workspace_item_matches_query(&setup, "setup"));
        assert!(workspace_item_matches_query(&published, "published"));
        assert!(workspace_item_matches_query(&published, "github"));
        assert!(workspace_item_matches_query(&published, "example.test"));
        assert!(workspace_item_matches_query(&published, "submitted"));
    }

    #[test]
    fn health_labels_describe_picker_state() {
        let setup = item(workspace("setup", Vec::new(), false));
        let needs_sources = item(workspace("needs", vec![source("source", false)], false));
        let mut drafts = item(workspace("drafts", vec![source("source", true)], false));
        drafts.draft_count = 1;
        let mut published = item(workspace("published", vec![source("source", true)], false));
        published.last_publish = Some(ReviewPublishRecord {
            id: "publish-1".to_string(),
            workspace_id: Some("published".to_string()),
            review_id: "review-1".to_string(),
            publisher_id: "test".to_string(),
            submitted: false,
            output: None,
            message: "ok".to_string(),
            created_at_ms: 4,
        });
        let active = item(workspace("active", vec![source("source", true)], false));
        let archived = item(workspace("archived", vec![source("source", true)], true));

        assert_eq!(workspace_health_label(&setup), "setup");
        assert_eq!(workspace_health_label(&needs_sources), "needs sources");
        assert_eq!(workspace_health_label(&drafts), "drafts");
        assert_eq!(workspace_health_label(&published), "published");
        assert_eq!(workspace_health_label(&active), "active");
        assert_eq!(workspace_health_label(&archived), "archived");
    }

    #[test]
    fn smart_open_uses_build_mode_for_setup_reviews() {
        assert!(workspace_should_open_in_build_mode(&workspace(
            "empty",
            Vec::new(),
            false
        )));
        assert!(workspace_should_open_in_build_mode(&workspace(
            "excluded",
            vec![source("source", false)],
            false,
        )));
        assert!(!workspace_should_open_in_build_mode(&workspace(
            "included",
            vec![source("source", true)],
            false,
        )));
    }

    #[test]
    fn continue_latest_skips_archived_when_possible() {
        let mut app = ReviewHomeApp::new(
            PathBuf::from("/repo"),
            vec![
                item(workspace("archived", vec![source("source", true)], true)),
                item(workspace("active", vec![source("source", true)], false)),
            ],
        );

        assert!(app.open_most_recent());

        assert!(app.should_exit);
        assert_eq!(
            app.outcome,
            Some(ReviewHomeOutcome::OpenWorkspace {
                workspace: workspace("active", vec![source("source", true)], false),
                build_mode: false,
            })
        );
    }

    #[test]
    fn category_navigation_wraps_visible_reviews() {
        let mut draft = item(workspace("draft", vec![source("source", true)], false));
        draft.draft_count = 1;
        let mut app = ReviewHomeApp::new(
            PathBuf::from("/repo"),
            vec![
                item(workspace("setup", Vec::new(), false)),
                item(workspace("active", vec![source("source", true)], false)),
                draft,
            ],
        );
        app.selected = 2;

        assert!(app.select_next_setup_review());
        assert_eq!(app.selected, 0);
        assert!(app.select_previous_draft_review());
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn visible_selection_bounds_keep_selection_visible() {
        let app = ReviewHomeApp {
            selected: 8,
            workspace_items: (0..10)
                .map(|index| item(workspace(&format!("review-{index}"), Vec::new(), false)))
                .collect(),
            ..ReviewHomeApp::new(PathBuf::from("/repo"), Vec::new())
        };

        assert_eq!(app.visible_selection_bounds(5), (4, 9));
        assert_eq!(app.visible_selection_bounds(20), (0, 10));
    }
}
